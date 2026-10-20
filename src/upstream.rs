//! Upstream connections to IRC servers (port of upstream's upstream.go).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use indexmap::IndexMap;
use rand::seq::IndexedRandom;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::conn::{BoxIo, Conn, RateLimit, RateLimitFlag};
use crate::database::{self as db, get_nick, get_realname, get_username, split_host_port};
use crate::downstream::{self, DownstreamConn, LabelContext};
use crate::irc::{Message, Prefix, numerics::*};
use crate::ircutil::{self, ChannelModeType, ChannelModes, UserModes};
use crate::logger::Logger;
use crate::metrics;
use crate::rate::Backoffer;
use crate::server::Server;
use crate::user::{Event, User, dc_ids_for, dcs_for};
use crate::xirc::{
    self, CapRegistry, CaseMapping, CaseMappingMap, ChannelStatus, Isupport, Membership,
    MembershipSet,
};
use crate::{debugf, logf, msg};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const UPSTREAM_MESSAGE_DELAY: Duration = Duration::from_secs(2);
pub const UPSTREAM_MESSAGE_BURST: u32 = 10;
/// The delay after each connect command, to let the server process it, e.g.
/// for NickServ to log us in.
pub const CONNECT_COMMAND_DELAY: Duration = Duration::from_secs(5);

/// The static list of upstream capabilities always requested when
/// supported.
const PERMANENT_UPSTREAM_CAPS: &[&str] = &[
    "account-notify",
    "account-tag",
    "away-notify",
    "batch",
    "chghost",
    "echo-message",
    "extended-join",
    "extended-monitor",
    "invite-notify",
    "labeled-response",
    "message-tags",
    "multi-prefix",
    "sasl",
    "server-time",
    "setname",
    "draft/account-registration",
    "draft/extended-monitor",
    "draft/message-redaction",
    "draft/multiline",
];

/// The static list of message tags that will cause a TAGMSG to be stored.
const STORABLE_MESSAGE_TAGS: &[&str] = &["+react", "+unreact", "+draft/react", "+draft/unreact"];

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// An error during connection registration.
#[derive(Debug)]
pub struct RegistrationError(pub Message);

impl RegistrationError {
    pub fn reason(&self) -> String {
        match self.0.params.last() {
            Some(p) => p.clone(),
            None => self.0.command.clone(),
        }
    }

    /// Only false if we're 100% sure that fixing the error requires a
    /// network configuration change.
    pub fn temporary(&self) -> bool {
        match self.0.command.as_str() {
            ERR_PASSWDMISMATCH | ERR_ERRONEUSNICKNAME => false,
            "FAIL" => self.0.param(1) != "ACCOUNT_REQUIRED",
            _ => true,
        }
    }
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "registration error ({}): {}",
            self.0.command,
            self.reason()
        )
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Debug, Clone)]
pub struct OutcomeError {
    /// Logged by the runner
    pub text: String,
    /// Sent to the user, prefixed with "connection error: "
    pub event: String,
    pub temporary: bool,
}

#[derive(Debug)]
struct TlsHandshakeError(String);

impl std::fmt::Display for TlsHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tls: {}", self.0)
    }
}

impl std::error::Error for TlsHandshakeError {}

/// How an upstream connection ended, reported to its runner.
#[derive(Debug)]
pub struct ConnOutcome {
    pub err: Option<OutcomeError>,
}

/// A connected (not yet registered) upstream socket, handed over by the
/// runner to the user task.
pub struct NewConn {
    pub id: u64,
    pub conn: Conn,
    pub rate_flag: RateLimitFlag,
    pub outcome_tx: oneshot::Sender<ConnOutcome>,
    pub net_id: i64,
    pub events: mpsc::UnboundedSender<Event>,
    pub srv: Arc<Server>,
}

pub struct UpstreamChannel {
    pub name: String,
    pub topic: String,
    pub topic_who: Option<Prefix>,
    pub topic_time: Option<chrono::DateTime<Utc>>,
    pub status: ChannelStatus,
    pub modes: Option<ChannelModes>,
    pub creation_time: String,
    pub members: CaseMappingMap<MembershipSet>,
    pub complete: bool,
    detach_timer: Option<AbortHandle>,
}

impl UpstreamChannel {
    /// Replaces the auto-detach timer. None just stops the current one.
    pub fn update_auto_detach(&mut self, timer: Option<AbortHandle>) {
        if let Some(t) = self.detach_timer.take() {
            t.abort();
        }
        self.detach_timer = timer;
    }
}

#[derive(Clone, Debug, Default)]
pub struct UpstreamUser {
    pub nickname: String,
    pub username: String,
    pub hostname: String,
    pub server: String,
    pub flags: String,
    pub account: String,
    pub realname: String,
}

impl UpstreamUser {
    fn has_whox_fields(&self, fields: &str) -> bool {
        fields.bytes().all(|f| match f {
            b'n' => !self.nickname.is_empty(),
            b'u' => !self.username.is_empty(),
            b'h' => !self.hostname.is_empty(),
            b's' => !self.server.is_empty(),
            b'f' => !self.flags.is_empty(),
            b'a' => !self.account.is_empty(),
            b'r' => !self.realname.is_empty(),
            // we return static values for those fields, so they are always
            // available
            b't' | b'c' | b'i' | b'd' | b'l' | b'o' => true,
            _ => false,
        })
    }

    fn update_from(&mut self, u: &UpstreamUser) {
        let set = |dst: &mut String, src: &String| {
            if !src.is_empty() {
                *dst = src.clone();
            }
        };
        set(&mut self.nickname, &u.nickname);
        set(&mut self.username, &u.username);
        set(&mut self.hostname, &u.hostname);
        set(&mut self.server, &u.server);
        set(&mut self.flags, &u.flags);
        set(&mut self.account, &u.account);
        set(&mut self.realname, &u.realname);
    }
}

#[derive(Clone, Debug)]
pub struct PendingCommand {
    pub downstream_id: u64,
    pub downstream_label: String,
    pub msg: Message,
    pub sent_at: Option<Instant>,
}

#[derive(Clone, Debug)]
struct UpstreamBatch {
    label: String,
    /// Set for draft/multiline batches, which are assembled into a single
    /// message.
    multiline: Option<xirc::MultilineBatch>,
}

pub enum SaslClient {
    Plain { username: String, password: String },
    External,
}

impl SaslClient {
    fn start(&self) -> Vec<u8> {
        match self {
            SaslClient::Plain { username, password } => {
                format!("\0{username}\0{password}").into_bytes()
            }
            SaslClient::External => Vec::new(),
        }
    }
}

pub struct UpstreamConn {
    pub id: u64,
    pub conn: Conn,
    pub logger: Logger,
    pub net_id: i64,
    events: mpsc::UnboundedSender<Event>,
    srv: Arc<Server>,
    rate_flag: RateLimitFlag,
    pub outcome_tx: Option<oneshot::Sender<ConnOutcome>>,
    pub reg_error: Option<OutcomeError>,
    /// Set once registered and the post-registration work is done; the
    /// connection is then visible to downstreams.
    pub attached: bool,
    /// Messages received while waiting before joining channels
    pub delayed_messages: Option<Vec<Message>>,

    pub server_prefix: Prefix,
    pub available_channel_modes: IndexMap<u8, ChannelModeType>,
    pub available_channel_types: String,
    pub available_status_msg: String,
    pub available_memberships: Vec<Membership>,
    pub isupport: Isupport,

    pub registered: bool,
    pub nick: String,
    pub username: String,
    pub realname: String,
    pub hostname: String,
    pub modes: UserModes,
    pub channels: CaseMappingMap<UpstreamChannel>,
    pub users: CaseMappingMap<UpstreamUser>,
    pub caps: CapRegistry,
    batches: HashMap<String, UpstreamBatch>,
    pub away: bool,
    pub account: String,
    next_label_id: u64,
    next_batch_ref: std::cell::Cell<u64>,
    pub monitored: CaseMappingMap<bool>,

    pub sasl_client: Option<SaslClient>,
    sasl_started: bool,

    /// Queue of commands in progress, indexed by type. The first entry has
    /// been sent to the server and is awaiting reply. The following entries
    /// have not been sent yet.
    pub pending_cmds: HashMap<String, Vec<PendingCommand>>,

    pending_regain_nick: String,
    regain_nick_timer: Option<AbortHandle>,
    regain_nick_backoff: Option<Backoffer>,

    got_motd: bool,
    has_desired_nick: bool,
}

impl UpstreamConn {
    pub fn new(new: NewConn, net_logger: &Logger, casemap: CaseMapping) -> UpstreamConn {
        UpstreamConn {
            id: new.id,
            conn: new.conn,
            logger: net_logger.clone(),
            net_id: new.net_id,
            events: new.events,
            srv: new.srv,
            rate_flag: new.rate_flag,
            outcome_tx: Some(new.outcome_tx),
            reg_error: None,
            attached: false,
            delayed_messages: None,
            server_prefix: Prefix::new("*"),
            available_channel_modes: ircutil::std_channel_modes(),
            available_channel_types: ircutil::STD_CHANNEL_TYPES.to_owned(),
            available_status_msg: String::new(),
            available_memberships: ircutil::std_memberships(),
            isupport: Isupport::new(),
            registered: false,
            nick: String::new(),
            username: String::new(),
            realname: String::new(),
            hostname: String::new(),
            modes: UserModes::default(),
            channels: CaseMappingMap::new(casemap),
            users: CaseMappingMap::new(casemap),
            caps: CapRegistry::new(),
            batches: HashMap::new(),
            away: false,
            account: String::new(),
            next_label_id: 0,
            next_batch_ref: std::cell::Cell::new(0),
            monitored: CaseMappingMap::new(casemap),
            sasl_client: None,
            sasl_started: false,
            pending_cmds: HashMap::new(),
            pending_regain_nick: String::new(),
            regain_nick_timer: None,
            regain_nick_backoff: None,
            got_motd: false,
            has_desired_nick: true,
        }
    }

    pub fn send_message(&self, msg: Message) {
        let mut msg = msg;
        if !self.caps.is_enabled("message-tags") {
            msg.tags.clear();
        }
        let prepared = xirc::prepare_text_message(
            msg,
            self.multiline_limits().is_some(),
            || self.prefix_len(),
            || {
                let r = self.next_batch_ref.get() + 1;
                self.next_batch_ref.set(r);
                r.to_string()
            },
        );
        match prepared {
            xirc::TextMessage::Single(m) => self.raw_send(m),
            xirc::TextMessage::Batch(l) => {
                for m in l {
                    self.raw_send(m);
                }
            }
            xirc::TextMessage::Lines(l) => {
                // Only the first message carries the label
                for (i, mut m) in l.into_iter().enumerate() {
                    if i > 0 {
                        m.tags.remove("label");
                    }
                    self.raw_send(m);
                }
            }
        }
    }

    fn raw_send(&self, msg: Message) {
        metrics::inc(&self.srv.metrics.upstream_out_messages_total);
        self.conn.send_message(msg);
    }

    /// The length of the prefix the server relays our messages with.
    fn prefix_len(&self) -> usize {
        // Assume the longest hostname if we don't know ours
        let host_len = if self.hostname.is_empty() {
            63
        } else {
            self.hostname.len()
        };
        let user_len = self.username.len().max(10);
        self.nick.len() + 1 + user_len + 1 + host_len
    }

    /// The draft/multiline limits of the server, if supported.
    pub fn multiline_limits(&self) -> Option<xirc::MultilineLimits> {
        if !self.caps.is_enabled("batch") || !self.caps.is_enabled(xirc::MULTILINE_BATCH_TYPE) {
            return None;
        }
        let v = self
            .caps
            .available
            .get(xirc::MULTILINE_BATCH_TYPE)
            .map(String::as_str)
            .unwrap_or("");
        Some(xirc::MultilineLimits::parse(v))
    }

    /// Returns the number of lines a multiline message would be sent as.
    pub fn multiline_line_count(&self, msg: &Message) -> usize {
        let max = xirc::max_text_length(self.prefix_len(), &msg.command, msg.param(0));
        xirc::split_multiline(msg.param(1), max).len()
    }

    /// Sends a message, labeled so that replies get routed to a downstream
    /// connection. label is the downstream's label (may be empty).
    pub fn send_message_labeled(&mut self, downstream_id: u64, label: &str, msg: Message) {
        let mut msg = msg;
        if self.caps.is_enabled("labeled-response") {
            msg.tags.set(
                "label",
                format!("sd-{downstream_id}-{}-{label}", self.next_label_id),
            );
            self.next_label_id += 1;
        }
        self.send_message(msg);
    }

