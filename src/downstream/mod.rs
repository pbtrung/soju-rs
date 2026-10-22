//! Downstream connections from IRC clients (port of upstream's
//! downstream.go). Command handlers for registered connections live in
//! cmds.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha512};

use crate::conn::{Conn, ConnInfo, ConnReader};
use crate::database::{self as db, get_nick, get_realname};
use crate::irc::{Message, Prefix, Tags, numerics::*};
use crate::ircutil;
use crate::logger::Logger;
use crate::metrics;
use crate::server::Server;
use crate::upstream::{UpstreamChannel, UpstreamConn};
use crate::user::User;
use crate::xirc::{self, CapRegistry, CaseMapping, CaseMappingMap, Isupport};
use crate::{debugf, logf, msg};

mod caps;
mod cmds;
mod registration;
mod sasl;
mod send;
mod sync;
mod welcome;

pub use caps::*;
pub use cmds::*;
pub use registration::*;
pub use sasl::*;
pub use sync::*;
pub use welcome::*;

pub const SERVICE_NICK: &str = "BouncerServ";
pub const SERVICE_NICK_CM: &str = "bouncerserv";
pub const SERVICE_REALNAME: &str = "soju bouncer service";

pub fn service_prefix() -> Prefix {
    Prefix::full(SERVICE_NICK, SERVICE_NICK, SERVICE_NICK)
}

pub const BACKLOG_LIMIT: usize = 4000;

/// An error replied to the client as an IRC message.
#[derive(Debug, Clone)]
pub struct IrcError(pub Message);

impl std::fmt::Display for IrcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for IrcError {}

pub fn irc_error(m: Message) -> anyhow::Error {
    IrcError(m).into()
}

pub fn unknown_command(cmd: &str) -> anyhow::Error {
    irc_error(msg!(ERR_UNKNOWNCOMMAND, "*", cmd, "Unknown command"))
}

pub fn unknown_irc_error(cmd: &str, text: &str) -> anyhow::Error {
    irc_error(msg!(ERR_UNKNOWNERROR, "*", cmd, text))
}

pub fn need_more_params(cmd: &str) -> IrcError {
    IrcError(msg!(ERR_NEEDMOREPARAMS, "*", cmd, "Not enough parameters"))
}

pub fn chathistory_error(subcommand: &str, target: &str) -> anyhow::Error {
    irc_error(msg!(
        "FAIL",
        "CHATHISTORY",
        "MESSAGE_ERROR",
        subcommand,
        target,
        "Messages could not be retrieved"
    ))
}

/// Checks that a message has at least n parameters.
pub fn need_params(msg: &Message, n: usize) -> Result<()> {
    if msg.params.len() < n {
        return Err(need_more_params(&msg.command).into());
    }
    Ok(())
}

pub fn parse_bouncer_net_id(subcommand: &str, s: &str) -> Result<i64> {
    match s.parse::<i64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(irc_error(msg!(
            "FAIL",
            "BOUNCER",
            "INVALID_NETID",
            subcommand,
            s,
            "Invalid network ID"
        ))),
    }
}

pub fn fill_network_addr_attrs(attrs: &mut Tags, network: &db::Network) {
    let Ok(u) = network.url() else { return };
    let has_host_port = match u.scheme.as_str() {
        "ircs" => {
            attrs.set("tls", "1");
            true
        }
        "irc+insecure" => {
            attrs.set("tls", "0");
            true
        }
        // e.g. unix://
        _ => false,
    };
    if !has_host_port {
        return;
    }
    match db::split_host_port(&u.host) {
        Ok((host, port)) => {
            attrs.set("host", host);
            attrs.set("port", port);
        }
        Err(_) => attrs.set("host", u.host.clone()),
    }
}

fn network_addr_from_attrs(attrs: &Tags) -> String {
    let host = attrs.get_or_empty("host");
    if host.is_empty() {
        return String::new();
    }
    let mut addr = host.to_owned();
    let port = attrs.get_or_empty("port");
    if !port.is_empty() {
        addr = format!("{addr}:{port}");
    }
    if attrs.get_or_empty("tls") == "0" {
        addr = format!("irc+insecure://{addr}");
    }
    addr
}

