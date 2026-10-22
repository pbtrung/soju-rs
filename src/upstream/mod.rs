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

macro_rules! uc {
    ($u:expr, $ni:expr) => {
        $u.networks[$ni].conn.as_mut().unwrap()
    };
}

mod channels;
mod commands;
mod connect;
mod messages;
mod registration;
#[cfg(test)]
mod server_tests;
mod users;

pub use channels::{auto_detach_timer_fired, update_channel_auto_detach};
pub use commands::{abort_pending_commands, enqueue_command};
pub use connect::{RunnerParams, go_errno_message, run_network};
pub use messages::{append_log, produce};
pub use registration::{finish_registration, register};
pub use users::{try_regain_nick, update_monitor};

use commands::{
    clear_ended_early, current_pending_command, current_pending_downstream, dequeue_command,
    end_command_early, take_ended_early,
};
use users::start_regain_nick_timer;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const UPSTREAM_MESSAGE_DELAY: Duration = Duration::from_secs(2);
pub const UPSTREAM_MESSAGE_BURST: u32 = 10;
/// The delay after each connect command, to let the server process it, e.g.
/// for NickServ to log us in.
pub const CONNECT_COMMAND_DELAY: Duration = Duration::from_secs(5);

/// The static list of upstream capabilities always requested when
/// supported. echo-message depends on labeled-response, see
/// caps_to_request.
const PERMANENT_UPSTREAM_CAPS: &[&str] = &[
    "account-notify",
    "account-tag",
    "away-notify",
    "batch",
    "chghost",
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
    detach_timer: Option<DetachTimer>,
}

/// A running auto-detach timer. Activity in the channel only moves the
/// deadline: when the timer fires before it, it's re-armed for the rest,
/// instead of restarting a timer task on every message.
struct DetachTimer {
    handle: AbortHandle,
    fires_at: tokio::time::Instant,
    deadline: tokio::time::Instant,
}

impl UpstreamChannel {
    /// Stops the auto-detach timer, if any.
    pub fn stop_auto_detach(&mut self) {
        if let Some(t) = self.detach_timer.take() {
            t.handle.abort();
        }
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

/// A pending command ended by an early error reply, see
/// commands::end_command_early.
#[derive(Debug)]
struct EndedEarly {
    command: String,
    downstream_id: u64,
    /// The command's target (WHO mask or WHOIS nick), case-mapped
    target: String,
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
    ended_early: Option<EndedEarly>,

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
            ended_early: None,
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
                t.handle.abort();
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

    /// Starts the auto-detach timer of a channel, replacing the current one.
    fn start_detach_timer(&mut self, name: &str, deadline: tokio::time::Instant) {
        let tx = self.events.clone();
        let (net_id, conn_id) = (self.net_id, self.id);
        let Some(uch) = self.channels.get_mut(name) else {
            return;
        };
        let name = uch.name.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let _ = tx.send(Event::ChannelDetach {
                net_id,
                conn_id,
                name,
            });
        })
        .abort_handle();
        uch.stop_auto_detach();
        uch.detach_timer = Some(DetachTimer {
            handle,
            fires_at: deadline,
            deadline,
        });
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
        let request = caps_to_request(&self.caps);
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

/// The capabilities to request (or disable, prefixed with "-").
fn caps_to_request(caps: &CapRegistry) -> Vec<String> {
    let mut request: Vec<String> = PERMANENT_UPSTREAM_CAPS
        .iter()
        .filter(|c| caps.is_available(c) && !caps.is_enabled(c))
        .map(|c| c.to_string())
        .collect();

    // Without labels, echoes can't be told apart from other messages:
    // downstreams without echo-message would get their own messages back.
    // A request with a cap the server doesn't support is rejected as a
    // whole.
    let echo_message = caps.is_available("echo-message") && caps.is_available("labeled-response");
    if !caps.is_enabled("echo-message") && echo_message {
        request.push("echo-message".to_owned());
    } else if caps.is_enabled("echo-message") && !echo_message {
        request.push("-echo-message".to_owned());
    }
    request
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
    downstream_id: u64,
    downstream_label: &str,
    label: &str,
) -> Result<()> {
    let net_id = u.networks[ni].id();

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

    let handled = messages::handle_message(u, ni, msg, downstream_id, downstream_label, label)?
        || registration::handle_message(u, ni, msg, downstream_id)?
        || users::handle_message(u, ni, msg, downstream_id)?
        || channels::handle_message(u, ni, msg, downstream_id)?;
    if !handled {
        debugf!(uc!(u, ni).logger, "unhandled message: {msg}");
        forward_msg_by_id(u, net_id, downstream_id, msg);
    }
    Ok(())
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
    fn caps_requested() {
        let mut caps = CapRegistry::new();
        for c in ["batch", "echo-message", "labeled-response", "foo"] {
            caps.available.insert(c.to_owned(), String::new());
        }
        assert_eq!(
            caps_to_request(&caps),
            ["batch", "labeled-response", "echo-message"]
        );

        // echo-message requires labeled-response
        caps.del("labeled-response");
        assert_eq!(caps_to_request(&caps), ["batch"]);
        caps.set_enabled("echo-message", true);
        assert_eq!(caps_to_request(&caps), ["batch", "-echo-message"]);

        // Unsupported caps are never requested
        let mut caps = CapRegistry::new();
        caps.available
            .insert("labeled-response".to_owned(), String::new());
        assert_eq!(caps_to_request(&caps), ["labeled-response"]);
    }

    #[test]
    fn cap_ls_continuation() {
        let params = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(registration::cap_ls_continued(&params(&["*", "a b"])));
        assert!(!registration::cap_ls_continued(&params(&["a b"])));
        // The marker is looked up after the subcommand, not in the nick
        assert!(!registration::cap_ls_continued(&params(&["x", "a b"])));
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