    pub fn is_channel(&self, entity: &str) -> bool {
        entity
            .chars()
            .next()
            .is_some_and(|c| self.available_channel_types.contains(c))
    }

    pub fn supports_sasl(&self, mech: &str) -> bool {
        let Some(v) = self.caps.available.get("sasl") else {
            return false;
        };
        if v.is_empty() {
            return true;
        }
        v.split(',').any(|m| m.eq_ignore_ascii_case(mech))
    }

    pub fn stop_timers(&self) {
        if let Some(t) = &self.regain_nick_timer {
            t.abort();
        }
        for uch in self.channels.values() {
            if let Some(t) = &uch.detach_timer {
                t.abort();
            }
        }
    }

    pub fn stop_regain_nick_timer(&mut self) {
        if let Some(t) = self.regain_nick_timer.take() {
            // Maybe we're racing with the timer task, so maybe we'll receive
            // a TryRegainNick event later on, but try_regain_nick handles
            // that case
            t.abort();
        }
        self.regain_nick_backoff = None;
    }

    fn schedule_regain_nick(&mut self, delay: Duration, nick: String) {
        let tx = self.events.clone();
        let (net_id, conn_id) = (self.net_id, self.id);
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Event::TryRegainNick {
                net_id,
                conn_id,
                nick,
            });
        });
        self.regain_nick_timer = Some(task.abort_handle());
    }

    /// Starts an auto-detach timer for a channel.
    pub fn start_detach_timer(&self, name: &str, dur: Duration) -> AbortHandle {
        let tx = self.events.clone();
        let (net_id, conn_id) = (self.net_id, self.id);
        let name = name.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(dur).await;
            let _ = tx.send(Event::ChannelDetach {
                net_id,
                conn_id,
                name,
            });
        })
        .abort_handle()
    }

    pub fn cancel_pending_commands_by_downstream_id(&mut self, downstream_id: u64) {
        for l in self.pending_cmds.values_mut() {
            // We can't cancel the currently running command stored in l[0]
            let mut i = l.len();
            while i > 1 {
                i -= 1;
                if l[i].downstream_id == downstream_id {
                    l.remove(i);
                }
            }
        }
    }

    fn parse_membership_prefix(&self, s: &str) -> (MembershipSet, String) {
        let b = s.as_bytes();
        let mut ms = MembershipSet::default();
        let mut i = 0;
        for m in &self.available_memberships {
            if i >= b.len() {
                break;
            }
            if b[i] == m.prefix {
                ms.0.push(*m);
                i += 1;
            }
        }
        (ms, s.get(i..).unwrap_or(s).to_owned())
    }

    fn handle_supported_caps(&mut self, caps: &str) {
        for s in caps.split_whitespace() {
            let (k, v) = match s.split_once('=') {
                Some((k, v)) => (k, v),
                None => (s, ""),
            };
            self.caps.available.insert(k.to_lowercase(), v.to_owned());
        }
    }

    fn update_caps(&mut self) {
        let mut request: Vec<String> = PERMANENT_UPSTREAM_CAPS
            .iter()
            .filter(|c| self.caps.is_available(c) && !self.caps.is_enabled(c))
            .map(|c| c.to_string())
            .collect();

        let echo_message = self.caps.is_available("labeled-response");
        if !self.caps.is_enabled("echo-message") && echo_message {
            // Upstream requests it a second time here; once is enough
            if !request.iter().any(|c| c == "echo-message") {
                request.push("echo-message".to_owned());
            }
        } else if self.caps.is_enabled("echo-message") && !echo_message {
            request.push("-echo-message".to_owned());
        }

        if request.is_empty() {
            return;
        }
        self.send_message(msg!("CAP", "REQ", request.join(" ")));
    }

    fn cache_user_info(&mut self, nick: &str, mut info: UpstreamUser) {
        if nick.is_empty() {
            return;
        }
        if let Some(uu) = self.users.get_mut(nick) {
            uu.update_from(&info);
            if !info.nickname.is_empty() && nick != info.nickname {
                let uu = self.users.del(nick).unwrap();
                let new_nick = uu.nickname.clone();
                self.users.set(&new_nick, uu);
            }
        } else {
            let key = if !info.nickname.is_empty() {
                info.nickname.clone()
            } else {
                info.nickname = nick.to_owned();
                nick.to_owned()
            };
            self.users.set(&key, info);
        }
    }

    pub fn get_cached_who(&self, mask: &str, fields: &str) -> Option<Vec<UpstreamUser>> {
        // Non-extended WHO fields
        let fields = if fields.is_empty() {
            "cuhsnfdr"
        } else {
            fields
        };

        // Some extensions are required to keep our cached state in sync. We
        // could require setname for 'r' and chghost for 'h'/'s', but servers
        // usually implement a QUIT/JOIN fallback, so let's not bother.
        if fields.contains('a') && !self.caps.is_enabled("account-notify") {
            return None;
        }
        if fields.contains('f') && !self.caps.is_enabled("away-notify") {
            return None;
        }

        if let Some(uu) = self.users.get(mask) {
            if uu.has_whox_fields(fields) {
                return Some(vec![uu.clone()]);
            }
        } else if let Some(uch) = self.channels.get(mask) {
            let mut l = Vec::with_capacity(uch.members.len());
            for nick in uch.members.keys() {
                let uu = self.users.get(nick)?;
                if !uu.has_whox_fields(fields) {
                    return None;
                }
                l.push(uu.clone());
            }
            return Some(l);
        }
        None
    }
}

fn is_our_nick(net_cm: CaseMapping, uc: &UpstreamConn, nick: &str) -> bool {
    net_cm(&uc.nick) == net_cm(nick)
}

fn should_cache_user_info(net_cm: CaseMapping, uc: &UpstreamConn, nick: &str) -> bool {
    if is_our_nick(net_cm, uc, nick) {
        return true;
    }
    // keep the cached user info only if we MONITOR it, or we share a channel
    // with them
    if uc.monitored.has(nick) {
        return true;
    }
    uc.channels.values().any(|ch| ch.members.has(nick))
}

pub struct RunnerParams {
    pub srv: Arc<Server>,
    pub tx: mpsc::UnboundedSender<Event>,
    pub user_id: i64,
    pub user_ident: String,
    pub record: Arc<Mutex<db::Network>>,
    pub runner_id: u64,
    pub stop: CancellationToken,
    pub logger: Logger,
}

/// Connects to the upstream server and reconnects with exponential backoff,
/// until the network is stopped.
pub async fn run_network(p: RunnerParams) {
    let mut last_try: Option<Instant> = None;
    let mut backoff = Backoffer::new(
        crate::user::RETRY_CONNECT_MIN_DELAY,
        crate::user::RETRY_CONNECT_MAX_DELAY,
        crate::user::RETRY_CONNECT_JITTER,
    );
    loop {
        // A closed event channel means the user's event loop is gone (e.g.
        // it panicked): stop instead of reconnecting in a loop
        if p.stop.is_cancelled() || p.tx.is_closed() {
            return;
        }

        let delay = backoff.next_delay();
        let elapsed = last_try.map(|t| t.elapsed()).unwrap_or(Duration::MAX);
        if delay > elapsed {
            let delay = delay - elapsed;
            let addr = p.record.lock().unwrap().addr.clone();
            logf!(
                p.logger,
                "waiting {}s before trying to reconnect to {addr:?}",
                delay.as_secs()
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = p.stop.cancelled() => return,
                _ = p.tx.closed() => return,
            }
        }
        last_try = Some(Instant::now());

        let record = p.record.lock().unwrap().clone();
        let net_id = record.id;
        p.srv.metrics.upstreams.fetch_add(1, Ordering::Relaxed);
        let res = run_conn(&p, &record).await;
        p.srv.metrics.upstreams.fetch_sub(1, Ordering::Relaxed);

        match res {
            Ok(()) => backoff.reset(),
            Err(err) => {
                logf!(
                    p.logger,
                    "connection error to {:?}: {}",
                    record.addr,
                    err.text
                );
                let _ = p.tx.send(Event::UpstreamConnectionError {
                    net_id,
                    runner_id: p.runner_id,
                    err: format!("connection error: {}", err.event),
                });
                metrics::inc(&p.srv.metrics.upstream_connect_errors_total);
                if !err.temporary {
                    return;
                }
            }
        }
    }
}