pub fn update_network_attrs(
    record: &mut db::Network,
    attrs: &Tags,
    subcommand: &str,
) -> Result<()> {
    let mut addr_attrs = Tags::new();
    fill_network_addr_attrs(&mut addr_attrs, record);

    let mut update_addr = false;
    for (k, v) in attrs.iter() {
        match k {
            "host" | "port" | "tls" => {
                update_addr = true;
                addr_attrs.set(k, v);
            }
            "name" => record.name = v.to_owned(),
            "nickname" => record.nick = v.to_owned(),
            "username" => record.username = v.to_owned(),
            "realname" => record.realname = v.to_owned(),
            "pass" => record.pass = v.to_owned(),
            _ => {
                return Err(irc_error(msg!(
                    "FAIL",
                    "BOUNCER",
                    "UNKNOWN_ATTRIBUTE",
                    subcommand,
                    k,
                    "Unknown attribute"
                )));
            }
        }
    }

    if update_addr {
        record.addr = network_addr_from_attrs(&addr_attrs);
        if record.addr.is_empty() {
            return Err(irc_error(msg!(
                "FAIL",
                "BOUNCER",
                "NEED_ATTRIBUTE",
                subcommand,
                "host",
                "Missing required host attribute"
            )));
        }
    }
    Ok(())
}

/// Characters forbidden in a nickname:
///
///   - ' ' and ':' break the IRC message wire format
///   - '@' and '!' break prefixes
///   - '*' breaks masks and is the reserved nickname for registration
///   - '?' breaks masks
///   - '$' breaks server masks in PRIVMSG/NOTICE
///   - ',' breaks lists
///   - '.' is reserved for server names
///
/// See https://modern.ircdocs.horse/#clients
pub const ILLEGAL_NICK_CHARS: &[char] = &[' ', ':', '@', '!', '*', '?', '$', ',', '.'];

/// Characters forbidden in a channel name, see
/// https://modern.ircdocs.horse/#channels
pub const ILLEGAL_CHAN_CHARS: &[char] = &[' ', ',', '\x07'];

/// The state of a labeled-response for the command being handled.
#[derive(Clone, Debug, Default)]
pub struct LabelContext {
    pub label: String,
    pub batch: String,
    /// The first reply, buffered until we know whether a batch is needed.
    /// A reply may span several messages, e.g. a draft/multiline batch.
    pub pending_msg: Option<Vec<Message>>,
}

#[derive(Clone, Debug, Default)]
pub struct Registration {
    pub nick: String,
    pub username: String,
    pub pass: String,
    pub network_name: String,
    pub network_id: i64,
    pub negotiating_caps: bool,
    pub auth_username: String,
    /// Whether the server of the requested new network was found reachable,
    /// see check_new_network.
    pub server_checked: bool,
}

pub struct DownstreamConn {
    pub id: u64,
    pub conn: Conn,
    pub info: ConnInfo,
    pub srv: Arc<Server>,
    pub logger: Logger,

    // These don't change after connection registration
    pub registered: bool,
    pub network_id: Option<i64>,
    pub client_name: String,
    pub impersonating: bool,

    pub nick: String,
    pub nick_cm: String,
    pub realname: String,
    pub username: String,
    pub hostname: String,
    /// RPL_LOGGEDIN/OUT state
    pub account: String,
    pub away: Option<String>,

    pub cap_version: i64,
    pub caps: CapRegistry,
    pub isupport: Isupport,
    pub sasl: Option<DownstreamSasl>,
    /// None after RPL_WELCOME
    pub registration: Option<Registration>,

    last_batch_ref: u64,
    /// draft/multiline batches being received, by reference tag
    multiline_batches: HashMap<String, xirc::MultilineBatch>,

    pub casemap: CaseMapping,
    pub monitored: CaseMappingMap<()>,
    pub metadata_subs: HashSet<String>,

    pub label: Option<LabelContext>,
}

/// Whether a message can be sent as part of a history batch.
///
/// Don't replay all messages, because that would mess up client state. For
/// instance we just sent the list of users, sending PART messages for one
/// of these users would be incorrect.
pub fn message_supports_backlog(msg: &Message) -> bool {
    matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE")
}

pub fn send_service_notice(dc: &mut DownstreamConn, text: &str) {
    let nick = dc.nick.clone();
    dc.send_message(msg!("NOTICE", nick, text).with_prefix(service_prefix()));
}

pub fn send_service_privmsg(dc: &mut DownstreamConn, text: &str) {
    let nick = dc.nick.clone();
    dc.send_message(msg!("PRIVMSG", nick, text).with_prefix(service_prefix()));
}

