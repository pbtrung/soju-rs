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
mod connection;
mod dispatch;
mod messages;
mod registration;
#[cfg(test)]
mod server_tests;
mod users;

pub use channels::{auto_detach_timer_fired, update_channel_auto_detach};
pub use commands::{abort_pending_commands, enqueue_command};
pub use connect::{RunnerParams, go_errno_message, run_network};
pub use dispatch::handle_message;
use dispatch::{forward_message, forward_msg_by_id, need_params, parse_label};
pub use messages::{append_log, produce};
pub use registration::{finish_registration, register};
pub use users::{try_regain_nick, update_monitor};

use commands::{
    clear_ended_early, current_pending_command, current_pending_downstream, dequeue_command,
    end_command_early, take_ended_early,
};
use users::start_regain_nick_timer;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long an upstream server has to complete registration (RPL_WELCOME)
/// once connected.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(60);
/// TCP keep-alive period of upstream connections, like Go's net.Dialer.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);
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
    /// Cancelled once registered, stopping the registration timeout
    pub registered: CancellationToken,
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
    registered_token: CancellationToken,
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

#[cfg(test)]
mod tests;