async fn run_conn(p: &RunnerParams, record: &db::Network) -> Result<(), OutcomeError> {
    let to_err = |e: anyhow::Error| {
        let text = format!("failed to connect: {e}");
        OutcomeError {
            event: text.clone(),
            text,
            temporary: true,
        }
    };

    let logger = p
        .logger
        .with_prefix(format!("upstream {:?}: ", record.get_name()));
    let identd = p.srv.identd();
    let connected = tokio::select! {
        res = tokio::time::timeout(CONNECT_TIMEOUT, connect(p, record, &logger, identd.as_deref())) => res,
        _ = p.stop.cancelled() => return Ok(()),
    };
    let (io, _identd_entry) = match connected {
        Ok(Ok(c)) => c,
        // Upstream performs the TLS handshake lazily, on the first write:
        // handshake errors surface while reading registration replies
        Ok(Err(e)) if e.is::<TlsHandshakeError>() => {
            let text = format!("failed to register: failed to read message: {e}");
            return Err(OutcomeError {
                event: text.clone(),
                text,
                temporary: true,
            });
        }
        Ok(Err(e)) => return Err(to_err(e)),
        Err(_) => return Err(to_err(anyhow!("i/o timeout"))),
    };

    let rate_flag: RateLimitFlag = Arc::new(AtomicBool::new(true));
    let rate = RateLimit {
        delay: UPSTREAM_MESSAGE_DELAY,
        burst: UPSTREAM_MESSAGE_BURST,
    };
    let (conn, mut reader) = Conn::new_stream(io, logger.clone(), Some((rate, rate_flag.clone())));
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    let (outcome_tx, outcome_rx) = oneshot::channel();

    let new = NewConn {
        id: conn_id,
        conn: conn.clone(),
        rate_flag,
        outcome_tx,
        net_id: record.id,
        events: p.tx.clone(),
        srv: p.srv.clone(),
    };
    if p.tx
        .send(Event::UpstreamConnected {
            net_id: record.id,
            runner_id: p.runner_id,
            conn: Box::new(new),
        })
        .is_err()
    {
        conn.close();
        return Ok(());
    }

    let read_error = loop {
        let res = tokio::select! {
            res = reader.read_message() => res,
            _ = p.stop.cancelled() => {
                conn.close();
                Ok(None)
            }
            _ = p.tx.closed() => {
                conn.close();
                Ok(None)
            }
        };
        match res {
            Ok(Some(msg)) => {
                metrics::inc(&p.srv.metrics.upstream_in_messages_total);
                let _ = p.tx.send(Event::UpstreamMessage {
                    net_id: record.id,
                    conn_id,
                    msg,
                });
            }
            Ok(None) => break None,
            Err(e) => break Some(e.to_string()),
        }
    };
    conn.close();
    let _ = p.tx.send(Event::UpstreamClosed {
        net_id: record.id,
        conn_id,
        read_error,
    });
    let outcome = outcome_rx.await.unwrap_or(ConnOutcome { err: None });

    if p.stop.is_cancelled() {
        return Ok(());
    }
    match outcome.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Resolves a host, randomizing the result order instead of always
/// returning the same IP address, to distribute bursts of connections to the
/// same host among available addresses. IPv6 is preferred, for per-user
/// local IP addresses.
async fn resolve_ip_addr(host: &str) -> Result<IpAddr> {
    let addrs: Vec<IpAddr> = tokio::net::lookup_host((host, 0))
        .await?
        .map(|a| a.ip())
        .collect();
    let v6: Vec<IpAddr> = addrs.iter().copied().filter(|a| a.is_ipv6()).collect();
    let pool = if v6.is_empty() { addrs } else { v6 };
    pool.choose(&mut rand::rng())
        .copied()
        .ok_or_else(|| anyhow!("no such host"))
}

/// Formats an OS error like Go's syscall.Errno: the C error message with a
/// lowercase first letter, without Rust's " (os error N)" suffix.
pub fn go_errno_message(e: &std::io::Error) -> String {
    let s = e.to_string();
    let s = match s.rfind(" (os error ") {
        Some(i) => &s[..i],
        None => &s,
    };
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Connects to a TCP address, with errors worded like Go's net.Dial.
async fn connect_tcp(addr: &str) -> Result<tokio::net::TcpStream> {
    let host = crate::conn::host_of(addr);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .map_err(|_| anyhow!("dial tcp: lookup {host}: no such host"))?
        .collect();
    let mut last_err = anyhow!("dial tcp: lookup {host}: no such host");
    for a in addrs {
        match tokio::net::TcpStream::connect(a).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = anyhow!("dial tcp {a}: connect: {}", go_errno_message(&e)),
        }
    }
    Err(last_err)
}

async fn dial_tcp(srv: &Server, user_id: i64, addr: &str) -> Result<tokio::net::TcpStream> {
    let upstream_user_ips = srv.config().basic.upstream_user_ips.clone();
    if upstream_user_ips.is_empty() {
        return connect_tcp(addr).await;
    }
    let (host, port) = split_host_port(addr)?;
    let ip = resolve_ip_addr(&host)
        .await
        .map_err(|e| anyhow!("failed to resolve host {host:?}: {e}"))?;
    let local = crate::user::local_tcp_addr(&upstream_user_ips, user_id, ip)
        .map_err(|e| anyhow!("failed to pick local IP for remote host {host:?}: {e}"))?;
    let port: u16 = port.parse()?;
    let remote = SocketAddr::new(ip, port);
    let socket = if ip.is_ipv6() {
        tokio::net::TcpSocket::new_v6()?
    } else {
        tokio::net::TcpSocket::new_v4()?
    };
    if let Some(local) = local {
        socket
            .bind(SocketAddr::new(local, 0))
            .map_err(|e| anyhow!("dial tcp {remote}: bind: {}", go_errno_message(&e)))?;
    }
    socket
        .connect(remote)
        .await
        .map_err(|e| anyhow!("dial tcp {remote}: connect: {}", go_errno_message(&e)))
}

/// Dials the upstream server. Returns the stream and the remote/local
/// addresses (for identd).
async fn connect<'a>(
    p: &RunnerParams,
    record: &db::Network,
    logger: &Logger,
    identd: Option<&'a crate::identd::Identd>,
) -> Result<(BoxIo, Option<IdentdEntry<'a>>)> {
    let u = record.url()?;
    match u.scheme.as_str() {
        "ircs" => {
            let (host, addr) = match split_host_port(&u.host) {
                Ok((h, _)) => (h, u.host.clone()),
                Err(_) => (u.host.clone(), format!("{}:6697", u.host)),
            };
            let host = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned();

            let client_cert = if record.sasl.mechanism == "EXTERNAL" {
                let Some(cert) = &record.sasl.external_cert else {
                    bail!("missing certificate for authentication");
                };
                let Some(key) = &record.sasl.external_key else {
                    bail!("missing private key for authentication");
                };
                use sha2::Digest;
                logf!(
                    logger,
                    "using TLS client certificate {}",
                    hex::encode(sha2::Sha256::digest(cert))
                );
                Some((cert.as_slice(), key.as_slice()))
            } else {
                None
            };
            let tls_config = crate::tls::client_config(&record.certfp, client_cert)?;

            logf!(logger, "connecting to TLS server at address {addr:?}");
            let tcp = dial_tcp(&p.srv, p.user_id, &addr).await?;
            // Register the connection with identd before the TLS handshake:
            // servers may query identd as soon as the TCP connection is up
            let entry = IdentdEntry::store(identd, &tcp, &p.user_ident);
            let server_name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| anyhow!("invalid server name {host:?}: {e}"))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config)
                .connect(server_name, tcp)
                .await
                .map_err(|e| TlsHandshakeError(e.to_string()))?;
            Ok((Box::new(tls), entry))
        }
        "irc+insecure" => {
            let addr = match split_host_port(&u.host) {
                Ok(_) => u.host.clone(),
                Err(_) => format!("{}:6667", u.host),
            };
            logf!(
                logger,
                "connecting to plain-text server at address {addr:?}"
            );
            let tcp = dial_tcp(&p.srv, p.user_id, &addr).await?;
            let entry = IdentdEntry::store(identd, &tcp, &p.user_ident);
            Ok((Box::new(tcp), entry))
        }
        "irc+unix" | "unix" => {
            logf!(logger, "connecting to Unix socket at path {:?}", u.path);
            #[cfg(unix)]
            {
                let s = tokio::net::UnixStream::connect(&u.path)
                    .await
                    .map_err(|e| {
                        anyhow!(
                            "failed to connect to Unix socket {:?}: dial unix {}: connect: {}",
                            u.path,
                            u.path,
                            go_errno_message(&e)
                        )
                    })?;
                Ok((Box::new(s), None))
            }
            #[cfg(not(unix))]
            bail!("Unix sockets are not supported on this platform")
        }
        s => bail!("failed to dial {:?}: unknown scheme: {s}", record.addr),
    }
}

/// A connection registered with identd, removed when dropped.
struct IdentdEntry<'a> {
    identd: &'a crate::identd::Identd,
    remote: String,
    local: String,
}

impl<'a> IdentdEntry<'a> {
    fn store(
        identd: Option<&'a crate::identd::Identd>,
        tcp: &tokio::net::TcpStream,
        ident: &str,
    ) -> Option<IdentdEntry<'a>> {
        let fmt = crate::listen::fmt_addr;
        let entry = IdentdEntry {
            identd: identd?,
            remote: fmt(tcp.peer_addr().ok()?),
            local: fmt(tcp.local_addr().ok()?),
        };
        entry.identd.store(&entry.remote, &entry.local, ident);
        Some(entry)
    }
}

impl Drop for IdentdEntry<'_> {
    fn drop(&mut self) {
        self.identd.delete(&self.remote, &self.local);
    }
}

macro_rules! uc {
    ($u:expr, $ni:expr) => {
        $u.networks[$ni].conn.as_mut().unwrap()
    };
}

/// Sends the registration messages.
pub fn register(u: &mut User, ni: usize) {
    let net = &mut u.networks[ni];
    let nick = get_nick(&u.record, Some(&net.record)).to_owned();
    let username = get_username(&u.record, Some(&net.record)).to_owned();
    let realname = get_realname(&u.record, Some(&net.record)).to_owned();
    let pass = net.record.pass.clone();
    let uc = net.conn.as_mut().unwrap();
    uc.nick = nick.clone();
    uc.username = username.clone();
    uc.realname = realname.clone();

    uc.send_message(msg!("CAP", "LS", "302"));
    if !pass.is_empty() {
        uc.send_message(msg!("PASS", pass));
    }
    uc.send_message(msg!("NICK", nick));
    uc.send_message(msg!("USER", username, "0", "*", realname));
}