/// Splits a raw username into username, client name and network name:
/// "<username>[/<network>][@<client>]" in any order.
pub fn unmarshal_username(raw: &str) -> (String, String, String) {
    let mut username = raw.to_owned();
    let mut client = String::new();
    let mut network = String::new();
    let i = raw.find(['/', '@']);
    let j = raw.rfind(['/', '@']);
    if let Some(i) = i {
        username = raw[..i].to_owned();
    }
    if let Some(j) = j {
        if raw.as_bytes()[j] == b'@' {
            client = raw[j + 1..].to_owned();
        } else {
            network = raw[j + 1..].to_owned();
        }
    }
    if let (Some(i), Some(j)) = (i, j)
        && i < j
    {
        if raw.as_bytes()[i] == b'@' {
            client = raw[i + 1..j].to_owned();
        } else {
            network = raw[i + 1..j].to_owned();
        }
    }
    (username, client, network)
}

/// Common handling for all downstream messages, before the command
/// handler: labeled-response setup.
fn begin_message(dc: &mut DownstreamConn, msg: &Message) {
    dc.label = None;
    if let Some(label) = msg.tags.get("label")
        && dc.caps.is_enabled("labeled-response")
    {
        dc.label = Some(LabelContext {
            label: label.to_owned(),
            ..Default::default()
        });
    }
}

/// Sends an IRC error to the client, or propagates other errors.
pub fn finish_message(dc: &mut DownstreamConn, res: Result<()>) -> Result<()> {
    if let Err(e) = res {
        match e.downcast::<IrcError>() {
            Ok(irc_err) => {
                let mut m = irc_err.0;
                m.prefix = Some(dc.srv.prefix());
                dc.send_message(m);
            }
            Err(e) => {
                dc.label = None;
                return Err(e);
            }
        }
    }
    dc.flush_batch();
    dc.label = None;
    Ok(())
}

/// The maximum number of draft/multiline batches a client may have open at
/// once.
const MAX_MULTILINE_BATCHES: usize = 8;

/// The draft/multiline limits for a downstream connection: the ones of its
/// upstream server.
fn multiline_limits(u: &User, i: usize) -> Option<xirc::MultilineLimits> {
    let ni = dc_network(u, i)?;
    u.networks[ni].upstream()?.multiline_limits()
}

/// Handles a client-initiated batch. Only draft/multiline batches are
/// supported: once complete, they are handled as a single message.
fn handle_batch(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    let tag = msg.param(0);
    if let Some(r) = tag.strip_prefix('+') {
        let dc = &mut u.downstreams[i];
        let res = if msg.param(1) != xirc::MULTILINE_BATCH_TYPE
            || !dc.caps.is_enabled(xirc::MULTILINE_BATCH_TYPE)
        {
            let typ = msg.param(1);
            let typ = if typ.is_empty() { "*" } else { typ };
            Err(fail(&[
                "BATCH",
                "UNKNOWN_TYPE",
                typ,
                "Unsupported batch type",
            ]))
        } else if r.is_empty()
            || dc.multiline_batches.contains_key(r)
            || dc.multiline_batches.len() >= MAX_MULTILINE_BATCHES
        {
            Err(fail(&[
                "BATCH",
                "MULTILINE_INVALID",
                "Invalid multiline batch",
            ]))
        } else {
            dc.multiline_batches
                .insert(r.to_owned(), xirc::MultilineBatch::new(msg.clone()));
            return Ok(());
        };
        begin_message(dc, msg);
        return finish_message(dc, res);
    }

    let batch = tag
        .strip_prefix('-')
        .and_then(|r| u.downstreams[i].multiline_batches.remove(r));
    let Some(batch) = batch else {
        let dc = &mut u.downstreams[i];
        begin_message(dc, msg);
        let err = fail(&[
            "BATCH",
            "INVALID_REFTAG",
            tag,
            "Unknown batch reference tag",
        ]);
        return finish_message(dc, Err(err));
    };
    let opener = batch.opener.clone();
    let target = batch.target.clone();
    let limits = multiline_limits(u, i);
    let res = batch.finish().and_then(|m| {
        // We may split lines differently from the client
        let uc = dc_network(u, i).and_then(|ni| u.networks[ni].upstream());
        match (uc, limits) {
            (Some(uc), Some(limits)) if uc.multiline_line_count(&m) > limits.max_lines => {
                Err(xirc::MultilineError::MaxLines)
            }
            _ => Ok(m),
        }
    });
    match res {
        Ok(m) => handle_message_labeled(u, dc_id, &m),
        Err(e) => {
            let limits = limits.unwrap_or_default();
            let err = match e {
                xirc::MultilineError::MaxBytes => fail(&[
                    "BATCH",
                    e.code(),
                    &limits.max_bytes.to_string(),
                    "Multiline batch max-bytes exceeded",
                ]),
                xirc::MultilineError::MaxLines => fail(&[
                    "BATCH",
                    e.code(),
                    &limits.max_lines.to_string(),
                    "Multiline batch max-lines exceeded",
                ]),
                xirc::MultilineError::InvalidTarget => {
                    fail(&["BATCH", e.code(), &target, "Invalid multiline target"])
                }
                xirc::MultilineError::Invalid => {
                    fail(&["BATCH", e.code(), "Invalid multiline batch"])
                }
            };
            let dc = &mut u.downstreams[i];
            begin_message(dc, &opener);
            finish_message(dc, Err(err))
        }
    }
}