/// Runs after RPL_WELCOME: sends connect commands, then joins channels,
/// possibly after a short delay.
pub fn finish_registration(u: &mut User, ni: usize) {
    let net = &mut u.networks[ni];
    let commands = net.record.connect_commands.clone();
    let mechanism = net.record.sasl.mechanism.clone();
    let uc = net.conn.as_mut().unwrap();

    let mut connect_msgs = Vec::new();
    for command in &commands {
        match Message::parse(command) {
            Ok(mut m) => {
                if !uc.caps.is_enabled("message-tags") {
                    m.tags.clear();
                }
                connect_msgs.push(m);
            }
            Err(e) => logf!(
                uc.logger,
                "failed to parse connect command {command:?}: {e}"
            ),
        }
    }

    // Some channels require the user to be logged in to be joined, which may
    // need to happen asynchronously either via connect commands or CertFP
    // without SASL EXTERNAL on networks that don't support this. Work
    // around these old networks by sending connect commands
    // CONNECT_COMMAND_DELAY apart, then leaving 3 more seconds for the
    // authentication to be processed. Meanwhile, server messages are
    // buffered (PINGs are answered).
    let mut delay = false;
    if !commands.is_empty() {
        debugf!(
            uc.logger,
            "sending {} connect command(s)",
            connect_msgs.len()
        );
        // Connect commands usually send a message to NickServ or similar,
        // which asynchronously informs the IRC server that the user is
        // logged in.
        delay = true;
    }
    if mechanism == "EXTERNAL" && !uc.supports_sasl("EXTERNAL") {
        debugf!(
            uc.logger,
            "client certificate configured but the upstream doesn't advertise SASL EXTERNAL, delaying joining channels"
        );
        delay = true;
    }

    if delay {
        uc.delayed_messages = Some(Vec::new());
        let tx = uc.events.clone();
        let conn = uc.conn.clone();
        let srv = uc.srv.clone();
        let (net_id, conn_id) = (uc.net_id, uc.id);
        tokio::spawn(async move {
            // Servers may need time to process each connect command
            for m in connect_msgs {
                if conn.is_closed() {
                    return;
                }
                metrics::inc(&srv.metrics.upstream_out_messages_total);
                conn.send_message(m);
                tokio::time::sleep(CONNECT_COMMAND_DELAY).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = tx.send(Event::UpstreamRegistrationDelayDone { net_id, conn_id });
        });
    } else {
        u.attach_upstream(ni);
    }
}

fn parse_label(label: &str) -> Result<(u64, String)> {
    if label.is_empty() {
        return Ok((0, String::new()));
    }
    let parts: Vec<&str> = label.splitn(4, '-').collect();
    let res = if parts.len() < 4 {
        Err(anyhow!("not enough arguments"))
    } else if parts[0] != "sd" {
        Err(anyhow!("expected sd, got {}", parts[0]))
    } else {
        parts[1].parse::<u64>().map_err(|e| anyhow!("{e}"))
    };
    match res {
        Ok(id) => Ok((id, parts[3].to_owned())),
        Err(e) => {
            bail!("unexpected message label: invalid downstream reference for label {label:?}: {e}")
        }
    }
}

fn need_params(msg: &Message, n: usize) -> Result<()> {
    if msg.params.len() < n {
        return Err(downstream::need_more_params(&msg.command).into());
    }
    Ok(())
}

/// Forwards a message to the downstreams of a network, or only one of them
/// if id isn't 0.
fn forward_msg_by_id(u: &mut User, net_id: i64, id: u64, msg: &Message) {
    for dc in dcs_for(&mut u.downstreams, net_id) {
        if id != 0 && id != dc.id {
            continue;
        }
        dc.send_message(msg.clone());
    }
}

fn forward_message(u: &mut User, net_id: i64, msg: &Message) {
    forward_msg_by_id(u, net_id, 0, msg);
}

/// The result of feeding a message to the draft/multiline assembler.
enum Multiline {
    /// The message isn't part of a multiline batch.
    Passthrough,
    /// The message has been buffered.
    Consumed,
    /// A multiline batch is complete: handle this logical message instead.
    Complete(Message),
}

/// The maximum number of batches a server may have open at once.
const MAX_OPEN_BATCHES: usize = 64;

fn assemble_multiline(uc: &mut UpstreamConn, msg: &Message) -> Result<Multiline> {
    if msg.command == "BATCH" {
        let tag = msg.param(0);
        if tag.starts_with('+') && uc.batches.len() >= MAX_OPEN_BATCHES {
            bail!("too many open batches");
        }
        if let Some(r) = tag.strip_prefix('+')
            && msg.param(1) == xirc::MULTILINE_BATCH_TYPE
        {
            if uc.batches.contains_key(r) {
                bail!("unexpected BATCH reference tag: batch was already defined: {r:?}");
            }
            let batch = UpstreamBatch {
                label: String::new(),
                multiline: Some(xirc::MultilineBatch::new(msg.clone())),
            };
            uc.batches.insert(r.to_owned(), batch);
            return Ok(Multiline::Consumed);
        }
        if let Some(r) = tag.strip_prefix('-')
            && uc.batches.get(r).is_some_and(|b| b.multiline.is_some())
        {
            let batch = uc.batches.remove(r).unwrap().multiline.unwrap();
            return match batch.finish() {
                Ok(m) => Ok(Multiline::Complete(m)),
                Err(e) => {
                    logf!(uc.logger, "ignoring invalid multiline batch: {}", e.code());
                    Ok(Multiline::Consumed)
                }
            };
        }
        return Ok(Multiline::Passthrough);
    }
    if let Some(r) = msg.tags.get("batch")
        && let Some(UpstreamBatch {
            multiline: Some(batch),
            ..
        }) = uc.batches.get_mut(r)
    {
        // The server's limits only apply to what we send, use our own
        batch.push(msg, &xirc::MultilineLimits::default());
        return Ok(Multiline::Consumed);
    }
    Ok(Multiline::Passthrough)
}

/// Handles a message from the upstream server. Like upstream, it may remove
/// tags from the message (e.g. label and batch).
pub fn handle_message(u: &mut User, ni: usize, msg: &mut Message) -> Result<()> {
    match assemble_multiline(uc!(u, ni), msg)? {
        Multiline::Passthrough => {}
        Multiline::Consumed => return Ok(()),
        Multiline::Complete(mut m) => return handle_message(u, ni, &mut m),
    }

    let net_id = u.networks[ni].id();
    let mut label = msg.tags.remove("label").unwrap_or_default();

    let mut batch_name = String::new();
    let mut batch_label = false;
    if let Some(bn) = msg.tags.remove("batch") {
        let uc = uc!(u, ni);
        let Some(b) = uc.batches.get(&bn) else {
            bail!("unexpected batch reference: batch was not defined: {bn:?}");
        };
        // Messages in a labeled batch belong to its labeled response
        if label.is_empty() {
            batch_label = true;
            label = b.label.clone();
        }
        batch_name = bn;
    }

    let (downstream_id, downstream_label) = parse_label(&label)?;
    let mut labeled_dcs = Vec::new();
    if downstream_id != 0 {
        for dc in dcs_for(&mut u.downstreams, net_id) {
            if dc.id != downstream_id {
                continue;
            }
            dc.label = Some(LabelContext {
                label: downstream_label.clone(),
                batch: if batch_label {
                    batch_name.clone()
                } else {
                    String::new()
                },
                pending_msg: None,
            });
            labeled_dcs.push(dc.id);
        }
    }

    let res = handle_message_inner(u, ni, msg, downstream_id, &downstream_label, &label);

    for id in labeled_dcs {
        if let Some(i) = u.dc_index(id) {
            if !batch_label {
                u.downstreams[i].flush_batch();
            }
            u.downstreams[i].label = None;
        }
    }
    res
}

fn handle_message_inner(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    mut downstream_id: u64,
    downstream_label: &str,
    label: &str,
) -> Result<()> {
    let net_id = u.networks[ni].id();
    let cm = u.networks[ni].casemap;

    if msg.prefix.is_none() {
        msg.prefix = Some(uc!(u, ni).server_prefix.clone());
    }

    if !ircutil::is_numeric(&msg.command) {
        let t = msg
            .tags
            .get("time")
            .and_then(xirc::parse_server_time)
            .unwrap_or_else(Utc::now);
        msg.tags.set("time", xirc::format_server_time(t));
    }

    let prefix_name = msg.prefix_name().to_owned();

    match msg.command.as_str() {
        "PING" => {
            uc!(u, ni).send_message(Message::new("PONG", msg.params.clone()));
        }
        "NOTICE" | "PRIVMSG" | "TAGMSG" | "REDACT" => {
            let is_text = msg.command == "NOTICE" || msg.command == "PRIVMSG";
            need_params(msg, if is_text { 2 } else { 1 })?;
            let uc = uc!(u, ni);
            // remove STATUSMSG sigils from target
            let target = msg.params[0]
                .trim_start_matches(|c| uc.available_status_msg.contains(c))
                .to_owned();

            if cm(&prefix_name) == cm(downstream::SERVICE_NICK) {
                logf!(
                    uc.logger,
                    "skipping {} from soju's service: {msg}",
                    msg.command
                );
                return Ok(());
            }
            if cm(&target) == cm(downstream::SERVICE_NICK) {
                logf!(
                    uc.logger,
                    "skipping {} to soju's service: {msg}",
                    msg.command
                );
                return Ok(());
            }

            if !uc.registered
                || cm(&prefix_name) == cm(&uc.server_prefix.name)
                || target == "*"
                || target.starts_with('$')
            {
                // This is a server message
                produce(u, ni, "", msg, 0, false);
                return Ok(());
            }

            let mut direct_message = is_our_nick(cm, uc, &target);
            let mut buffer_name = if direct_message {
                prefix_name.clone()
            } else {
                target.clone()
            };
            if let Some(t) = msg.tags.get("+draft/channel-context")
                && let Some(ch) = uc.channels.get(t)
                && ch.members.has(&prefix_name)
            {
                buffer_name = ch.name.clone();
                direct_message = false;
            }

            let self_msg = is_our_nick(cm, uc, &prefix_name);
            let mut highlight = false;
            let mut detached = false;
            let ch = (u.networks[ni].channels.get(&buffer_name))
                .map(|c| (c.detached, c.detach_on, c.relay_detached));
            if let Some((was_detached, detach_on, relay_detached)) = ch
                && is_text
                && !self_msg
            {
                if was_detached {
                    handle_detached_message(u, ni, &buffer_name, msg);
                }
                highlight = u.networks[ni].is_highlight(&u.record, msg);
                if detach_on == db::MessageFilter::Message
                    || detach_on == db::MessageFilter::Default
                    || (detach_on == db::MessageFilter::Highlight && highlight)
                {
                    update_channel_auto_detach(u, ni, &buffer_name);
                }
                // The message may have reattached the channel
                let (is_detached, relay_detached) = (u.networks[ni].channels.get(&buffer_name))
                    .map_or((was_detached, relay_detached), |c| {
                        (c.detached, c.relay_detached)
                    });
                if is_detached && relay_detached == db::MessageFilter::None {
                    detached = true;
                }
            }

            let mut send_web_push =
                !self_msg && !detached && is_text && (highlight || direct_message);

            let source_cm = cm(&prefix_name);
            let mut blocked = false;
            match u.srv.db.get_message_target(net_id, &source_cm) {
                Ok(mt) => {
                    if mt.muted || mt.blocked {
                        send_web_push = false;
                    }
                    blocked = mt.blocked;
                }
                Err(e) => logf!(
                    u.networks[ni].logger,
                    "failed to get the message target for {source_cm:?}: {e}"
                ),
            }

            if send_web_push {
                let timestamp = msg
                    .tags
                    .get("time")
                    .and_then(xirc::parse_server_time)
                    .unwrap_or_else(Utc::now);
                let target_cm = cm(&buffer_name);
                let srv = u.srv.clone();
                let user_id = u.record.id;
                let logger = u.networks[ni].logger.clone();
                let push_targets = u.networks[ni].push_targets.clone();
                let buffer = buffer_name.clone();
                let m = msg.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let receipt =
                        db::run(&srv.db, move |db| db.get_read_receipt(net_id, &target_cm)).await;
                    match receipt {
                        Err(e) => {
                            logf!(logger, "failed to get the read receipt for {buffer:?}: {e}");
                            return;
                        }
                        Ok(Some(r)) if r.timestamp.is_some_and(|t| t >= timestamp) => return,
                        Ok(_) => {}
                    }
                    push_targets.lock().unwrap().set(&buffer, timestamp);
                    crate::user::broadcast_web_push(srv, user_id, net_id, logger, m).await;
                });
            }

            produce(u, ni, &buffer_name, msg, downstream_id, blocked);
        }
        "CAP" => {
            need_params(msg, 2)?;
            let sub_cmd = msg.params[1].to_uppercase();
            let sub_params = msg.params[2..].to_vec();
            match sub_cmd.as_str() {
                "LS" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    let caps = sub_params.last().unwrap().clone();
                    // Like upstream, the "*" continuation marker is looked
                    // up in the full parameter list
                    let more = sub_params.len() >= 2 && msg.params[sub_params.len() - 2] == "*";
                    let uc = uc!(u, ni);
                    uc.handle_supported_caps(&caps);
                    if more {
                        return Ok(()); // wait to receive all capabilities
                    }
                    uc.update_caps();
                    if request_sasl(u, ni) {
                        return Ok(()); // we'll send CAP END after authentication is completed
                    }
                    uc!(u, ni).send_message(msg!("CAP", "END"));
                }
                "ACK" | "NAK" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    for name in sub_params[0].split_whitespace() {
                        let mut enable = sub_cmd == "ACK";
                        let mut name = name;
                        if let Some(n) = name.strip_prefix('-') {
                            name = n;
                            enable = false;
                        }
                        handle_cap_ack(u, ni, &name.to_lowercase(), enable)?;
                    }
                    if uc!(u, ni).registered {
                        for id in dc_ids_for(&u.downstreams, net_id) {
                            downstream::update_supported_caps(u, id);
                        }
                    }
                }
                "NEW" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    let uc = uc!(u, ni);
                    uc.handle_supported_caps(&sub_params[0]);
                    uc.update_caps();
                }
                "DEL" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    let uc = uc!(u, ni);
                    for c in sub_params[0].split_whitespace() {
                        uc.caps.del(&c.to_lowercase());
                    }
                    if uc.registered {
                        for id in dc_ids_for(&u.downstreams, net_id) {
                            downstream::update_supported_caps(u, id);
                        }
                    }
                }
                _ => debugf!(uc!(u, ni).logger, "unhandled message: {msg}"),
            }
        }
        "AUTHENTICATE" => {
            let uc = uc!(u, ni);
            let Some(client) = &uc.sasl_client else {
                bail!("received unexpected AUTHENTICATE message");
            };
            // TODO: if a challenge is 400 bytes long, buffer it
            if msg.params.is_empty() {
                uc.send_message(msg!("AUTHENTICATE", "*"));
                return Err(downstream::need_more_params(&msg.command).into());
            }
            let challenge_str = &msg.params[0];
            if challenge_str != "+" && BASE64.decode(challenge_str).is_err() {
                uc.send_message(msg!("AUTHENTICATE", "*"));
                bail!("illegal base64 data");
            }
            if uc.sasl_started {
                uc.send_message(msg!("AUTHENTICATE", "*"));
                bail!("sasl: unexpected server challenge");
            }
            let resp = client.start();
            uc.sasl_started = true;
            for m in xirc::generate_sasl(&resp) {
                uc.send_message(m);
            }
        }
        RPL_LOGGEDIN => {
            need_params(msg, 3)?;
            let uc = uc!(u, ni);
            uc.account = msg.params[2].clone();
            let prefix = Prefix::parse(&msg.params[1]);
            uc.username = prefix.user;
            uc.hostname = prefix.host;
            logf!(uc.logger, "logged in with account {:?}", uc.account);
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_account(u, id);
                downstream::update_host(u, id);
            }
        }
        RPL_LOGGEDOUT => {
            need_params(msg, 2)?;
            let uc = uc!(u, ni);
            uc.account.clear();
            let prefix = Prefix::parse(&msg.params[1]);
            uc.username = prefix.user;
            uc.hostname = prefix.host;
            logf!(uc.logger, "logged out");
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_account(u, id);
                downstream::update_host(u, id);
            }
        }
        RPL_VISIBLEHOST => {
            need_params(msg, 2)?;
            let uc = uc!(u, ni);
            match msg.params[1].split_once('@') {
                Some((user, host)) => {
                    uc.username = user.to_owned();
                    uc.hostname = host.to_owned();
                }
                None => uc.hostname = msg.params[1].clone(),
            }
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_host(u, id);
            }
        }
        ERR_NICKLOCKED | RPL_SASLSUCCESS | ERR_SASLFAIL | ERR_SASLTOOLONG | ERR_SASLABORTED
        | ERR_SASLALREADY => {
            need_params(msg, 2)?;
            let info = &msg.params[1];
            let uc = uc!(u, ni);
            match msg.command.as_str() {
                ERR_NICKLOCKED => logf!(
                    uc.logger,
                    "invalid nick used with SASL authentication: {info}"
                ),
                ERR_SASLFAIL => logf!(uc.logger, "SASL authentication failed: {info}"),
                ERR_SASLTOOLONG => logf!(uc.logger, "SASL message too long: {info}"),
                _ => {}
            }
            uc.sasl_client = None;
            uc.sasl_started = false;

            if let Some((dc_id, _)) = dequeue_command(u, ni, "AUTHENTICATE")
                && let Some(i) = u.dc_index(dc_id)
                && u.downstreams[i].sasl.is_some()
            {
                if msg.command == RPL_SASLSUCCESS
                    && let Some(plain) =
                        u.downstreams[i].sasl.as_ref().and_then(|s| s.plain.clone())
                {
                    u.auto_save_sasl_plain(ni, &plain.username, &plain.password);
                }
                u.downstreams[i].end_sasl(Some(msg.clone()));
            }

            let uc = uc!(u, ni);
            if !uc.registered {
                uc.send_message(msg!("CAP", "END"));
            }
        }
        "REGISTER" | "VERIFY" => {
            if let Some((dc_id, cmd)) = dequeue_command(u, ni, &msg.command) {
                if msg.command == "REGISTER" {
                    need_params(msg, 2)?;
                    need_params(&cmd, 3)?;
                    let account = msg.params[1].clone();
                    let password = cmd.params[2].clone();
                    u.auto_save_sasl_plain(ni, &account, &password);
                }
                if let Some(i) = u.dc_index(dc_id) {
                    u.downstreams[i].send_message(msg.clone());
                }
            }
        }
        RPL_WELCOME => {
            need_params(msg, 1)?;
            let uc = uc!(u, ni);
            uc.nick = msg.params[0].clone();
            uc.registered = true;
            uc.server_prefix = msg.prefix.clone().unwrap_or_default();
            logf!(uc.logger, "connection registered with nick {:?}", uc.nick);
        }
        RPL_MYINFO => {
            // Registration info, not forwarded: downstreams get our own
            need_params(msg, 5)?;
        }
        RPL_ISUPPORT => {
            need_params(msg, 2)?;
            let tokens = msg.params[1..msg.params.len() - 1].to_vec();
            for token in tokens {
                let (mut parameter, negate) = match token.strip_prefix('-') {
                    Some(p) => (p.to_owned(), true),
                    None => (token.clone(), false),
                };
                let mut value = String::new();
                let mut has_value = false;
                if !negate {
                    if let Some((p, v)) = token.split_once('=') {
                        parameter = p.to_owned();
                        value = v.to_owned();
                        has_value = true;
                    } else {
                        parameter = token.clone();
                    }
                }
                parameter = match parameter.split_once('/') {
                    Some((prefix, key)) => {
                        format!("{}/{}", prefix.to_lowercase(), key.to_uppercase())
                    }
                    None => parameter.to_uppercase(),
                };
                let value = xirc::decode_isupport_value(&value);

                let uc = uc!(u, ni);
                if has_value {
                    uc.isupport.insert(parameter.clone(), Some(value.clone()));
                } else if !negate {
                    uc.isupport.insert(parameter.clone(), None);
                } else {
                    uc.isupport.remove(&parameter);
                }

                match parameter.as_str() {
                    "CASEMAPPING" => {
                        let casemap =
                            xirc::parse_casemapping(&value).unwrap_or(xirc::CASEMAPPING_RFC1459);
                        u.update_casemapping(ni, casemap);
                    }
                    "CHANMODES" => {
                        if !negate {
                            handle_chan_modes(uc, &value)?;
                        } else {
                            uc.available_channel_modes = ircutil::std_channel_modes();
                        }
                    }
                    "CHANTYPES" => {
                        uc.available_channel_types = if !negate {
                            value.clone()
                        } else {
                            ircutil::STD_CHANNEL_TYPES.to_owned()
                        };
                    }
                    "STATUSMSG" => {
                        uc.available_status_msg = if !negate {
                            value.clone()
                        } else {
                            String::new()
                        };
                    }
                    "PREFIX" => {
                        if !negate {
                            handle_memberships(uc, &value)?;
                        } else {
                            uc.available_memberships = ircutil::std_memberships();
                        }
                    }
                    "soju.im/SAFERATE" => uc.rate_flag.store(negate, Ordering::Relaxed),
                    _ => {}
                }
            }

            update_monitor(u, ni);
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_isupport(u, id);
            }
        }
        ERR_NOMOTD | RPL_ENDOFMOTD => {
            let uc = uc!(u, ni);
            if !uc.got_motd {
                // Ignore the initial MOTD upon connection, but forward
                // subsequent MOTD messages downstream
                uc.got_motd = true;

                // If upstream did not send any CASEMAPPING token, assume it
                // implements the old RFCs with rfc1459.
                if uc.isupport.get("CASEMAPPING").is_none_or(|v| v.is_none()) {
                    u.update_casemapping(ni, xirc::CASEMAPPING_RFC1459);
                }

                // If the server doesn't support MONITOR, periodically try to
                // regain our desired nick
                if !uc!(u, ni).isupport.contains_key("MONITOR") {
                    start_regain_nick_timer(u, ni);
                }
                return Ok(());
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        "BATCH" => {
            need_params(msg, 1)?;
            let tag = msg.params[0].clone();
            if let Some(tag) = tag.strip_prefix('+') {
                let uc = uc!(u, ni);
                if uc.batches.contains_key(tag) {
                    bail!("unexpected BATCH reference tag: batch was already defined: {tag:?}");
                }
                need_params(msg, 2)?;
                // label is the batch's own, or else the one of its parent
                uc.batches.insert(
                    tag.to_owned(),
                    UpstreamBatch {
                        label: label.to_owned(),
                        multiline: None,
                    },
                );
                if downstream_id != 0 && !downstream_label.is_empty() {
                    let prefix = u.srv.prefix();
                    for dc in dcs_for(&mut u.downstreams, net_id) {
                        if dc.id != downstream_id {
                            continue;
                        }
                        if let Some(l) = &mut dc.label {
                            l.label.clear();
                        }
                        dc.send_message(
                            Message::new("BATCH", msg.params.clone())
                                .with_prefix(prefix.clone())
                                .with_tag("label", downstream_label),
                        );
                    }
                }
            } else if let Some(tag) = tag.strip_prefix('-') {
                let uc = uc!(u, ni);
                let Some(b) = uc.batches.remove(tag) else {
                    bail!("unknown BATCH reference tag: {tag:?}");
                };
                // BATCH - does not have @label/@batch attached to it, so
                // the downstream ID is empty. Extract it back from the batch.
                if let Ok((id, lbl)) = parse_label(&b.label)
                    && id != 0
                    && !lbl.is_empty()
                {
                    let prefix = u.srv.prefix();
                    for dc in dcs_for(&mut u.downstreams, net_id) {
                        if dc.id == id {
                            dc.send_message(
                                Message::new("BATCH", msg.params.clone())
                                    .with_prefix(prefix.clone()),
                            );
                        }
                    }
                }
            } else {
                bail!("unexpected BATCH reference tag: missing +/- prefix: {tag:?}");
            }
        }
        "NICK" => {
            need_params(msg, 1)?;
            let new_nick = msg.params[0].clone();
            let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
            let uc = uc!(u, ni);
            let mut me = false;
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(uc.logger, "changed nick from {:?} to {new_nick:?}", uc.nick);
                me = true;
                uc.nick = new_nick.clone();
                if cm(&uc.pending_regain_nick) == cm(&new_nick) {
                    uc.pending_regain_nick.clear();
                    uc.stop_regain_nick_timer();
                }
                if cm(&want_nick) == cm(&new_nick) {
                    uc.has_desired_nick = true;
                }
            }

            let mut log_channels = Vec::new();
            for ch in uc.channels.values_mut() {
                if let Some(ms) = ch.members.del(&prefix_name) {
                    ch.members.set(&new_nick, ms);
                    log_channels.push(ch.name.clone());
                }
            }
            uc.cache_user_info(
                &prefix_name,
                UpstreamUser {
                    nickname: new_nick.clone(),
                    ..Default::default()
                },
            );
            for ch in log_channels {
                append_log(u, ni, &ch, msg);
            }

            if !me {
                forward_message(u, net_id, msg);
            } else {
                for id in dc_ids_for(&u.downstreams, net_id) {
                    downstream::update_nick(u, id);
                }
                update_monitor(u, ni);
            }
        }
        "SETNAME" => {
            need_params(msg, 1)?;
            let new_realname = msg.params[0].clone();
            let uc = uc!(u, ni);
            uc.cache_user_info(
                &prefix_name,
                UpstreamUser {
                    realname: new_realname.clone(),
                    ..Default::default()
                },
            );
            // TODO: consider appending this message to logs
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(
                    uc.logger,
                    "changed realname from {:?} to {new_realname:?}",
                    uc.realname
                );
                uc.realname = new_realname;
                for id in dc_ids_for(&u.downstreams, net_id) {
                    downstream::update_realname(u, id);
                }
            } else {
                forward_message(u, net_id, msg);
            }
        }
        "CHGHOST" => {
            need_params(msg, 2)?;
            let uc = uc!(u, ni);
            // The server sends CHGHOST instead of QUIT/JOIN: keep cached WHO
            // replies up to date
            uc.cache_user_info(
                &prefix_name,
                UpstreamUser {
                    username: msg.params[0].clone(),
                    hostname: msg.params[1].clone(),
                    ..Default::default()
                },
            );
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(
                    uc.logger,
                    "changed prefix from {:?} to {:?}",
                    msg.prefix
                        .as_ref()
                        .map(|p| p.to_string())
                        .unwrap_or_default(),
                    Prefix::full(
                        uc.nick.clone(),
                        msg.params[0].clone(),
                        msg.params[1].clone()
                    )
                    .to_string()
                );
                uc.username = msg.params[0].clone();
                uc.hostname = msg.params[1].clone();
                for id in dc_ids_for(&u.downstreams, net_id) {
                    downstream::update_host(u, id);
                }
            } else {
                // TODO: add fallback with QUIT/JOIN/MODE messages
                forward_message(u, net_id, msg);
            }
        }
        "JOIN" => {
            need_params(msg, 1)?;
            let channels = msg.params[0].clone();
            let prefix = msg.prefix.clone().unwrap_or_default();
            let uc = uc!(u, ni);
            let mut uu = UpstreamUser {
                username: prefix.user.clone(),
                hostname: prefix.host.clone(),
                ..Default::default()
            };
            if uc.caps.is_enabled("away-notify") {
                // we have enough info to build the user flags in a
                // best-effort manner:
                // - the H/G flag is set to Here first, will be replaced by
                //   Gone later if the user is AWAY
                uu.flags = "H".to_owned();
                // - the B (bot mode) flag is set if the JOIN comes from a
                //   bot. Note: we have no way to track the user bot mode
                //   after they have joined (we are not notified of the bot
                //   mode updates), but this is good enough.
                if msg.tags.contains("bot")
                    && let Some(Some(bot)) = uc.isupport.get("BOT")
                {
                    uu.flags.push_str(bot);
                }
                // TODO: add the server operator flag (`*`) if the message
                // has an oper-tag
            }
            if msg.params.len() > 2 {
                // extended-join
                uu.account = msg.params[1].clone();
                uu.realname = msg.params[2].clone();
            }
            uc.cache_user_info(&prefix_name, uu);

            for ch in channels.split(',') {
                let uc = uc!(u, ni);
                if is_our_nick(cm, uc, &prefix_name) {
                    logf!(uc.logger, "joined channel {ch:?}");
                    if let Some(mut old) = uc.channels.del(ch) {
                        old.update_auto_detach(None);
                    }
                    uc.channels.set(
                        ch,
                        UpstreamChannel {
                            name: ch.to_owned(),
                            topic: String::new(),
                            topic_who: None,
                            topic_time: None,
                            status: ChannelStatus::Public,
                            modes: None,
                            creation_time: String::new(),
                            members: CaseMappingMap::new(cm),
                            complete: false,
                            detach_timer: None,
                        },
                    );
                    update_channel_auto_detach(u, ni, ch);
                    uc!(u, ni).send_message(msg!("MODE", ch));
                } else {
                    let Some(uch) = uc.channels.get_mut(ch) else {
                        bail!("unknown channel {ch:?}");
                    };
                    uch.members.set(&prefix_name, MembershipSet::default());
                }

                let mut ch_msg = msg.clone();
                ch_msg.params[0] = ch.to_owned();
                produce(u, ni, ch, &ch_msg, 0, false);
            }
        }
        "PART" => {
            need_params(msg, 1)?;
            let channels = msg.params[0].clone();
            for ch in channels.split(',') {
                let uc = uc!(u, ni);
                if is_our_nick(cm, uc, &prefix_name) {
                    logf!(uc.logger, "parted channel {ch:?}");
                    if let Some(mut uch) = uc.channels.del(ch) {
                        uch.update_auto_detach(None);
                        for nick in uch.members.keys() {
                            if !should_cache_user_info(cm, uc, nick) {
                                uc.users.del(nick);
                            }
                        }
                    }
                } else {
                    let Some(uch) = uc.channels.get_mut(ch) else {
                        bail!("unknown channel {ch:?}");
                    };
                    uch.members.del(&prefix_name);
                    if !should_cache_user_info(cm, uc, &prefix_name) {
                        uc.users.del(&prefix_name);
                    }
                }
                let mut ch_msg = msg.clone();
                ch_msg.params[0] = ch.to_owned();
                produce(u, ni, ch, &ch_msg, 0, false);
            }
        }
        "KICK" => {
            need_params(msg, 2)?;
            let channel = msg.params[0].clone();
            let user = msg.params[1].clone();
            let uc = uc!(u, ni);
            if is_our_nick(cm, uc, &user) {
                logf!(
                    uc.logger,
                    "kicked from channel {channel:?} by {prefix_name}"
                );
                if let Some(mut uch) = uc.channels.del(&channel) {
                    uch.update_auto_detach(None);
                    for nick in uch.members.keys() {
                        if !should_cache_user_info(cm, uc, nick) {
                            uc.users.del(nick);
                        }
                    }
                }
            } else {
                let Some(uch) = uc.channels.get_mut(&channel) else {
                    bail!("unknown channel {channel:?}");
                };
                uch.members.del(&user);
                if !should_cache_user_info(cm, uc, &user) {
                    uc.users.del(&user);
                }
            }
            produce(u, ni, &channel, msg, 0, false);
        }
        "QUIT" => {
            let uc = uc!(u, ni);
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(uc.logger, "quit");
            }
            let mut log_channels = Vec::new();
            for ch in uc.channels.values_mut() {
                if ch.members.del(&prefix_name).is_some() {
                    log_channels.push(ch.name.clone());
                }
            }
            uc.users.del(&prefix_name);
            let our_nick = uc.nick.clone();
            for ch in log_channels {
                append_log(u, ni, &ch, msg);
            }
            if prefix_name != our_nick {
                forward_message(u, net_id, msg);
            }
        }
        RPL_TOPIC | RPL_NOTOPIC => {
            need_params(msg, 3)?;
            let name = msg.params[1].clone();
            let topic = msg.params[2].clone();
            let uc = uc!(u, ni);
            match uc.channels.get_mut(&name) {
                None => forward_msg_by_id(u, net_id, downstream_id, msg),
                Some(ch) => {
                    ch.topic = if msg.command == RPL_TOPIC {
                        topic
                    } else {
                        String::new()
                    };
                }
            }
        }
        "TOPIC" => {
            need_params(msg, 1)?;
            let name = msg.params[0].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&name) else {
                bail!("unknown channel {name:?}");
            };
            if msg.params.len() > 1 {
                ch.topic = msg.params[1].clone();
                ch.topic_who = msg.prefix.clone();
                ch.topic_time = Some(
                    msg.tags
                        .get("time")
                        .and_then(xirc::parse_server_time)
                        .unwrap_or_else(Utc::now),
                );
            } else {
                ch.topic.clear();
            }
            let ch_name = ch.name.clone();
            produce(u, ni, &ch_name, msg, 0, false);
        }
        "MODE" => {
            need_params(msg, 2)?;
            let name = msg.params[0].clone();
            let mode_str = msg.params[1].clone();
            let uc = uc!(u, ni);
            if !uc.is_channel(&name) {
                // user mode change
                if name != uc.nick {
                    bail!("received MODE message for unknown nick {name:?}");
                }
                uc.modes.apply(&mode_str).map_err(|e| anyhow!(e))?;
                forward_message(u, net_id, msg);
            } else {
                // channel mode change
                let avail_memberships = uc.available_memberships.clone();
                let avail_modes = uc.available_channel_modes.clone();
                let Some(ch) = uc.channels.get_mut(&name) else {
                    bail!("unknown channel {name:?}");
                };
                ircutil::apply_channel_modes(
                    ch.modes.as_mut(),
                    &mut ch.members,
                    &avail_memberships,
                    &avail_modes,
                    &mode_str,
                    &msg.params[2..],
                )
                .map_err(|e| anyhow!(e))?;
                let ch_name = ch.name.clone();
                append_log(u, ni, &ch_name, msg);
                let detached = u.networks[ni]
                    .channels
                    .get(&name)
                    .is_some_and(|c| c.detached);
                if !detached {
                    forward_message(u, net_id, msg);
                }
            }
        }
        RPL_UMODEIS => {
            need_params(msg, 1)?;
            let mode_str = msg.params.get(1).cloned().unwrap_or_default();
            let uc = uc!(u, ni);
            uc.modes = UserModes::default();
            uc.modes.apply(&mode_str).map_err(|e| anyhow!(e))?;
            forward_message(u, net_id, msg);
        }
        RPL_CHANNELMODEIS => {
            need_params(msg, 2)?;
            let channel = msg.params[1].clone();
            let mode_str = msg.params.get(2).cloned().unwrap_or_default();
            let mode_args: Vec<String> =
                msg.params.get(3..).map(|s| s.to_vec()).unwrap_or_default();
            let uc = uc!(u, ni);
            let avail_memberships = uc.available_memberships.clone();
            let avail_modes = uc.available_channel_modes.clone();
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(());
            };
            let first_mode = ch.modes.is_none();
            ch.modes = Some(ChannelModes::new());
            ircutil::apply_channel_modes(
                ch.modes.as_mut(),
                &mut ch.members,
                &avail_memberships,
                &avail_modes,
                &mode_str,
                &mode_args,
            )
            .map_err(|e| anyhow!(e))?;
            let detached = u.networks[ni]
                .channels
                .get(&channel)
                .is_some_and(|c| c.detached);
            if first_mode && !detached {
                forward_message(u, net_id, msg);
            }
        }
        RPL_CREATIONTIME => {
            need_params(msg, 3)?;
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(());
            };
            let first = ch.creation_time.is_empty();
            ch.creation_time = msg.params[2].clone();
            let detached = u.networks[ni]
                .channels
                .get(&channel)
                .is_some_and(|c| c.detached);
            if first && !detached {
                forward_message(u, net_id, msg);
            }
        }
        RPL_TOPICWHOTIME => {
            need_params(msg, 4)?;
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(());
            };
            let first = ch.topic_who.is_none();
            ch.topic_who = Some(Prefix::parse(&msg.params[2]));
            let sec: i64 = msg.params[3]
                .parse()
                .map_err(|e| anyhow!("failed to parse topic time: {e}"))?;
            ch.topic_time = chrono::DateTime::from_timestamp(sec, 0);
            let detached = u.networks[ni]
                .channels
                .get(&channel)
                .is_some_and(|c| c.detached);
            if first && !detached {
                forward_message(u, net_id, msg);
            }
        }
        RPL_LISTSTART | RPL_LIST => {
            let Some((dc_id, _)) = current_pending_command(u, ni, "LIST") else {
                bail!("unexpected RPL_LIST: no matching pending LIST");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_LISTEND => {
            let Some((dc_id, _)) = dequeue_command(u, ni, "LIST") else {
                bail!("unexpected RPL_LISTEND: no matching pending LIST");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_NAMREPLY => {
            need_params(msg, 4)?;
            let status_str = msg.params[1].clone();
            let name = msg.params[2].clone();
            let members = msg.params[3].clone();
            let uc = uc!(u, ni);
            if !uc.channels.has(&name) {
                // NAMES on a channel we have not joined, forward to downstream
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(());
            }
            let status = ChannelStatus::parse(&status_str).map_err(|e| anyhow!(e))?;
            let parsed: Vec<(MembershipSet, String)> = members
                .split(' ')
                .filter(|s| !s.is_empty())
                .map(|s| uc.parse_membership_prefix(s))
                .collect();
            let ch = uc.channels.get_mut(&name).unwrap();
            ch.status = status;
            for (ms, nick) in parsed {
                ch.members.set(&nick, ms);
            }
        }
        RPL_ENDOFNAMES => {
            need_params(msg, 2)?;
            let name = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&name) else {
                // NAMES on a channel we have not joined, forward to downstream
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(());
            };
            if ch.complete {
                bail!("received unexpected RPL_ENDOFNAMES");
            }
            ch.complete = true;
            let detached = u.networks[ni]
                .channels
                .get(&name)
                .is_some_and(|c| c.detached);
            if !detached {
                let net = &u.networks[ni];
                let uch = net.conn.as_ref().unwrap().channels.get(&name).unwrap();
                for dc in u
                    .downstreams
                    .iter_mut()
                    .filter(|d| d.network_id == Some(net_id))
                {
                    downstream::forward_channel(&u.srv.db, dc, uch, net_id, cm);
                }
            }
        }
        RPL_WHOREPLY => {
            need_params(msg, 8)?;
            let (username, host, server, nick, flags, trailing) = (
                msg.params[2].clone(),
                msg.params[3].clone(),
                msg.params[4].clone(),
                msg.params[5].clone(),
                msg.params[6].clone(),
                msg.params[7].clone(),
            );
            let Some((dc_id, _)) = current_pending_command(u, ni, "WHO") else {
                bail!("unexpected RPL_WHOREPLY: no matching pending WHO");
            };
            let Some((_, realname)) = trailing.split_once(' ') else {
                bail!("malformed RPL_WHOREPLY: failed to parse real name");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            } else {
                return Ok(());
            }
            let uc = uc!(u, ni);
            if should_cache_user_info(cm, uc, &nick) {
                let flags = ircutil::strip_member_prefixes(&flags, &uc.available_memberships);
                uc.cache_user_info(
                    &nick,
                    UpstreamUser {
                        username,
                        hostname: host,
                        server,
                        nickname: nick.clone(),
                        flags,
                        realname: realname.to_owned(),
                        ..Default::default()
                    },
                );
            }
        }
        RPL_WHOSPCRPL => {
            let Some((dc_id, cmd)) = current_pending_command(u, ni, "WHO") else {
                bail!("unexpected RPL_WHOSPCRPL: no matching pending WHO");
            };
            match u.dc_index(dc_id) {
                Some(i) => u.downstreams[i].send_message(msg.clone()),
                None => return Ok(()),
            }
            if cmd.params.len() > 1 {
                let (fields, _) = xirc::parse_whox_options(&cmd.params[1]);
                if !fields.contains('n') {
                    return Ok(());
                }
                let info = xirc::parse_whox_reply(msg, &fields).map_err(|e| anyhow!(e))?;
                let uc = uc!(u, ni);
                if should_cache_user_info(cm, uc, &info.nickname) {
                    let flags =
                        ircutil::strip_member_prefixes(&info.flags, &uc.available_memberships);
                    uc.cache_user_info(
                        &info.nickname.clone(),
                        UpstreamUser {
                            nickname: info.nickname,
                            username: info.username,
                            hostname: info.hostname,
                            server: info.server,
                            flags,
                            account: info.account,
                            realname: info.realname,
                        },
                    );
                }
            }
        }
        RPL_ENDOFWHO => {
            // Some servers send RPL_TRYAGAIN followed by RPL_ENDOFWHO
            if let Some((dc_id, _)) = dequeue_command(u, ni, "WHO")
                && let Some(i) = u.dc_index(dc_id)
            {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_WHOISCERTFP | RPL_WHOISREGNICK | RPL_WHOISUSER | RPL_WHOISSERVER
        | RPL_WHOISCHANNELS | RPL_WHOISOPERATOR | RPL_WHOISIDLE | RPL_WHOISSPECIAL
        | RPL_WHOISACCOUNT | RPL_WHOISACTUALLY | RPL_WHOISHOST | RPL_WHOISMODES
        | RPL_WHOISSECURE => {
            let Some((dc_id, _)) = current_pending_command(u, ni, "WHOIS") else {
                bail!(
                    "unexpected WHOIS reply {:?}: no matching pending WHOIS",
                    msg.command
                );
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_ENDOFWHOIS => {
            let Some((dc_id, _)) = dequeue_command(u, ni, "WHOIS") else {
                bail!("unexpected RPL_ENDOFWHOIS: no matching pending WHOIS");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        "INVITE" => {
            need_params(msg, 2)?;
            let nick = msg.params[0].clone();
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let we_are_invited = is_our_nick(cm, uc, &nick);
            let mut blocked = false;
            if we_are_invited {
                let joined = uc.channels.has(&channel);
                if !joined && let Some(c) = u.networks[ni].channels.get(&channel) {
                    // Automatically join a saved channel when we are invited
                    let msgs = xirc::generate_join(
                        std::slice::from_ref(&c.name),
                        std::slice::from_ref(&c.key),
                    );
                    let uc = uc!(u, ni);
                    for m in msgs {
                        uc.send_message(m);
                    }
                    return Ok(());
                }
                let source_cm = cm(&prefix_name);
                match u.srv.db.get_message_target(net_id, &source_cm) {
                    Ok(mt) => blocked = mt.blocked,
                    Err(e) => logf!(
                        u.networks[ni].logger,
                        "failed to get the message target for {source_cm:?}: {e}"
                    ),
                }
            }
            for dc in dcs_for(&mut u.downstreams, net_id) {
                if !we_are_invited && !dc.caps.is_enabled("invite-notify") {
                    continue;
                }
                if blocked && !dc.metadata_subs.contains("soju.im/blocked") {
                    continue;
                }
                dc.send_message(msg.clone());
            }
            if we_are_invited {
                u.broadcast_web_push(ni, msg.clone());
            }
        }
        RPL_INVITING => {
            need_params(msg, 3)?;
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        RPL_MONONLINE | RPL_MONOFFLINE => {
            need_params(msg, 2)?;
            let targets: Vec<String> = msg.params[1].split(',').map(str::to_owned).collect();
            let online = msg.command == RPL_MONONLINE;
            let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
            let uc = uc!(u, ni);
            for target in &targets {
                let prefix = Prefix::parse(target);
                uc.monitored.set(&prefix.name, online);
            }

            // Check if the nick we want is now free
            if !online && !is_our_nick(cm, uc, &want_nick) && !uc.has_desired_nick {
                let found = targets
                    .iter()
                    .any(|t| cm(&Prefix::parse(t).name) == cm(&want_nick));
                if found {
                    logf!(uc.logger, "desired nick {want_nick:?} is now available");
                    uc.send_message(msg!("NICK", want_nick));
                }
            }

            for dc in dcs_for(&mut u.downstreams, net_id) {
                for target in &targets {
                    let prefix = Prefix::parse(target);
                    if dc.monitored.has(&prefix.name) {
                        let nick = dc.nick.clone();
                        dc.send_message(Message::new(
                            msg.command.clone(),
                            vec![nick, target.clone()],
                        ));
                    }
                }
            }
        }
        ERR_MONLISTFULL => {
            need_params(msg, 3)?;
            let limit = msg.params[1].clone();
            let targets: Vec<String> = msg.params[2].split(',').map(str::to_owned).collect();
            for dc in dcs_for(&mut u.downstreams, net_id) {
                for target in &targets {
                    if dc.monitored.has(target) {
                        let nick = dc.nick.clone();
                        dc.send_message(Message::new(
                            msg.command.clone(),
                            vec![nick, limit.clone(), target.clone()],
                        ));
                    }
                }
            }
        }
        RPL_AWAY => forward_msg_by_id(u, net_id, downstream_id, msg),
        "AWAY" => {
            // Update user flags, if we already have the flags cached
            let uc = uc!(u, ni);
            if let Some(uu) = uc.users.get(&prefix_name)
                && !uu.flags.is_empty()
            {
                let flags = if !msg.params.is_empty() {
                    uu.flags.replace('H', "G")
                } else {
                    uu.flags.replace('G', "H")
                };
                uc.cache_user_info(
                    &prefix_name,
                    UpstreamUser {
                        flags,
                        ..Default::default()
                    },
                );
            }
            forward_message(u, net_id, msg);
        }
        "ACCOUNT" => {
            need_params(msg, 1)?;
            let account = msg.params[0].clone();
            uc!(u, ni).cache_user_info(
                &prefix_name,
                UpstreamUser {
                    account,
                    ..Default::default()
                },
            );
            forward_message(u, net_id, msg);
        }
        RPL_BANLIST | RPL_INVITELIST | RPL_EXCEPTLIST | RPL_ENDOFBANLIST | RPL_ENDOFINVITELIST
        | RPL_ENDOFEXCEPTLIST => forward_msg_by_id(u, net_id, downstream_id, msg),
        ERR_NOSUCHNICK | ERR_NOSUCHSERVER => {
            // one argument WHOIS variant errors with NOSUCHNICK
            // two argument WHOIS variant errors with NOSUCHSERVER
            need_params(msg, 3)?;
            let nick = msg.params[1].clone();
            match current_pending_command(u, ni, "WHOIS") {
                Some((dc_id, cmd)) if cmd.params.last().is_some_and(|p| cm(p) == cm(&nick)) => {
                    dequeue_command(u, ni, "WHOIS");
                    if let Some(i) = u.dc_index(dc_id) {
                        u.downstreams[i].send_message(msg.clone());
                    }
                }
                _ => forward_msg_by_id(u, net_id, downstream_id, msg),
            }
        }
        ERR_UNKNOWNERROR | ERR_UNKNOWNCOMMAND | ERR_NEEDMOREPARAMS | RPL_TRYAGAIN => {
            need_params(msg, 3)?;
            let command = msg.params[1].clone();
            if let Some((dc_id, _)) = dequeue_command(u, ni, &command)
                && downstream_id == 0
                && u.dc_index(dc_id).is_some()
            {
                downstream_id = dc_id;
            }
            if command == "AUTHENTICATE" {
                let uc = uc!(u, ni);
                uc.sasl_client = None;
                uc.sasl_started = false;
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        "FAIL" => {
            need_params(msg, 2)?;
            let command = msg.params[0].clone();
            let code = msg.params[1].clone();
            if !uc!(u, ni).registered && command == "*" && code == "ACCOUNT_REQUIRED" {
                return Err(RegistrationError(msg.clone()).into());
            }
            if let Some((dc_id, _)) = dequeue_command(u, ni, &command)
                && downstream_id == 0
                && u.dc_index(dc_id).is_some()
            {
                downstream_id = dc_id;
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        // Ignore
        "ACK" | RPL_NOWAWAY | RPL_UNAWAY | RPL_YOURHOST | RPL_CREATED => {}
        RPL_LUSERCLIENT | RPL_LUSEROP | RPL_LUSERUNKNOWN | RPL_LUSERCHANNELS | RPL_LUSERME
        | RPL_STATSVLINE | RPL_STATSPING | RPL_STATSBLINE | RPL_STATSDLINE | RPL_LOCALUSERS
        | RPL_GLOBALUSERS | RPL_MOTDSTART | RPL_MOTD => {
            // Ignore these messages if they're part of the initial
            // registration message burst. Forward them if the user
            // explicitly asked for them.
            if !uc!(u, ni).got_motd {
                return Ok(());
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        "ERROR" => {
            need_params(msg, 1)?;
            bail!("fatal server error: {}", msg.params[0]);
        }
        ERR_NICKNAMEINUSE | ERR_PASSWDMISMATCH | ERR_ERRONEUSNICKNAME | ERR_NICKCOLLISION
        | ERR_UNAVAILRESOURCE | ERR_NOPERMFORHOST | ERR_YOUREBANNEDCREEP => {
            if msg.command == ERR_NICKNAMEINUSE {
                let uc = uc!(u, ni);
                // At this point, we haven't received ISUPPORT so we don't
                // know the maximum nickname length or whether the server
                // supports MONITOR. Many servers have NICKLEN=30 so let's
                // just use that.
                if !uc.registered && uc.nick.len() + 1 < 30 {
                    uc.nick.push('_');
                    uc.has_desired_nick = false;
                    logf!(
                        uc.logger,
                        "desired nick is not available, falling back to {:?}",
                        uc.nick
                    );
                    let nick = uc.nick.clone();
                    uc.send_message(msg!("NICK", nick));
                    return Ok(());
                }
            }
            // Servers refuse a nick with any of these, e.g. 437 for a nick
            // held by services
            let nick_error = matches!(
                msg.command.as_str(),
                ERR_NICKNAMEINUSE | ERR_ERRONEUSNICKNAME | ERR_NICKCOLLISION | ERR_UNAVAILRESOURCE
            );
            let uc = uc!(u, ni);
            if nick_error
                && uc.registered
                && msg.params.len() >= 2
                && !uc.pending_regain_nick.is_empty()
                && cm(&uc.pending_regain_nick) == cm(&msg.params[1])
            {
                // This message comes from our own logic to try to regain our
                // desired nick, don't relay to downstream connections
                uc.pending_regain_nick.clear();
                return Ok(());
            }
            if !uc!(u, ni).registered {
                return Err(RegistrationError(msg.clone()).into());
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        _ => {
            debugf!(uc!(u, ni).logger, "unhandled message: {msg}");
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
    }
    Ok(())
}

fn handle_detached_message(u: &mut User, ni: usize, name: &str, msg: &Message) {
    let net = &u.networks[ni];
    let Some(ch) = net.channels.get(name) else {
        return;
    };
    let net_id = net.id();
    let relay = net.detached_message_needs_relay(&u.record, ch, msg);
    let reattach = ch.reattach_on == db::MessageFilter::Message
        || (ch.reattach_on == db::MessageFilter::Highlight && net.is_highlight(&u.record, msg));
    if relay {
        let highlight = net.is_highlight(&u.record, msg);
        for dc in dcs_for(&mut u.downstreams, net_id) {
            downstream::relay_detached_message(dc, highlight, msg);
        }
    }
    if reattach {
        u.attach_channel(ni, name);
        u.store_channel(ni, name);
    }
}

fn handle_chan_modes(uc: &mut UpstreamConn, s: &str) -> Result<()> {
    let parts: Vec<&str> = s.splitn(5, ',').collect();
    if parts.len() < 4 {
        bail!("malformed ISUPPORT CHANMODES value: {s}");
    }
    let mut modes = IndexMap::new();
    for (i, mt) in [
        ChannelModeType::A,
        ChannelModeType::B,
        ChannelModeType::C,
        ChannelModeType::D,
    ]
    .into_iter()
    .enumerate()
    {
        for m in parts[i].bytes() {
            modes.insert(m, mt);
        }
    }
    uc.available_channel_modes = modes;
    Ok(())
}

fn handle_memberships(uc: &mut UpstreamConn, s: &str) -> Result<()> {
    if s.is_empty() {
        uc.available_memberships = Vec::new();
        return Ok(());
    }
    let b = s.as_bytes();
    // Memberships are single bytes: a multi-byte prefix can't be matched
    if b[0] != b'(' || !s.is_ascii() {
        bail!("malformed ISUPPORT PREFIX value: {s}");
    }
    let Some(sep) = s.find(')') else {
        bail!("malformed ISUPPORT PREFIX value: {s}");
    };
    if b.len() != sep * 2 {
        bail!("malformed ISUPPORT PREFIX value: {s}");
    }
    uc.available_memberships = (0..b.len() / 2 - 1)
        .map(|i| Membership {
            mode: b[i + 1],
            prefix: b[sep + i + 1],
        })
        .collect();
    Ok(())
}

fn request_sasl(u: &User, ni: usize) -> bool {
    let net = &u.networks[ni];
    let mech = &net.record.sasl.mechanism;
    if mech.is_empty() {
        return false;
    }
    net.conn.as_ref().unwrap().supports_sasl(mech)
}

fn handle_cap_ack(u: &mut User, ni: usize, name: &str, ok: bool) -> Result<()> {
    uc!(u, ni).caps.set_enabled(name, ok);
    match name {
        "sasl" => {
            if !request_sasl(u, ni) {
                return Ok(());
            }
            let auth = u.networks[ni].record.sasl.clone();
            let uc = uc!(u, ni);
            if !ok {
                logf!(
                    uc.logger,
                    "server refused to acknowledge the SASL capability"
                );
                return Ok(());
            }
            match auth.mechanism.as_str() {
                "PLAIN" => {
                    logf!(
                        uc.logger,
                        "starting SASL PLAIN authentication with username {:?}",
                        auth.plain_username
                    );
                    uc.sasl_client = Some(SaslClient::Plain {
                        username: auth.plain_username.clone(),
                        password: auth.plain_password.clone(),
                    });
                }
                "EXTERNAL" => {
                    logf!(uc.logger, "starting SASL EXTERNAL authentication");
                    uc.sasl_client = Some(SaslClient::External);
                }
                mech => bail!("unsupported SASL mechanism {mech:?}"),
            }
            uc.send_message(msg!("AUTHENTICATE", auth.mechanism));
        }
        "echo-message" => {}
        _ => {
            if !PERMANENT_UPSTREAM_CAPS.contains(&name) {
                logf!(
                    uc!(u, ni).logger,
                    "received CAP ACK/NAK for a cap we don't support: {name}"
                );
            }
        }
    }
    Ok(())
}

/// Appends a message to the message store. Returns the internal message
/// ID, or an empty string if the message isn't stored.
pub fn append_log(u: &mut User, ni: usize, entity: &str, msg: &Message) -> String {
    let store = &mut u.msg_store;
    if msg.command == "TAGMSG"
        && !msg
            .tags
            .iter()
            .any(|(k, _)| STORABLE_MESSAGE_TAGS.contains(&k))
    {
        return String::new();
    }
    // Don't store messages with a server mask target
    if entity.starts_with('$') {
        return String::new();
    }
    let net = &mut u.networks[ni];
    let entity_cm = net.casemap(entity);
    if entity_cm == "nickserv" {
        // The messages sent/received from NickServ may contain
        // security-related information (like passwords). Don't store these.
        return String::new();
    }

    if !net.delivered.is_empty() && !net.delivered.has_target(entity) {
        // This is the first message we receive from this target. Save the
        // last message ID in delivery receipts, so that we can send the new
        // message in the backlog if an offline client reconnects.
        let last_id = match store.last_msg_id(&net.record, &entity_cm) {
            Ok(id) => id,
            Err(e) => {
                logf!(
                    net.logger,
                    "failed to log message: failed to get last message ID: {e}"
                );
                return String::new();
            }
        };
        for client in net.delivered.clients() {
            net.delivered.store_id(entity, &client, &last_id);
        }
    }

    match store.append(&net.record, &entity_cm, msg) {
        Ok(id) => id,
        Err(e) => {
            logf!(net.logger, "failed to append message to store: {e}");
            String::new()
        }
    }
}

/// Appends a message to the logs and forwards it to connected downstream
/// connections.
///
/// origin_id is the ID of the downstream that sent the message. If it's not
/// 0 and the origin doesn't support echo-message, the message is forwarded
/// to all connections except the origin.
pub fn produce(
    u: &mut User,
    ni: usize,
    target: &str,
    msg: &Message,
    origin_id: u64,
    blocked: bool,
) {
    let msg_id = if !target.is_empty() {
        append_log(u, ni, target, msg)
    } else {
        String::new()
    };

    // Don't forward messages if it's a detached channel
    let net = &u.networks[ni];
    let detached = net.channels.get(target).is_some_and(|c| c.detached);
    let net_id = net.id();
    let cm = net.casemap;
    let our_nick = net
        .conn
        .as_ref()
        .map(|uc| uc.nick.clone())
        .unwrap_or_default();
    let from_us = msg
        .prefix
        .as_ref()
        .is_some_and(|p| cm(&p.name) == cm(&our_nick));
    for dc in dcs_for(&mut u.downstreams, net_id) {
        if blocked && !dc.metadata_subs.contains("soju.im/blocked") {
            continue;
        }
        let echo = dc.id == origin_id && from_us;
        if !detached && (!echo || dc.caps.is_enabled("echo-message")) {
            dc.send_message_with_id(msg.clone(), &msg_id);
        } else {
            dc.advance_message_with_id(msg, &msg_id);
        }
    }
}

pub fn update_channel_auto_detach(u: &mut User, ni: usize, name: &str) {
    let net = &mut u.networks[ni];
    let Some(ch) = net.channels.get(name) else {
        return;
    };
    if ch.detached {
        return;
    }
    let dur = ch.detach_after;
    let Some(uc) = net.upstream_mut() else {
        // Go's updateChannelAutoDetach also runs on registering
        // connections, which have no channels yet
        return;
    };
    let timer = if dur.is_zero() {
        None
    } else {
        let uch_name = match uc.channels.get(name) {
            Some(uch) => uch.name.clone(),
            None => return,
        };
        Some(uc.start_detach_timer(&uch_name, dur))
    };
    if let Some(uch) = uc.channels.get_mut(name) {
        uch.update_auto_detach(timer);
    } else if let Some(t) = timer {
        t.abort();
    }
}

pub fn update_monitor(u: &mut User, ni: usize) {
    let net = &u.networks[ni];
    let net_id = net.id();
    let cm = net.casemap;
    let want_nick = get_nick(&u.record, Some(&net.record)).to_owned();
    let Some(uc) = u.networks[ni].conn.as_mut() else {
        return;
    };
    if !uc.isupport.contains_key("MONITOR") {
        return;
    }

    let mut add: HashSet<String> = HashSet::new();
    let mut add_list = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dc in u
        .downstreams
        .iter()
        .filter(|d| d.network_id == Some(net_id))
    {
        for target in dc.monitored.keys() {
            let target_cm = cm(target);
            if target_cm == downstream::SERVICE_NICK_CM {
                continue;
            }
            if !uc.monitored.has(&target_cm) {
                if add.insert(target_cm.clone()) {
                    add_list.push(target_cm);
                }
            } else {
                seen.insert(target_cm);
            }
        }
    }

    // Monitor our desired nick, to regain it once it's available
    let want_nick_cm = cm(&want_nick);
    if !is_our_nick(cm, uc, &want_nick) && !uc.has_desired_nick {
        if uc.monitored.has(&want_nick) {
            seen.insert(want_nick_cm);
        } else if add.insert(want_nick_cm.clone()) {
            add_list.push(want_nick_cm);
        }
    }

    let mut remove_all = true;
    let mut remove_list = Vec::new();
    for nick in uc.monitored.keys() {
        if seen.contains(&cm(nick)) {
            remove_all = false;
        } else {
            remove_list.push(nick.to_owned());
        }
    }

    // TODO: better handle the case where len(uc.monitored) + len(add_list)
    // exceeds the limit, probably by immediately sending ERR_MONLISTFULL?

    if remove_all && add_list.is_empty() && !remove_list.is_empty() {
        // Optimization when the last MONITOR-aware downstream disconnects
        uc.send_message(msg!("MONITOR", "C"));
    } else {
        let mut msgs = xirc::generate_monitor("-", &remove_list);
        msgs.extend(xirc::generate_monitor("+", &add_list));
        for m in msgs {
            uc.send_message(m);
        }
    }

    for target in remove_list {
        uc.monitored.del(&target);
        if !should_cache_user_info(cm, uc, &target) {
            uc.users.del(&target);
        }
    }
}

fn start_regain_nick_timer(u: &mut User, ni: usize) {
    let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
    let cm = u.networks[ni].casemap;
    let uc = uc!(u, ni);
    if uc.regain_nick_backoff.is_some() || uc.regain_nick_timer.is_some() {
        panic!("start_regain_nick_timer called twice");
    }
    if is_our_nick(cm, uc, &want_nick) {
        return;
    }
    let mut backoff = Backoffer::new(
        Duration::from_secs(15),
        Duration::from_secs(600),
        Duration::from_secs(10),
    );
    let delay = backoff.next_delay();
    uc.regain_nick_backoff = Some(backoff);
    uc.schedule_regain_nick(delay, want_nick);
}

pub fn try_regain_nick(u: &mut User, ni: usize, nick: &str) {
    let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
    let cm = u.networks[ni].casemap;
    let uc = uc!(u, ni);
    if uc.regain_nick_timer.is_none() {
        return;
    }
    // Maybe the user has updated their desired nick
    if want_nick != nick || is_our_nick(cm, uc, &want_nick) {
        uc.stop_regain_nick_timer();
        return;
    }
    let delay = uc
        .regain_nick_backoff
        .as_mut()
        .map(|b| b.next_delay())
        .unwrap_or(Duration::from_secs(600));
    uc.schedule_regain_nick(delay, want_nick.clone());
    if !uc.pending_regain_nick.is_empty() {
        return;
    }
    uc.send_message(msg!("NICK", want_nick.clone()));
    uc.pending_regain_nick = want_nick;
}

/// Replies to downstreams waiting on pending commands when the upstream
/// connection goes away.
pub fn abort_pending_commands(
    dcs: &mut [DownstreamConn],
    pending: HashMap<String, Vec<PendingCommand>>,
) {
    // TODO: support labeled-response
    for l in pending.into_values() {
        for cmd in l {
            let Some(dc) = dcs.iter_mut().find(|d| d.id == cmd.downstream_id) else {
                continue;
            };
            let nick = dc.nick.clone();
            match cmd.msg.command.as_str() {
                "LIST" => dc.send_message(msg!(RPL_LISTEND, nick, "Command aborted")),
                "WHO" => {
                    let mask = cmd
                        .msg
                        .params
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "*".to_owned());
                    dc.send_message(msg!(RPL_ENDOFWHO, nick, mask, "Command aborted"));
                }
                "WHOIS" => {
                    let target = cmd.msg.params.last().cloned().unwrap_or_default();
                    dc.send_message(msg!(RPL_ENDOFWHOIS, nick, target, "Command aborted"));
                }
                "AUTHENTICATE" => {
                    dc.end_sasl(Some(msg!(
                        ERR_SASLABORTED,
                        nick,
                        "SASL authentication aborted"
                    )));
                }
                "REGISTER" | "VERIFY" => {
                    let account = cmd.msg.params.first().cloned().unwrap_or_default();
                    dc.send_message(msg!(
                        "FAIL",
                        cmd.msg.command.clone(),
                        "TEMPORARILY_UNAVAILABLE",
                        account,
                        "Command aborted"
                    ));
                }
                c => panic!("Unsupported pending command {c:?}"),
            }
        }
    }
}

fn send_next_pending_command(uc: &mut UpstreamConn, cmd: &str) {
    let Some(first) = uc.pending_cmds.get_mut(cmd).and_then(|l| l.first_mut()) else {
        return;
    };
    first.sent_at = Some(Instant::now());
    let (id, label, msg) = (
        first.downstream_id,
        first.downstream_label.clone(),
        first.msg.clone(),
    );
    uc.send_message_labeled(id, &label, msg);
}

/// Queues a command whose replies need to be routed to a downstream
/// connection. label is the downstream's label for the command.
pub fn enqueue_command(uc: &mut UpstreamConn, dc_id: u64, label: String, msg: Message) {
    match msg.command.as_str() {
        "LIST" | "WHO" | "WHOIS" | "AUTHENTICATE" | "REGISTER" | "VERIFY" => {}
        c => panic!("Unsupported pending command {c:?}"),
    }
    let cmd = msg.command.clone();
    let l = uc.pending_cmds.entry(cmd.clone()).or_default();
    l.push(PendingCommand {
        downstream_id: dc_id,
        downstream_label: label,
        msg,
        sent_at: None,
    });

    // If we didn't get a reply after a while, just give up
    // TODO: consider sending an abort reply to downstream
    let mut timed_out = 0;
    while timed_out < l.len() {
        match l[timed_out].sent_at {
            Some(t) if t.elapsed() > Duration::from_secs(30) => {}
            _ => break,
        }
        logf!(
            uc.logger,
            "timed out waiting for reply to pending command: {:?}",
            l[timed_out].msg.to_string()
        );
        timed_out += 1;
    }
    if timed_out > 0 {
        l.drain(..timed_out);
    }

    // Send the head of the queue unless it's already waiting for replies.
    // After dropping timed out commands, it may not have been sent yet.
    if l.first().is_some_and(|p| p.sent_at.is_none()) {
        send_next_pending_command(uc, &cmd);
    }
}

/// Returns the downstream ID and message of the current pending command.
fn current_pending_command(u: &mut User, ni: usize, cmd: &str) -> Option<(u64, Message)> {
    let uc = uc!(u, ni);
    let first = uc.pending_cmds.get(cmd)?.first()?;
    Some((first.downstream_id, first.msg.clone()))
}

fn dequeue_command(u: &mut User, ni: usize, cmd: &str) -> Option<(u64, Message)> {
    let current = current_pending_command(u, ni, cmd);
    let uc = uc!(u, ni);
    if let Some(l) = uc.pending_cmds.get_mut(cmd)
        && !l.is_empty()
    {
        l.remove(0);
    }
    send_next_pending_command(uc, cmd);
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_messages() {
        let e = std::io::Error::from_raw_os_error(111);
        assert_eq!(go_errno_message(&e), "connection refused");
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(go_errno_message(&e), "no such file or directory");
    }

    #[test]
    fn labels() {
        assert_eq!(parse_label("").unwrap(), (0, String::new()));
        assert_eq!(
            parse_label("sd-12-3-abc-d").unwrap(),
            (12, "abc-d".to_owned())
        );
        assert!(parse_label("sd-x-1-a").is_err());
        assert!(parse_label("xx-1-1-a").is_err());
        assert!(parse_label("sd-1").is_err());
    }

    #[test]
    fn registration_errors() {
        let e = RegistrationError(msg!(ERR_PASSWDMISMATCH, "*", "Password incorrect"));
        assert!(!e.temporary());
        assert_eq!(
            e.to_string(),
            "registration error (464): Password incorrect"
        );
        let e = RegistrationError(msg!("FAIL", "*", "ACCOUNT_REQUIRED", "Login first"));
        assert!(!e.temporary());
        assert!(RegistrationError(msg!(ERR_NICKNAMEINUSE, "*", "x", "in use")).temporary());
    }

    #[test]
    fn whox_fields() {
        let uu = UpstreamUser {
            nickname: "n".into(),
            username: "u".into(),
            ..Default::default()
        };
        assert!(uu.has_whox_fields("tnu"));
        assert!(!uu.has_whox_fields("nr"));
    }
}