/// Handles a message from a registered downstream connection.
pub fn handle_message(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    metrics::inc(&u.srv.metrics.downstream_in_messages_total);
    let i = u.dc_index(dc_id).unwrap();
    if msg.command == "BATCH" {
        return handle_batch(u, dc_id, msg);
    }
    if let Some(r) = msg.tags.get("batch")
        && u.downstreams[i].multiline_batches.contains_key(r)
    {
        let limits = multiline_limits(u, i).unwrap_or_default();
        if let Some(b) = u.downstreams[i].multiline_batches.get_mut(r) {
            b.push(msg, &limits);
        }
        return Ok(());
    }
    handle_message_labeled(u, dc_id, msg)
}

/// Handles a message, replying with a labeled-response if requested.
fn handle_message_labeled(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    begin_message(&mut u.downstreams[i], msg);
    let res = match msg.command.as_str() {
        "QUIT" => {
            u.downstreams[i].conn.shutdown();
            // TODO: stop handling commands
            Ok(())
        }
        "ISUPPORT" => {
            if !u.downstreams[i].caps.is_enabled("draft/extended-isupport") {
                Err(unknown_command(&msg.command))
            } else {
                let isupport = build_user_isupport(u, i);
                let dc = &mut u.downstreams[i];
                send_isupport(dc, &isupport);
                dc.isupport = isupport;
                Ok(())
            }
        }
        _ => handle_message_registered(u, dc_id, msg),
    };
    let Some(i) = u.dc_index(dc_id) else {
        return Ok(());
    };
    finish_message(&mut u.downstreams[i], res)
}

/// The network a downstream connection is bound to, if any.
pub fn dc_network(u: &User, dc_idx: usize) -> Option<usize> {
    u.downstreams[dc_idx]
        .network_id
        .and_then(|id| u.net_index(id))
}

/// Returns the network index of the downstream's upstream connection, or
/// an error replied to the client.
pub fn upstream_for_command(u: &User, dc_idx: usize, cmd: &str) -> Result<usize> {
    let Some(ni) = dc_network(u, dc_idx) else {
        return Err(unknown_irc_error(
            cmd,
            "Cannot interact with channels and users on the bouncer connection. Did you mean to use a specific network?",
        ));
    };
    if u.networks[ni].upstream().is_none() {
        return Err(unknown_irc_error(cmd, "Disconnected from upstream network"));
    }
    Ok(ni)
}

/// Sends a labeled message to the upstream server of a downstream
/// connection, consuming the downstream's label.
pub fn send_upstream_labeled(u: &mut User, dc_idx: usize, ni: usize, msg: Message) {
    let label = u.downstreams[dc_idx].take_label();
    let dc_id = u.downstreams[dc_idx].id;
    if let Some(uc) = u.networks[ni].upstream_mut() {
        uc.send_message_labeled(dc_id, &label, msg);
    }
}

/// Queues a command whose replies are routed to the downstream connection.
pub fn enqueue_upstream_command(u: &mut User, dc_idx: usize, ni: usize, msg: Message) {
    let label = u.downstreams[dc_idx].take_label();
    let dc_id = u.downstreams[dc_idx].id;
    if let Some(uc) = u.networks[ni].upstream_mut() {
        crate::upstream::enqueue_command(uc, dc_id, label, msg);
    }
}

pub fn parse_nickserv_credentials(text: &str, nick: &str) -> Option<(String, String)> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 2 {
        return None;
    }
    let cmd = fields[0].to_uppercase();
    let params = &fields[1..];
    match cmd.as_str() {
        "REGISTER" => Some((nick.to_owned(), params[0].to_owned())),
        "IDENTIFY" => {
            if params.len() == 1 {
                Some((nick.to_owned(), params[0].to_owned()))
            } else {
                Some((params[0].to_owned(), params[1].to_owned()))
            }
        }
        // Upstream also reports success for other SET subcommands, with
        // empty credentials which then overwrite the saved ones
        "SET" if params.len() == 2 && params[0].eq_ignore_ascii_case("PASSWORD") => {
            Some((nick.to_owned(), params[1].to_owned()))
        }
        _ => None,
    }
}

/// Serves a downstream connection: registration, then forwarding of
/// messages to the user's event loop.
pub async fn serve(
    srv: Arc<Server>,
    conn: Conn,
    mut reader: ConnReader,
    info: ConnInfo,
    logger: Logger,
) {
    let id = srv.next_downstream_id();
    let mut dc = DownstreamConn::new(srv.clone(), conn.clone(), info, id, logger);

    if srv.is_shutting_down() {
        dc.send_message(msg!("ERROR", "Server is shutting down"));
        conn.shutdown();
        return;
    }

    let registered = tokio::time::timeout(
        crate::server::DOWNSTREAM_REGISTER_TIMEOUT,
        run_until_registered(&mut dc, &mut reader),
    )
    .await;
    match registered {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            conn.shutdown();
            return;
        }
        Ok(Err(e)) => {
            logf!(dc.logger, "{e}");
            conn.shutdown();
            return;
        }
        Err(_) => {
            let p = srv.prefix();
            dc.send_message(msg!("ERROR", "Connection registration timed out").with_prefix(p));
            conn.shutdown();
            return;
        }
    }

    let username = dc.registration.as_ref().unwrap().auth_username.clone();
    let user = match srv.get_or_create_user(&username).await {
        Ok(u) => u,
        Err(e) => {
            logf!(dc.logger, "failed to get/create user: {e}");
            dc.send_message(msg!("ERROR", "Internal server error"));
            conn.shutdown();
            return;
        }
    };

    if let Err(e) = check_new_network(&user, &mut dc).await {
        let mut m = match e.downcast::<IrcError>() {
            Ok(irc_err) => irc_err.0,
            Err(_) => msg!("ERROR", "Internal server error"),
        };
        m.prefix = Some(srv.prefix());
        dc.send_message(m);
        conn.shutdown();
        return;
    }

    let logger = dc.logger.clone();
    if !user.send(crate::user::Event::DownstreamConnected(Box::new(dc))) {
        conn.shutdown();
        return;
    }
    loop {
        match reader.read_message().await {
            Ok(Some(msg)) => {
                if !user.send(crate::user::Event::DownstreamMessage { dc_id: id, msg }) {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                logf!(logger, "failed to read IRC command: {e}");
                break;
            }
        }
    }
    user.send(crate::user::Event::DownstreamDisconnected { dc_id: id });
    conn.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        let t = |s: &str| unmarshal_username(s);
        let s = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
        assert_eq!(t("alice"), s("alice", "", ""));
        assert_eq!(t("alice/libera"), s("alice", "", "libera"));
        assert_eq!(t("alice@phone"), s("alice", "phone", ""));
        assert_eq!(t("alice/libera@phone"), s("alice", "phone", "libera"));
        assert_eq!(t("alice@phone/libera"), s("alice", "phone", "libera"));
    }

    #[test]
    fn nickserv() {
        assert_eq!(
            parse_nickserv_credentials("IDENTIFY pw", "me"),
            Some(("me".to_owned(), "pw".to_owned()))
        );
        assert_eq!(
            parse_nickserv_credentials("identify acc pw", "me"),
            Some(("acc".to_owned(), "pw".to_owned()))
        );
        assert_eq!(parse_nickserv_credentials("HELP", "me"), None);
        assert_eq!(parse_nickserv_credentials("INFO x", "me"), None);
    }

    #[test]
    fn network_attrs_roundtrip() {
        let mut n = db::Network::new("irc+insecure://irc.example.org:6667");
        let mut attrs = Tags::new();
        fill_network_addr_attrs(&mut attrs, &n);
        assert_eq!(attrs.get("tls"), Some("0"));
        assert_eq!(attrs.get("host"), Some("irc.example.org"));
        assert_eq!(attrs.get("port"), Some("6667"));
        let upd = Tags::parse("tls=1;name=ex");
        update_network_attrs(&mut n, &upd, "CHANGENETWORK").unwrap();
        assert_eq!(n.addr, "irc.example.org:6667");
        assert_eq!(n.name, "ex");
        assert!(update_network_attrs(&mut n, &Tags::parse("bogus=1"), "X").is_err());
        let mut n = db::Network::new("");
        assert!(update_network_attrs(&mut n, &Tags::parse("port=1"), "ADDNETWORK").is_err());
    }
}
