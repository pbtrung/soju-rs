//! Downstream connections from IRC clients (port of upstream's
//! downstream.go). Command handlers for registered connections live in
//! downstream_cmds.rs.

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

use crate::downstream_cmds::fail;
pub use crate::downstream_cmds::handle_message_registered;

pub const SERVICE_NICK: &str = "BouncerServ";
pub const SERVICE_NICK_CM: &str = "bouncerserv";
pub const SERVICE_REALNAME: &str = "soju bouncer service";

pub fn service_prefix() -> Prefix {
    Prefix::full(SERVICE_NICK, SERVICE_NICK, SERVICE_NICK)
}

pub const CHAT_HISTORY_LIMIT: usize = 1000;
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

/// Always-supported downstream capabilities.
const PERMANENT_DOWNSTREAM_CAPS: &[(&str, &str)] = &[
    ("batch", ""),
    ("cap-notify", ""),
    ("echo-message", ""),
    ("invite-notify", ""),
    ("no-implicit-names", ""),
    ("server-time", ""),
    ("setname", ""),
    ("draft/extended-isupport", ""),
    (
        "draft/metadata-2",
        "before-connect,max-keys=0,max-value-bytes=1",
    ),
    ("draft/no-implicit-names", ""),
    ("draft/pre-away", ""),
    ("draft/read-marker", ""),
    ("soju.im/account-required", ""),
    ("soju.im/bouncer-networks", ""),
    ("soju.im/bouncer-networks-notify", ""),
    ("soju.im/no-implicit-names", ""),
    ("soju.im/read", ""),
    ("soju.im/webpush", ""),
];

/// Downstream capabilities which require support from the upstream server
/// to be enabled.
pub const PASSTHROUGH_DOWNSTREAM_CAPS: &[(&str, &str)] = &[
    ("account-notify", ""),
    ("account-tag", ""),
    ("away-notify", ""),
    ("chghost", ""),
    ("extended-join", ""),
    ("extended-monitor", ""),
    ("labeled-response", ""),
    ("message-tags", ""),
    ("multi-prefix", ""),
    ("draft/extended-monitor", ""),
    ("draft/message-redaction", ""),
];

/// ISUPPORT tokens always passed to downstream clients.
const PERMANENT_ISUPPORT: &[&str] = &["soju.im/SAFERATE", "draft/ACCOUNTREQUIRED"];

/// ISUPPORT tokens directly passed through from the upstream server.
const PASSTHROUGH_ISUPPORT: &[&str] = &[
    "ACCOUNTEXTBAN",
    "AWAYLEN",
    "BOT",
    "CASEMAPPING",
    "CHANLIMIT",
    "CHANMODES",
    "CHANNELLEN",
    "CHANTYPES",
    "CLIENTTAGDENY",
    "ELIST",
    "EXCEPTS",
    "EXTBAN",
    "HOSTLEN",
    "INVEX",
    "KICKLEN",
    "LINELEN",
    "MAXLIST",
    "MAXTARGETS",
    "MODES",
    "MONITOR",
    "NAMELEN",
    "NETWORK",
    "NICKLEN",
    "PREFIX",
    "SAFELIST",
    "STATUSMSG",
    "TARGMAX",
    "TOPICLEN",
    "USERLEN",
    "UTF8ONLY",
    "WHOX",
    "draft/ICON",
];

/// The state of a labeled-response for the command being handled.
#[derive(Clone, Debug, Default)]
pub struct LabelContext {
    pub label: String,
    pub batch: String,
    /// The first reply, buffered until we know whether a batch is needed.
    /// A reply may span several messages, e.g. a draft/multiline batch.
    pub pending_msg: Option<Vec<Message>>,
}

#[derive(Clone, Debug)]
pub struct SaslPlain {
    pub identity: String,
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Default)]
pub struct OAuthBearerOptions {
    pub username: String,
    pub token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaslServerKind {
    Plain,
    OAuthBearer,
    Anonymous,
    External,
}

#[derive(Clone, Debug)]
pub struct DownstreamSasl {
    kind: SaslServerKind,
    pub mechanism: String,
    done: bool,
    oauth_fail: Option<String>,
    pub plain: Option<SaslPlain>,
    pub oauth_bearer: Option<OAuthBearerOptions>,
    pending_resp: String,
}

impl DownstreamSasl {
    /// Feeds a client response to the SASL server. Returns the challenge to
    /// send back, or None when the exchange is complete.
    fn next(&mut self, response: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        if self.kind == SaslServerKind::OAuthBearer
            && let Some(fail) = &self.oauth_fail
        {
            if response.is_none_or(|r| r.len() != 1 && r.first() != Some(&0x01)) {
                bail!("sasl: invalid response");
            }
            bail!("{fail}");
        }
        if self.done {
            bail!("sasl: unexpected client response");
        }
        let Some(response) = response else {
            return Ok(Some(Vec::new()));
        };
        self.done = true;
        match self.kind {
            SaslServerKind::Plain => {
                let parts: Vec<&[u8]> = response.split(|&b| b == 0).collect();
                if parts.len() != 3 {
                    bail!("sasl: invalid response");
                }
                let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
                self.plain = Some(SaslPlain {
                    identity: s(parts[0]),
                    username: s(parts[1]),
                    password: s(parts[2]),
                });
                Ok(None)
            }
            SaslServerKind::Anonymous => Ok(None),
            SaslServerKind::External => {
                if response.contains(&0) {
                    bail!("sasl: identity contains a NUL character");
                }
                Ok(None)
            }
            SaslServerKind::OAuthBearer => {
                let fail = |this: &mut DownstreamSasl, descr: &str| -> Result<Option<Vec<u8>>> {
                    this.oauth_fail = Some(format!("sasl: client error: {descr}"));
                    Ok(Some(
                        br#"{"status":"invalid_request","schemes":"bearer","scope":""}"#.to_vec(),
                    ))
                };
                let mut parts = response.splitn(3, |&b| b == b',');
                let (Some(flag), Some(authzid), Some(rest)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return fail(self, "Invalid response");
                };
                if flag != b"n" {
                    return fail(self, "Invalid response, missing 'n' in gs2-cb-flag");
                }
                let mut opts = OAuthBearerOptions::default();
                if !authzid.is_empty() {
                    let Some(user) = authzid.strip_prefix(b"a=") else {
                        return fail(self, "Invalid response, missing 'a=' in gs2-authzid");
                    };
                    opts.username = String::from_utf8_lossy(user).into_owned();
                }
                for p in rest.split(|&b| b == 0x01) {
                    if p.is_empty() {
                        continue;
                    }
                    let p = String::from_utf8_lossy(p);
                    let Some((k, v)) = p.split_once('=') else {
                        return fail(self, "Invalid response, missing '='");
                    };
                    match k {
                        "host" => {}
                        "port" => {
                            if v.parse::<u16>().is_err() {
                                return fail(self, "Invalid response, malformed 'port' value");
                            }
                        }
                        "auth" => {
                            const PREFIX: &str = "bearer ";
                            if !v.to_lowercase().starts_with(PREFIX) {
                                return fail(self, "Unsupported token type");
                            }
                            opts.token = v[PREFIX.len()..].to_owned();
                        }
                        _ => {
                            return fail(
                                self,
                                &format!("Invalid response, unknown parameter: {k}"),
                            );
                        }
                    }
                }
                self.oauth_bearer = Some(opts);
                Ok(None)
            }
        }
    }
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

impl DownstreamConn {
    pub fn new(
        srv: Arc<Server>,
        conn: Conn,
        info: ConnInfo,
        id: u64,
        logger: Logger,
    ) -> DownstreamConn {
        let cfg = srv.config();
        let cm = xirc::CASEMAPPING_ASCII;
        let mut caps = CapRegistry::new();
        for (k, v) in PERMANENT_DOWNSTREAM_CAPS {
            caps.available.insert(k.to_string(), v.to_string());
        }
        let mut mechs = Vec::new();
        if cfg.auth.plain.is_some() {
            mechs.push("PLAIN");
        }
        if cfg.auth.oauth_bearer.is_some() {
            mechs.push("OAUTHBEARER");
        }
        if cfg.basic.client_cert_auth && info.peer_cert.is_some() {
            mechs.push("EXTERNAL");
        }
        caps.available.insert("sasl".to_owned(), mechs.join(","));
        // TODO: we should only enable chathistory after authentication and
        // then check that the user's message store supports it
        match cfg.basic.msg_store.driver {
            crate::config::MsgStoreDriver::Db => {
                caps.available
                    .insert("draft/chathistory".to_owned(), String::new());
                caps.available
                    .insert("soju.im/search".to_owned(), String::new());
            }
            crate::config::MsgStoreDriver::Memory => {}
        }
        if cfg.basic.client_cert_auth {
            caps.available
                .insert("soju.im/client-cert".to_owned(), String::new());
        }

        DownstreamConn {
            id,
            conn,
            hostname: crate::conn::host_of(&info.remote_addr),
            info,
            srv,
            logger,
            registered: false,
            network_id: None,
            client_name: String::new(),
            impersonating: false,
            nick: "*".to_owned(),
            nick_cm: "*".to_owned(),
            realname: String::new(),
            username: "~u".to_owned(),
            account: String::new(),
            away: None,
            cap_version: 0,
            caps,
            isupport: Isupport::new(),
            sasl: None,
            registration: Some(Registration::default()),
            last_batch_ref: 0,
            multiline_batches: HashMap::new(),
            casemap: cm,
            monitored: CaseMappingMap::new(cm),
            metadata_subs: HashSet::new(),
            label: None,
        }
    }

    pub fn prefix(&self) -> Prefix {
        Prefix::full(
            self.nick.clone(),
            self.username.clone(),
            self.hostname.clone(),
        )
    }

    pub fn casemap(&self, s: &str) -> String {
        (self.casemap)(s)
    }

    fn raw_send(&self, msg: Message) {
        metrics::inc(&self.srv.metrics.downstream_out_messages_total);
        self.conn.send_message(msg);
    }

    /// Sends a message, adapting it to the client's capabilities and
    /// wrapping it in a labeled-response if a label is pending.
    pub fn send_message(&mut self, msg: Message) {
        let mut msg = msg;
        if !self.caps.is_enabled("message-tags") {
            if msg.command == "TAGMSG" {
                return;
            }
            let caps = &self.caps;
            msg.tags.retain(|name, _| match name {
                "time" => caps.is_enabled("server-time"),
                "account" => caps.is_enabled("account-tag"),
                "batch" => caps.is_enabled("batch"),
                _ => false,
            });
        }
        if !self.caps.is_enabled("batch") && !msg.tags.get_or_empty("batch").is_empty() {
            msg.tags.remove("batch");
        }
        if msg.command == "JOIN" && !self.caps.is_enabled("extended-join") {
            msg.params.truncate(1);
        }
        let required_cap = match msg.command.as_str() {
            "SETNAME" => Some("setname"),
            "CHGHOST" => Some("chghost"),
            "AWAY" => Some("away-notify"),
            "ACCOUNT" => Some("account-notify"),
            "MARKREAD" => Some("draft/read-marker"),
            "READ" => Some("soju.im/read"),
            "METADATA" => Some("draft/metadata-2"),
            "REDACT" => Some("draft/message-redaction"),
            _ => None,
        };
        if let Some(cap) = required_cap
            && !self.caps.is_enabled(cap)
        {
            return;
        }
        if msg.prefix.as_ref().is_some_and(|p| p.name == "*") {
            // We use "*" as a sentinel value to simplify upstream message
            // handling
            msg.prefix = None;
        }
        if msg.prefix.as_ref().is_none_or(|p| p.name.is_empty())
            && ircutil::is_numeric(&msg.command)
        {
            // Numerics must always carry a source
            msg.prefix = Some(self.srv.prefix());
        }

        let multiline =
            self.caps.is_enabled("batch") && self.caps.is_enabled(xirc::MULTILINE_BATCH_TYPE);
        let prefix_len = msg.prefix.as_ref().map(|p| p.formatted_len());
        let srv = &self.srv;
        let last_batch_ref = &mut self.last_batch_ref;
        let prepared = xirc::prepare_text_message(
            msg,
            multiline,
            || prefix_len.unwrap_or_else(|| srv.prefix().formatted_len()),
            || {
                *last_batch_ref += 1;
                last_batch_ref.to_string()
            },
        );
        match prepared {
            xirc::TextMessage::Single(m) => self.send_reply(vec![m]),
            // A multiline batch is a single reply
            xirc::TextMessage::Batch(l) => self.send_reply(l),
            xirc::TextMessage::Lines(l) => {
                for m in l {
                    self.send_reply(vec![m]);
                }
            }
        }
    }

    /// Sends a reply made of one or more messages. The reply is wrapped in
    /// a labeled-response batch if needed: a multi-message reply (e.g. a
    /// draft/multiline batch) counts as a single reply, its first message
    /// carries the label or batch tag.
    fn send_reply(&mut self, mut reply: Vec<Message>) {
        let Some(label) = self.label.as_mut().filter(|l| !l.label.is_empty()) else {
            for m in reply {
                self.raw_send(m);
            }
            return;
        };
        if let Some(mut pending) = label.pending_msg.take() {
            // create a batch
            self.last_batch_ref += 1;
            let batch = self.last_batch_ref.to_string();
            let label_str = label.label.clone();
            label.batch = batch.clone();
            if pending[0].tags.get_or_empty("batch").is_empty() {
                pending[0].tags.set("batch", batch.clone());
            }
            self.raw_send(
                msg!("BATCH", format!("+{batch}"), "labeled-response").with_tag("label", label_str),
            );
            // send the buffered reply
            for m in pending {
                self.raw_send(m);
            }
        }
        let label = self.label.as_mut().unwrap();
        if !label.batch.is_empty() {
            // send the current reply in the batch
            if reply[0].tags.get_or_empty("batch").is_empty() {
                reply[0].tags.set("batch", label.batch.clone());
            }
            for m in reply {
                self.raw_send(m);
            }
        } else {
            // first reply we're sending: buffer it
            label.pending_msg = Some(reply);
        }
    }

    /// Ends a labeled-response: sends the buffered message, closes the
    /// batch or sends an ACK.
    pub fn flush_batch(&mut self) {
        let Some(label) = self.label.as_mut().filter(|l| !l.label.is_empty()) else {
            return;
        };
        let label_str = label.label.clone();
        if let Some(mut pending) = label.pending_msg.take() {
            pending[0].tags.set("label", label_str);
            for m in pending {
                self.raw_send(m);
            }
        } else if !label.batch.is_empty() {
            let batch = label.batch.clone();
            self.raw_send(msg!("BATCH", format!("-{batch}")));
        } else {
            self.raw_send(msg!("ACK").with_tag("label", label_str));
        }
    }

    /// Takes the label of the current command, so that the upstream
    /// server's labeled response can be routed back.
    pub fn take_label(&mut self) -> String {
        match &mut self.label {
            Some(l) => std::mem::take(&mut l.label),
            None => String::new(),
        }
    }

    /// Drops the label of the current command, for commands mixing local
    /// and remote replies.
    pub fn drop_label(&mut self) {
        if let Some(l) = &mut self.label {
            l.label.clear();
        }
    }

    /// Starts a batch if the client supports it, returning its reference.
    pub fn begin_batch(&mut self, typ: &str, params: &[String], tags: Tags) -> String {
        self.last_batch_ref += 1;
        let r = self.last_batch_ref.to_string();
        if self.caps.is_enabled("batch") {
            let mut p = vec![format!("+{r}"), typ.to_owned()];
            p.extend_from_slice(params);
            self.send_message(Message::new("BATCH", p).with_tags(tags));
        }
        r
    }

    pub fn end_batch(&mut self, r: &str) {
        if self.caps.is_enabled("batch") {
            self.send_message(msg!("BATCH", format!("-{r}")));
        }
    }

    /// Sends messages in a batch, tagging each with the batch reference.
    pub fn send_batch(&mut self, typ: &str, params: &[String], msgs: Vec<Message>) {
        let r = self.begin_batch(typ, params, Tags::new());
        for mut m in msgs {
            m.tags.set("batch", r.clone());
            self.send_message(m);
        }
        self.end_batch(&r);
    }

    /// Sends a message with an internal ID, so that clients without
    /// chathistory acknowledge it via PING/PONG.
    pub fn send_message_with_id(&mut self, msg: Message, id: &str) {
        let supports_backlog = message_supports_backlog(&msg);
        self.send_message(msg);
        if id.is_empty() || !supports_backlog || self.caps.is_enabled("draft/chathistory") {
            return;
        }
        self.send_ping(id);
    }

    /// Advances history to a message ID without sending the message, e.g.
    /// for self-messages when echo-message isn't enabled.
    pub fn advance_message_with_id(&mut self, msg: &Message, id: &str) {
        if id.is_empty()
            || !message_supports_backlog(msg)
            || self.caps.is_enabled("draft/chathistory")
        {
            return;
        }
        self.send_ping(id);
    }

    fn send_ping(&mut self, msg_id: &str) {
        self.send_message(msg!("PING", format!("soju-msgid-{msg_id}")));
    }

    pub fn end_sasl(&mut self, msg: Option<Message>) {
        if self.sasl.is_none() {
            return;
        }
        self.sasl = None;
        let m = msg.unwrap_or_else(|| {
            msg!(
                RPL_SASLSUCCESS,
                self.nick.clone(),
                "SASL authentication successful"
            )
        });
        self.send_message(m);
    }

    pub fn set_supported_cap(&mut self, name: &str, value: &str) {
        let changed = self.caps.available.get(name).is_none_or(|v| v != value);
        self.caps
            .available
            .insert(name.to_owned(), value.to_owned());
        if !self.caps.is_enabled("cap-notify") || !changed {
            return;
        }
        let cap = if !value.is_empty() && self.cap_version >= 302 {
            format!("{name}={value}")
        } else {
            name.to_owned()
        };
        let p = self.srv.prefix();
        let nick = self.nick.clone();
        self.send_message(msg!("CAP", nick, "NEW", cap).with_prefix(p));
    }

    pub fn unset_supported_cap(&mut self, name: &str) {
        let had = self.caps.is_available(name);
        self.caps.del(name);
        if !self.caps.is_enabled("cap-notify") || !had {
            return;
        }
        let p = self.srv.prefix();
        let nick = self.nick.clone();
        self.send_message(msg!("CAP", nick, "DEL", name).with_prefix(p));
    }

    pub fn update_casemapping(&mut self, cm: CaseMapping) {
        self.casemap = cm;
        self.nick_cm = cm(&self.nick);
        self.monitored.set_casemapping(cm);
    }

    /// Sends a CAP LS or LIST reply. With CAP 302, it's split over several
    /// lines if needed, all but the last marked with "*", so that each line
    /// fits in an IRC message.
    fn send_cap_list(&mut self, subcmd: &str, caps: &[String], prefix: Prefix) {
        // Room for ":<prefix> CAP <nick> <subcmd> * :" in 512 bytes
        let max = xirc::MAX_MESSAGE_LENGTH
            - 2
            - (prefix.formatted_len() + 2)
            - (self.nick.len() + subcmd.len() + 10);
        let mut lines: Vec<String> = vec![String::new()];
        for cap in caps {
            let line = lines.last_mut().unwrap();
            if self.cap_version >= 302 && !line.is_empty() && line.len() + 1 + cap.len() > max {
                lines.push(cap.clone());
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(cap);
            }
        }
        let n = lines.len();
        for (i, line) in lines.into_iter().enumerate() {
            let mut params = vec![self.nick.clone(), subcmd.to_owned()];
            if i + 1 < n {
                params.push("*".to_owned());
            }
            params.push(line);
            self.send_message(Message::new("CAP", params).with_prefix(prefix.clone()));
        }
    }

    pub fn handle_cap(&mut self, msg: &Message) -> Result<()> {
        need_params(msg, 1)?;
        let cmd = msg.params[0].to_uppercase();
        let args = &msg.params[1..];
        let p = self.srv.prefix();
        match cmd.as_str() {
            "LS" => {
                if let Some(v) = args.first() {
                    self.cap_version = v
                        .parse()
                        .map_err(|_| anyhow!("strconv.Atoi: parsing {v:?}: invalid syntax"))?;
                }
                if !self.registered && self.cap_version >= 302 {
                    // Let downstream show everything it supports, and trim
                    // down the available capabilities when upstreams are
                    // known.
                    for (k, v) in PASSTHROUGH_DOWNSTREAM_CAPS {
                        self.caps.available.insert(k.to_string(), v.to_string());
                    }
                    self.caps.available.insert(
                        xirc::MULTILINE_BATCH_TYPE.to_owned(),
                        xirc::MultilineLimits::default().cap_value(),
                    );
                }
                let caps: Vec<String> = self
                    .caps
                    .available
                    .iter()
                    .map(|(k, v)| {
                        if self.cap_version >= 302 && !v.is_empty() {
                            format!("{k}={v}")
                        } else {
                            k.clone()
                        }
                    })
                    .collect();
                self.send_cap_list("LS", &caps, p);
                if self.cap_version >= 302 {
                    // CAP version 302 implicitly enables cap-notify
                    self.caps.set_enabled("cap-notify", true);
                }
                if let Some(r) = &mut self.registration
                    && !self.registered
                {
                    r.negotiating_caps = true;
                }
            }
            "LIST" => {
                let caps: Vec<String> = self.caps.enabled.iter().cloned().collect();
                self.send_cap_list("LIST", &caps, p);
            }
            "REQ" => {
                let Some(arg) = args.first() else {
                    return Err(irc_error(msg!(
                        ERR_INVALIDCAPCMD,
                        self.nick.clone(),
                        cmd,
                        "Missing argument in CAP REQ command"
                    )));
                };
                let mut ack = true;
                let mut changes = Vec::new();
                for name in arg.split_whitespace() {
                    let name = name.to_lowercase();
                    let (name, enable) = match name.strip_prefix('-') {
                        Some(n) => (n.to_owned(), false),
                        None => (name, true),
                    };
                    if enable == self.caps.is_enabled(&name) {
                        continue;
                    }
                    if !self.caps.is_available(&name) {
                        ack = false;
                        break;
                    }
                    if name == "cap-notify" && self.cap_version >= 302 && !enable {
                        // cap-notify cannot be disabled with CAP version 302
                        ack = false;
                        break;
                    }
                    if name == "soju.im/account-required" {
                        // account-required is an informational cap
                        ack = false;
                        break;
                    }
                    changes.push((name, enable));
                }
                // Atomically ack the whole capability set
                if ack {
                    for (name, enable) in changes {
                        self.caps.set_enabled(&name, enable);
                    }
                }
                let reply = if ack { "ACK" } else { "NAK" };
                let nick = self.nick.clone();
                self.send_message(msg!("CAP", nick, reply, arg.clone()).with_prefix(p));
                if let Some(r) = &mut self.registration
                    && !self.registered
                {
                    r.negotiating_caps = true;
                }
            }
            "END" => {
                if let Some(r) = &mut self.registration
                    && !self.registered
                {
                    r.negotiating_caps = false;
                }
            }
            _ => {
                return Err(irc_error(msg!(
                    ERR_INVALIDCAPCMD,
                    self.nick.clone(),
                    cmd,
                    "Unknown CAP command"
                )));
            }
        }
        Ok(())
    }

    /// Handles an AUTHENTICATE message. Returns the completed SASL state
    /// once the client has sent its credentials.
    pub fn handle_authenticate(&mut self, msg: &Message) -> Result<Option<DownstreamSasl>> {
        let res = self.handle_authenticate_inner(msg);
        if res.is_err() {
            self.sasl = None;
        }
        res
    }

    fn handle_authenticate_inner(&mut self, msg: &Message) -> Result<Option<DownstreamSasl>> {
        let nick = self.nick.clone();
        if !self.caps.is_enabled("sasl") {
            return Err(irc_error(msg!(
                ERR_SASLFAIL,
                nick,
                "AUTHENTICATE requires the \"sasl\" capability to be enabled"
            )));
        }
        let Some(param) = msg.params.first() else {
            return Err(irc_error(msg!(
                ERR_SASLFAIL,
                nick,
                "Missing AUTHENTICATE argument"
            )));
        };
        if param == "*" {
            return Err(irc_error(msg!(
                ERR_SASLABORTED,
                nick,
                "SASL authentication aborted"
            )));
        }

        let mut resp: Option<Vec<u8>> = None;
        match &mut self.sasl {
            None => {
                let mech = param.to_uppercase();
                let kind = match mech.as_str() {
                    "PLAIN" => SaslServerKind::Plain,
                    "OAUTHBEARER" => SaslServerKind::OAuthBearer,
                    "ANONYMOUS" => SaslServerKind::Anonymous,
                    "EXTERNAL" => SaslServerKind::External,
                    _ => {
                        return Err(irc_error(msg!(
                            ERR_SASLFAIL,
                            nick,
                            "Unsupported SASL mechanism"
                        )));
                    }
                };
                self.sasl = Some(DownstreamSasl {
                    kind,
                    mechanism: mech,
                    done: false,
                    oauth_fail: None,
                    plain: None,
                    oauth_bearer: None,
                    pending_resp: String::new(),
                });
            }
            Some(sasl) => {
                let chunk = if param == "+" { "" } else { param.as_str() };
                if sasl.pending_resp.len() + chunk.len() > 10 * 1024 {
                    return Err(irc_error(msg!(ERR_SASLFAIL, nick, "Response too long")));
                }
                sasl.pending_resp.push_str(chunk);
                if chunk.len() == xirc::MAX_SASL_LENGTH {
                    return Ok(None); // Multi-line response, wait for the next command
                }
                let decoded = BASE64.decode(&sasl.pending_resp).map_err(|_| {
                    irc_error(msg!(
                        ERR_SASLFAIL,
                        nick.clone(),
                        "Invalid base64-encoded response"
                    ))
                })?;
                sasl.pending_resp.clear();
                resp = Some(decoded);
            }
        }

        let sasl = self.sasl.as_mut().unwrap();
        match sasl.next(resp.as_deref())? {
            None => Ok(Some(sasl.clone())),
            Some(challenge) => {
                let s = if challenge.is_empty() {
                    "+".to_owned()
                } else {
                    BASE64.encode(&challenge)
                };
                // TODO: multi-line messages
                self.send_message(msg!("AUTHENTICATE", s));
                Ok(None)
            }
        }
    }

    fn set_auth_username(&mut self, username: &str, client: &str, network: &str) {
        self.client_name = client.to_owned();
        let r = self.registration.as_mut().unwrap();
        r.auth_username = username.to_owned();
        r.network_name = network.to_owned();
    }
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

/// Context for building ISUPPORT tokens.
pub struct IsupportCtx<'a> {
    pub has_user: bool,
    pub network_id: Option<i64>,
    pub chathistory: bool,
    pub upstream: Option<&'a UpstreamConn>,
}

/// The ISUPPORT tokens of a registered downstream connection.
fn build_user_isupport(u: &User, i: usize) -> Isupport {
    let dc = &u.downstreams[i];
    let ctx = IsupportCtx {
        has_user: true,
        network_id: dc.network_id,
        chathistory: u.has_chathistory_store(),
        upstream: dc_network(u, i).and_then(|ni| u.networks[ni].upstream()),
    };
    build_isupport(dc, &ctx)
}

pub fn build_isupport(dc: &DownstreamConn, ctx: &IsupportCtx) -> Isupport {
    let cfg = dc.srv.config();
    let mut isupport = Isupport::new();
    for k in PERMANENT_ISUPPORT {
        isupport.insert(k.to_string(), None);
    }
    if let Some(id) = ctx.network_id {
        isupport.insert("BOUNCER_NETID".to_owned(), Some(id.to_string()));
    }
    if ctx.network_id.is_none() && !cfg.basic.title.is_empty() {
        isupport.insert("NETWORK".to_owned(), Some(cfg.basic.title.clone()));
    }
    if ctx.network_id.is_none() && !cfg.icon_url.is_empty() {
        isupport.insert("draft/ICON".to_owned(), Some(cfg.icon_url.clone()));
    } else if ctx.network_id.is_none() && !cfg.icon_path.is_empty() {
        isupport.insert(
            "draft/ICON".to_owned(),
            Some(format!("{}/icon", cfg.basic.http_ingress)),
        );
    }
    if ctx.has_user {
        if ctx.network_id.is_none() {
            isupport.insert("BOT".to_owned(), Some("B".to_owned()));
            isupport.insert("CASEMAPPING".to_owned(), Some("ascii".to_owned()));
            isupport.insert("WHOX".to_owned(), None);
            // channels are not supported
            isupport.insert("CHANTYPES".to_owned(), Some(String::new()));
            // default bufio.Reader size
            isupport.insert("LINELEN".to_owned(), Some("4096".to_owned()));
        }
        if ctx.chathistory && ctx.network_id.is_some() {
            isupport.insert(
                "CHATHISTORY".to_owned(),
                Some(CHAT_HISTORY_LIMIT.to_string()),
            );
            isupport.insert("MSGREFTYPES".to_owned(), Some("timestamp".to_owned()));
        }
        if dc.caps.is_enabled("soju.im/webpush")
            && let Some(wp) = dc.srv.web_push()
        {
            isupport.insert("VAPID".to_owned(), Some(wp.vapid_public.clone()));
        }
    }

    if let Some(uc) = ctx.upstream {
        // If upstream doesn't support message-tags, indicate that we'll drop
        // all of them
        if !uc.isupport.contains_key("CLIENTTAGDENY") && !uc.caps.is_enabled("message-tags") {
            isupport.insert("CLIENTTAGDENY".to_owned(), Some("*".to_owned()));
        }
        for k in PASSTHROUGH_ISUPPORT {
            if let Some(v) = uc.isupport.get(*k) {
                isupport.insert(k.to_string(), v.clone());
            }
        }
    }
    isupport
}

pub fn send_isupport(dc: &mut DownstreamConn, isupport: &Isupport) {
    let msgs = xirc::generate_isupport(isupport);
    if dc.caps.is_enabled("draft/extended-isupport") {
        dc.send_batch("draft/isupport", &[], msgs);
    } else {
        for m in msgs {
            dc.send_message(m);
        }
    }
}

/// Reads and handles messages until the connection is registered. Returns
/// an error (or Ok(false) when the client quit) if registration failed.
pub async fn run_until_registered(
    dc: &mut DownstreamConn,
    reader: &mut ConnReader,
) -> Result<bool> {
    loop {
        let msg = match reader.read_message().await {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(false),
            Err(e) => bail!("failed to read IRC command: {e}"),
        };
        metrics::inc(&dc.srv.metrics.downstream_in_messages_total);

        let msg_str = ircutil::redact_for_log(&msg);
        if let Err(e) = handle_message_unregistered(dc, msg).await {
            bail!("failed to handle IRC command {msg_str:?}: {e}");
        }
        if dc.conn.is_closed() {
            return Ok(false);
        }

        let r = dc.registration.as_ref().unwrap();
        if r.nick.is_empty() || r.username.is_empty() || r.negotiating_caps {
            continue;
        }

        match register(dc).await {
            Ok(()) => return Ok(true),
            Err(e) => match e.downcast::<IrcError>() {
                Ok(irc_err) => {
                    let mut m = irc_err.0;
                    m.prefix = Some(dc.srv.prefix());
                    dc.send_message(m);
                    return Ok(false);
                }
                Err(e) => bail!("connection registration failed: {e}"),
            },
        }
    }
}

async fn handle_message_unregistered(dc: &mut DownstreamConn, msg: Message) -> Result<()> {
    begin_message(dc, &msg);
    let res = match msg.command.as_str() {
        "QUIT" => {
            dc.conn.shutdown();
            Ok(())
        }
        "ISUPPORT" => {
            if !dc.caps.is_enabled("draft/extended-isupport") {
                Err(unknown_command(&msg.command))
            } else {
                let ctx = IsupportCtx {
                    has_user: false,
                    network_id: None,
                    chathistory: false,
                    upstream: None,
                };
                let isupport = build_isupport(dc, &ctx);
                send_isupport(dc, &isupport);
                dc.isupport = isupport;
                Ok(())
            }
        }
        _ => handle_unregistered_command(dc, &msg).await,
    };
    finish_message(dc, res)
}

async fn handle_unregistered_command(dc: &mut DownstreamConn, msg: &Message) -> Result<()> {
    match msg.command.as_str() {
        "NICK" => {
            need_params(msg, 1)?;
            dc.registration.as_mut().unwrap().nick = msg.params[0].clone();
        }
        "USER" => {
            need_params(msg, 4)?;
            dc.registration.as_mut().unwrap().username = msg.params[0].clone();
        }
        "PASS" => {
            need_params(msg, 1)?;
            dc.registration.as_mut().unwrap().pass = msg.params[0].clone();
        }
        "CAP" => return dc.handle_cap(msg),
        "AUTHENTICATE" => {
            let Some(credentials) = dc.handle_authenticate(msg)? else {
                return Ok(());
            };
            match authenticate(dc, &credentials).await {
                Err(e) => {
                    logf!(
                        dc.logger,
                        "SASL {} authentication error for nick {:?}: {e}",
                        credentials.mechanism,
                        dc.registration.as_ref().unwrap().nick
                    );
                    let nick = dc.nick.clone();
                    dc.end_sasl(Some(msg!(
                        ERR_SASLFAIL,
                        nick,
                        crate::auth::auth_error_reason(&e)
                    )));
                }
                Ok((username, client, network, impersonating)) => {
                    assert!(
                        !username.is_empty(),
                        "username unset after SASL authentication"
                    );
                    dc.set_auth_username(&username, &client, &network);
                    dc.impersonating = impersonating;
                    // Technically we should send RPL_LOGGEDIN here. However
                    // we use RPL_LOGGEDIN to mirror the upstream connection
                    // status. See:
                    // https://github.com/ircv3/ircv3-specifications/pull/476
                    dc.end_sasl(None);
                }
            }
        }
        "BOUNCER" => {
            need_params(msg, 1)?;
            let subcommand = msg.params[0].clone();
            match subcommand.to_uppercase().as_str() {
                "BIND" => {
                    need_params(msg, 2)?;
                    if dc.registration.as_ref().unwrap().auth_username.is_empty() {
                        return Err(irc_error(msg!(
                            "FAIL",
                            "BOUNCER",
                            "ACCOUNT_REQUIRED",
                            "BIND",
                            "Authentication needed to bind to bouncer network"
                        )));
                    }
                    let id = parse_bouncer_net_id(&subcommand, &msg.params[1])?;
                    dc.registration.as_mut().unwrap().network_id = id;
                }
                _ => {
                    return Err(irc_error(msg!(
                        "FAIL",
                        "BOUNCER",
                        "UNKNOWN_COMMAND",
                        subcommand,
                        "Unknown subcommand"
                    )));
                }
            }
        }
        "AWAY" => {
            dc.away = msg.params.first().cloned();
            let reply = ircutil::generate_away_reply(dc.away.is_some());
            dc.send_message(reply);
        }
        "METADATA" => {
            need_params(msg, 2)?;
            let subcommand = msg.params[1].clone();
            if let Some(res) = handle_metadata_sub(dc, msg, None) {
                return res;
            }
            return Err(irc_error(msg!(
                "FAIL",
                "METADATA",
                "INVALID_PARAMS",
                subcommand,
                "Unknown command"
            )));
        }
        _ => {
            debugf!(dc.logger, "unhandled message: {msg}");
            return Err(unknown_command(&msg.command));
        }
    }
    Ok(())
}

/// Checks SASL credentials. Returns the username, client name, network name
/// and whether the user is impersonated.
async fn authenticate(
    dc: &mut DownstreamConn,
    credentials: &DownstreamSasl,
) -> Result<(String, String, String, bool)> {
    let srv = dc.srv.clone();
    let cfg = srv.config();
    let remote_addr = dc.info.remote_addr.clone();
    match credentials.mechanism.as_str() {
        "PLAIN" => {
            let plain = credentials.plain.as_ref().unwrap();
            let (mut username, mut client, mut network) = unmarshal_username(&plain.username);
            let Some(auth) = &cfg.auth.plain else {
                bail!("SASL PLAIN not supported");
            };
            auth.auth_plain(&srv.db, &username, &plain.password, &remote_addr)
                .await
                .map_err(|e| wrap_auth_err(e, &username))?;
            let mut impersonating = false;
            if !plain.identity.is_empty() && plain.identity != plain.username {
                let name = username.clone();
                let u = db::run(&srv.db, move |db| db.get_user(&name))
                    .await?
                    .ok_or_else(|| anyhow!("sql: no rows in result set (username {username:?})"))?;
                if !u.admin {
                    bail!("SASL impersonation only allowed for admin users");
                }
                (username, client, network) = unmarshal_username(&plain.identity);
                let name = username.clone();
                if db::run(&srv.db, move |db| db.get_user(&name))
                    .await?
                    .is_none()
                {
                    bail!("sql: no rows in result set (username {username:?})");
                }
                impersonating = true;
            }
            Ok((username, client, network, impersonating))
        }
        "OAUTHBEARER" => {
            let Some(auth) = &cfg.auth.oauth_bearer else {
                bail!("SASL OAUTHBEARER not supported");
            };
            let opts = credentials.oauth_bearer.clone().unwrap_or_default();
            let username = auth.auth_oauth_bearer(&opts.token, &remote_addr).await?;
            if !opts.username.is_empty() && opts.username != username {
                bail!(
                    "username mismatch (client provided {:?}, but server returned {username:?})",
                    opts.username
                );
            }
            Ok((username, String::new(), String::new(), false))
        }
        "EXTERNAL" => {
            if !cfg.basic.client_cert_auth {
                bail!("SASL EXTERNAL not supported");
            }
            let Some(cert) = &dc.info.peer_cert else {
                bail!("TLS client certificate not sent");
            };
            let h = Sha512::digest(cert);
            let Some((user_id, mut device)) =
                db::run(&srv.db, move |db| db.get_device_certificate(&h)).await?
            else {
                bail!("Unknown TLS client certificate");
            };
            let username = db::run(&srv.db, move |db| db.get_username_by_id(user_id))
                .await?
                .ok_or_else(|| anyhow!("sql: no rows in result set"))?;
            device.mark_used(dc.info.remote_ip.map(|ip| std::net::SocketAddr::new(ip, 0)));
            db::run(&srv.db, move |db| {
                db.store_device_certificate(user_id, &mut device)
            })
            .await?;
            Ok((username, String::new(), String::new(), false))
        }
        _ => bail!("unsupported SASL mechanism"),
    }
}

/// Adds the username to an authentication error. Like upstream (which
/// formats the error with %v), this drops the user-facing message, so the
/// client gets a generic "Authentication failed".
fn wrap_auth_err(e: anyhow::Error, username: &str) -> anyhow::Error {
    anyhow!("{e} (username {username:?})")
}

async fn register(dc: &mut DownstreamConn) -> Result<()> {
    assert!(!dc.registered, "tried to register twice");

    if dc.sasl.is_some() {
        let nick = dc.nick.clone();
        dc.end_sasl(Some(msg!(
            ERR_SASLABORTED,
            nick,
            "SASL authentication aborted"
        )));
    }

    let srv = dc.srv.clone();
    let r = dc.registration.as_mut().unwrap();
    let password = std::mem::take(&mut r.pass);
    let reg_username = r.username.clone();
    if r.auth_username.is_empty() {
        let nick = dc.nick.clone();
        if password.is_empty() {
            if dc.caps.is_enabled("sasl") {
                return Err(irc_error(msg!(
                    "FAIL",
                    "*",
                    "ACCOUNT_REQUIRED",
                    "Authentication required"
                )));
            } else {
                return Err(irc_error(msg!(
                    ERR_PASSWDMISMATCH,
                    nick,
                    "Authentication required"
                )));
            }
        }
        let cfg = srv.config();
        let Some(plain_auth) = &cfg.auth.plain else {
            return Err(irc_error(msg!(
                ERR_PASSWDMISMATCH,
                nick,
                "PASS authentication disabled"
            )));
        };
        let (username, client, network) = unmarshal_username(&reg_username);
        if let Err(e) = plain_auth
            .auth_plain(&srv.db, &username, &password, &dc.info.remote_addr)
            .await
        {
            logf!(
                dc.logger,
                "PASS authentication error for user {reg_username:?}: {e}"
            );
            return Err(irc_error(msg!(
                ERR_PASSWDMISMATCH,
                nick,
                crate::auth::auth_error_reason(&e)
            )));
        }
        dc.set_auth_username(&username, &client, &network);
    }

    let (_, fallback_client, fallback_network) = unmarshal_username(&reg_username);
    if dc.client_name.is_empty() {
        dc.client_name = fallback_client;
    } else if !fallback_client.is_empty() && dc.client_name != fallback_client {
        return Err(irc_error(msg!(
            ERR_ERRONEUSNICKNAME,
            dc.nick.clone(),
            "Client name mismatch in usernames"
        )));
    }

    let r = dc.registration.as_mut().unwrap();
    if r.network_name.is_empty() {
        r.network_name = fallback_network;
    } else if !fallback_network.is_empty() && r.network_name != fallback_network {
        return Err(irc_error(msg!(
            ERR_ERRONEUSNICKNAME,
            dc.nick.clone(),
            "Network name mismatch in usernames"
        )));
    }

    dc.registered = true;
    dc.username = r.auth_username.clone();
    logf!(
        dc.logger,
        "registration complete for user {:?}",
        dc.username
    );
    Ok(())
}

/// Checks that a TLS server is reachable, before auto-creating a network.
async fn sanity_check_server(addr: &str) -> Result<()> {
    let host = crate::conn::host_of(addr);
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| anyhow!("i/o timeout"))??;
    let cfg = crate::tls::client_config("", None)?;
    let name = rustls::pki_types::ServerName::try_from(host)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio_rustls::TlsConnector::from(cfg).connect(name, tcp),
    )
    .await
    .map_err(|_| anyhow!("i/o timeout"))??;
    Ok(())
}

/// Checks that the server of a new network requested by a connection is
/// reachable, before load_network auto-creates the network. This runs
/// before the connection is handed to its user, as it may take a while:
/// the user's event loop must not wait on it.
async fn check_new_network(user: &crate::user::UserHandle, dc: &mut DownstreamConn) -> Result<()> {
    let r = dc.registration.as_mut().unwrap();
    if r.network_id != 0 || r.network_name.is_empty() || r.network_name == "*" {
        return Ok(());
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let name = r.network_name.clone();
    if !user.send(crate::user::Event::HasNetwork { name, reply: tx }) {
        return Ok(());
    }
    if rx.await.unwrap_or(true) {
        return Ok(());
    }
    let mut addr = r.network_name.clone();
    if !addr.contains(':') {
        addr.push_str(":6697");
    }
    logf!(dc.logger, "trying to connect to new network {addr:?}");
    if let Err(e) = sanity_check_server(&addr).await {
        logf!(dc.logger, "failed to connect to {addr:?}: {e}");
        return Err(irc_error(msg!(
            ERR_PASSWDMISMATCH,
            dc.nick.clone(),
            format!("Failed to connect to {:?}", r.network_name)
        )));
    }
    r.server_checked = true;
    Ok(())
}

fn load_network(u: &mut User, dc_id: u64) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    let dc = &u.downstreams[i];
    let r = dc.registration.clone().unwrap();
    let nick = dc.nick.clone();
    if r.network_id != 0 {
        if u.net_index(r.network_id).is_none() {
            return Err(irc_error(msg!(
                "FAIL",
                "BOUNCER",
                "INVALID_NETID",
                r.network_id.to_string(),
                "Unknown network ID"
            )));
        }
        u.downstreams[i].network_id = Some(r.network_id);
        return Ok(());
    }

    if r.network_name == "*" {
        return Err(irc_error(msg!(
            ERR_PASSWDMISMATCH,
            nick,
            "Multi-upstream mode is no longer supported"
        )));
    }
    if r.network_name.is_empty() {
        return Ok(());
    }

    let ni = match u.get_network(&r.network_name) {
        Some(ni) => ni,
        None => {
            // The network was deleted since check_new_network
            if !r.server_checked {
                return Err(irc_error(msg!(
                    ERR_PASSWDMISMATCH,
                    nick,
                    format!("Failed to connect to {:?}", r.network_name)
                )));
            }

            // Some clients only allow specifying the nickname (and use the
            // nickname as a username too). Strip the network name from the
            // nickname when auto-saving networks.
            let (net_nick, _, _) = unmarshal_username(&r.nick);
            if net_nick.is_empty() || net_nick.contains(ILLEGAL_NICK_CHARS) {
                return Err(irc_error(msg!(
                    ERR_ERRONEUSNICKNAME,
                    nick,
                    r.nick.clone(),
                    "Nickname contains illegal characters"
                )));
            }
            if xirc::casemap_ascii(&net_nick) == SERVICE_NICK_CM {
                return Err(irc_error(msg!(
                    ERR_NICKNAMEINUSE,
                    nick,
                    r.nick.clone(),
                    "Nickname reserved for bouncer service"
                )));
            }

            let mut record = db::Network::new(&r.network_name);
            record.nick = net_nick;
            logf!(
                u.downstreams[i].logger,
                "auto-saving network {:?}",
                r.network_name
            );
            u.create_network(record, true)?
        }
    };
    let net_id = u.networks[ni].id();
    let i = u.dc_index(dc_id).unwrap();
    u.downstreams[i].network_id = Some(net_id);
    Ok(())
}

/// Sends the welcome burst to a newly registered connection.
pub fn welcome(u: &mut User, dc_id: u64) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    {
        let dc = &mut u.downstreams[i];
        assert!(dc.registered, "tried to welcome an unregistered connection");
        dc.logger = u.srv.logger.with_prefix(format!(
            "user {:?}: downstream {:?}: ",
            u.record.username, dc.info.remote_addr
        ));
    }

    load_network(u, dc_id)?;

    let i = u.dc_index(dc_id).unwrap();
    u.downstreams[i].registration = None;

    update_supported_caps(u, dc_id);

    let i = u.dc_index(dc_id).unwrap();
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let nick = current_nick(u, net_idx);
    let cfg = u.srv.config();
    {
        let dc = &mut u.downstreams[i];
        dc.nick = nick.clone();
        dc.nick_cm = dc.casemap(&nick);
        dc.send_message(msg!(
            RPL_WELCOME,
            nick.clone(),
            format!("Welcome to soju, {nick}")
        ));
        dc.send_message(msg!(
            RPL_YOURHOST,
            nick.clone(),
            format!("Your host is {}", cfg.basic.hostname)
        ));
        dc.send_message(msg!(
            RPL_MYINFO,
            nick.clone(),
            cfg.basic.hostname.clone(),
            "soju",
            "aiwroO",
            "OovaimnqpsrtklbeI"
        ));
    }
    update_isupport(u, dc_id);
    let i = u.dc_index(dc_id).unwrap();
    if let Some(ni) = net_idx
        && let Some(uc) = u.networks[ni].upstream()
    {
        let modes = format!("+{}", uc.modes.0);
        u.downstreams[i].send_message(msg!(RPL_UMODEIS, nick.clone(), modes));
    }
    if net_idx.is_none() && u.record.admin {
        u.downstreams[i].send_message(msg!(RPL_UMODEIS, nick.clone(), "+o"));
    }

    update_host(u, dc_id);
    update_realname(u, dc_id);
    update_account(u, dc_id);
    let cm = net_idx
        .map(|ni| u.networks[ni].casemap)
        .unwrap_or(xirc::CASEMAPPING_ASCII);
    let i = u.dc_index(dc_id).unwrap();
    u.downstreams[i].update_casemapping(cm);

    {
        let dc = &mut u.downstreams[i];
        if !cfg.motd.is_empty() && net_idx.is_none() {
            for m in xirc::generate_motd(&cfg.motd) {
                dc.send_message(m);
            }
        } else {
            let hint = if net_idx.is_some() {
                "Use /motd to read the message of the day"
            } else {
                "No MOTD"
            };
            dc.send_message(msg!(ERR_NOMOTD, nick.clone(), hint));
        }
    }

    if u.downstreams[i]
        .caps
        .is_enabled("soju.im/bouncer-networks-notify")
    {
        send_bouncer_networks(u, i);
    }

    if u.downstreams[i].caps.is_enabled("draft/metadata-2") {
        // No user-specific metadata
        u.downstreams[i].send_batch("metadata", &[], Vec::new());
    }

    if let Some(ni) = net_idx
        && u.downstreams[i].caps.is_enabled("draft/metadata-2")
    {
        let targets = u
            .srv
            .db
            .list_message_targets(u.networks[ni].id())
            .map_err(|e| anyhow!("failed to list message targets: {e}"))?;
        for mt in targets {
            send_target_metadata(&mut u.downstreams[i], &mt);
        }
    }

    if let Some(ni) = net_idx {
        let net = &u.networks[ni];
        if let Some(uc) = net.upstream() {
            let dc = &mut u.downstreams[i];
            for ch in uc.channels.values() {
                if !ch.complete {
                    continue;
                }
                if net.channels.get(&ch.name).is_some_and(|r| r.detached) {
                    continue;
                }
                let mut params = vec![ch.name.clone()];
                if dc.caps.is_enabled("extended-join") {
                    let account = if uc.account.is_empty() {
                        "*".to_owned()
                    } else {
                        uc.account.clone()
                    };
                    params.push(account);
                    params.push(uc.realname.clone());
                }
                let prefix = dc.prefix();
                dc.send_message(Message::new("JOIN", params).with_prefix(prefix));
                forward_channel(&u.srv.db, dc, ch, net.id(), net.casemap);
            }
        }
    }

    if let Some(ni) = net_idx
        && !u.downstreams[i].caps.is_enabled("draft/chathistory")
    {
        // Only send history if we're the first connected client with that
        // name for the network
        let dc = &u.downstreams[i];
        let net_id = u.networks[ni].id();
        let first_client = !u.downstreams.iter().any(|c| {
            c.id != dc.id && c.client_name == dc.client_name && c.network_id == Some(net_id)
        });
        if first_client {
            let client = dc.client_name.clone();
            for target in u.networks[ni].delivered.targets() {
                let last_delivered = u.networks[ni].delivered.load_id(&target, &client);
                if last_delivered.is_empty() {
                    continue;
                }
                let i = u.dc_index(dc_id).unwrap();
                send_target_backlog(u, i, ni, &target, &last_delivered);

                // Fast-forward history to last message
                let net = &mut u.networks[ni];
                let target_cm = net.casemap(&target);
                let store = &mut u.msg_store;
                match store.last_msg_id(&net.record, &target_cm) {
                    Ok(last_id) => net.delivered.store_id(&target, &client, &last_id),
                    Err(e) => {
                        let i = u.dc_index(dc_id).unwrap();
                        logf!(
                            u.downstreams[i].logger,
                            "failed to get last message ID: {e}"
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Sends the backlog of a target since a message ID, for clients without
/// chathistory.
pub fn send_target_backlog(u: &mut User, dc_idx: usize, ni: usize, target: &str, msg_id: &str) {
    if u.downstreams[dc_idx].caps.is_enabled("draft/chathistory") {
        return;
    }
    let net = &u.networks[ni];
    let ch = net.channels.get(target).cloned();
    let target_cm = net.casemap(target);
    let opts = crate::msgstore::LoadMessageOptions {
        network: &net.record,
        entity: &target_cm,
        limit: BACKLOG_LIMIT,
        events: false,
    };
    let history = match u.msg_store.load_latest_id(msg_id, &opts) {
        Ok(h) => h,
        Err(e) => {
            logf!(
                u.downstreams[dc_idx].logger,
                "failed to send backlog for {target:?}: {e}"
            );
            return;
        }
    };

    let net = &u.networks[ni];
    let user = &u.record;
    let relays: Vec<(bool, bool)> = history
        .iter()
        .map(|m| match &ch {
            Some(ch) if ch.detached => (
                net.detached_message_needs_relay(user, ch, m),
                net.is_highlight(user, m),
            ),
            _ => (false, false),
        })
        .collect();
    let detached = ch.as_ref().is_some_and(|c| c.detached);

    let dc = &mut u.downstreams[dc_idx];
    let r = dc.begin_batch("chathistory", &[target.to_owned()], Tags::new());
    for (mut m, (relay, highlight)) in history.into_iter().zip(relays) {
        if detached {
            if relay {
                relay_detached_message(dc, highlight, &m);
            }
        } else {
            m.tags.set("batch", r.clone());
            dc.send_message(m);
        }
    }
    dc.end_batch(&r);
}

/// Sends the list of the user's networks, in a soju.im/bouncer-networks
/// batch.
pub fn send_bouncer_networks(u: &mut User, i: usize) {
    let msgs: Vec<Message> = (0..u.networks.len())
        .map(|ni| {
            msg!(
                "BOUNCER",
                "NETWORK",
                u.networks[ni].id().to_string(),
                u.network_attrs(ni).to_string()
            )
        })
        .collect();
    u.downstreams[i].send_batch("soju.im/bouncer-networks", &[], msgs);
}

pub fn relay_detached_message(dc: &mut DownstreamConn, highlight: bool, msg: &Message) {
    if msg.command != "PRIVMSG" && msg.command != "NOTICE" {
        return;
    }
    let sender = msg.prefix_name();
    let (target, text) = (msg.param(0), msg.param(1));
    let s = if highlight {
        format!("highlight in {target}: <{sender}> {text}")
    } else {
        format!("message in {target}: <{sender}> {text}")
    };
    send_service_notice(dc, &s);
}

pub fn update_supported_caps(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let uc = net_idx.and_then(|ni| u.networks[ni].upstream());
    let has_network = u.downstreams[i].network_id.is_some();
    let chathistory = u.has_chathistory_store();

    let supported: Vec<(&str, &str, bool)> = PASSTHROUGH_DOWNSTREAM_CAPS
        .iter()
        .map(|(cap, v)| (*cap, *v, uc.is_none_or(|uc| uc.caps.is_enabled(cap))))
        .collect();
    let sasl_plain = uc.is_some_and(|uc| uc.supports_sasl("PLAIN"));
    let multiline = uc.and_then(|uc| uc.multiline_limits());
    let account_registration = uc.and_then(|uc| {
        if uc.caps.is_enabled("draft/account-registration") {
            Some(
                uc.caps
                    .available
                    .get("draft/account-registration")
                    .cloned()
                    .unwrap_or_default(),
            )
        } else {
            None
        }
    });

    let dc = &mut u.downstreams[i];
    for (cap, v, ok) in supported {
        if ok {
            dc.set_supported_cap(cap, v);
        } else {
            dc.unset_supported_cap(cap);
        }
    }

    if sasl_plain {
        dc.set_supported_cap("sasl", "PLAIN,ANONYMOUS");
    } else if has_network {
        dc.unset_supported_cap("sasl");
    }

    match account_registration {
        Some(v) => {
            // Strip "before-connect", because we require downstreams to be
            // fully connected before attempting account registration.
            let mut values: Vec<&str> = v.split(',').collect();
            if let Some(pos) = values.iter().position(|v| *v == "before-connect") {
                values.remove(pos);
            }
            dc.set_supported_cap("draft/account-registration", &values.join(","));
        }
        None => dc.unset_supported_cap("draft/account-registration"),
    }

    match multiline {
        Some(l) => dc.set_supported_cap(xirc::MULTILINE_BATCH_TYPE, &l.cap_value()),
        None => dc.unset_supported_cap(xirc::MULTILINE_BATCH_TYPE),
    }

    if chathistory && has_network {
        dc.set_supported_cap("draft/event-playback", "");
    } else {
        dc.unset_supported_cap("draft/event-playback");
    }
}

pub fn update_isupport(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let dc = &u.downstreams[i];
    if !dc.registered && !dc.caps.is_enabled("draft/extended-isupport") {
        return;
    }
    let isupport = build_user_isupport(u, i);

    let mut changed = Isupport::new();
    for (k, v) in &isupport {
        if dc.isupport.get(k) == Some(v) {
            continue;
        }
        changed.insert(k.clone(), v.clone());
    }
    for k in dc.isupport.keys() {
        if !isupport.contains_key(k) {
            changed.insert(format!("-{k}"), None);
        }
    }

    let dc = &mut u.downstreams[i];
    send_isupport(dc, &changed);
    dc.isupport = isupport;
}

/// The nick of a downstream connection: our nick on its network, or the
/// user's for a bouncer connection.
fn current_nick(u: &User, net_idx: Option<usize>) -> String {
    match net_idx {
        Some(ni) => match u.networks[ni].upstream() {
            Some(uc) => uc.nick.clone(),
            None => get_nick(&u.record, Some(&u.networks[ni].record)).to_owned(),
        },
        None => get_nick(&u.record, None).to_owned(),
    }
}

pub fn update_nick(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let nick = current_nick(u, net_idx);
    let dc = &mut u.downstreams[i];
    if nick == dc.nick {
        return;
    }
    let prefix = dc.prefix();
    dc.send_message(msg!("NICK", nick.clone()).with_prefix(prefix));
    dc.nick_cm = dc.casemap(&nick);
    dc.nick = nick;
}

pub fn update_host(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let Some(uc) = net_idx.and_then(|ni| u.networks[ni].upstream()) else {
        return;
    };
    if uc.hostname.is_empty() {
        return;
    }
    let (host, user) = (uc.hostname.clone(), uc.username.clone());
    let dc = &mut u.downstreams[i];
    if host == dc.hostname && user == dc.username {
        return;
    }
    let prefix = dc.prefix();
    if dc.caps.is_enabled("chghost") {
        dc.send_message(msg!("CHGHOST", user.clone(), host.clone()).with_prefix(prefix));
    } else if host != dc.hostname {
        let nick = dc.nick.clone();
        dc.send_message(msg!(
            RPL_VISIBLEHOST,
            nick,
            host.clone(),
            "is now your visible host"
        ));
    }
    dc.hostname = host;
    dc.username = user;
}

pub fn update_realname(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    if !u.downstreams[i].caps.is_enabled("setname") {
        return;
    }
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let realname = match net_idx {
        Some(ni) => match u.networks[ni].upstream() {
            Some(uc) => uc.realname.clone(),
            None => get_realname(&u.record, Some(&u.networks[ni].record)).to_owned(),
        },
        None => get_realname(&u.record, None).to_owned(),
    };
    let dc = &mut u.downstreams[i];
    if realname != dc.realname {
        let prefix = dc.prefix();
        dc.send_message(msg!("SETNAME", realname.clone()).with_prefix(prefix));
        dc.realname = realname;
    }
}

pub fn update_account(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let account = match u.downstreams[i].network_id {
        None => u.record.username.clone(),
        Some(id) => {
            let ni = u.net_index(id);
            match ni.and_then(|ni| u.networks[ni].upstream()) {
                Some(uc) => uc.account.clone(),
                None => return,
            }
        }
    };
    let dc = &mut u.downstreams[i];
    if dc.account == account || !dc.caps.is_enabled("sasl") {
        return;
    }
    let nick = dc.nick.clone();
    let prefix = dc.prefix().to_string();
    if !account.is_empty() {
        dc.send_message(msg!(
            RPL_LOGGEDIN,
            nick,
            prefix,
            account.clone(),
            format!("You are logged in as {account}")
        ));
    } else {
        dc.send_message(msg!(RPL_LOGGEDOUT, nick, prefix, "You are logged out"));
    }
    dc.account = account;
}

/// Sends the state of a channel after a JOIN.
pub fn forward_channel(
    db: &db::Database,
    dc: &mut DownstreamConn,
    ch: &UpstreamChannel,
    net_id: i64,
    cm: CaseMapping,
) {
    assert!(ch.complete, "Tried to forward a partial channel");

    // RPL_NOTOPIC shouldn't be sent on JOIN
    if !ch.topic.is_empty() {
        send_topic(dc, ch);
    }

    let mark_read_cmd = if dc.caps.is_enabled("draft/read-marker") {
        Some("MARKREAD")
    } else if dc.caps.is_enabled("soju.im/read") {
        Some("READ")
    } else {
        None
    };
    if let Some(cmd) = mark_read_cmd {
        match db.get_read_receipt(net_id, &cm(&ch.name)) {
            Err(e) => logf!(
                dc.logger,
                "failed to get the read receipt for {:?}: {e}",
                ch.name
            ),
            Ok(r) => {
                let ts = match r.and_then(|r| r.timestamp) {
                    Some(t) => format!("timestamp={}", xirc::format_server_time(t)),
                    None => "*".to_owned(),
                };
                let prefix = dc.prefix();
                dc.send_message(msg!(cmd, ch.name.clone(), ts).with_prefix(prefix));
            }
        }
    }

    if !dc.caps.is_enabled("soju.im/no-implicit-names")
        && !dc.caps.is_enabled("no-implicit-names")
        && !dc.caps.is_enabled("draft/no-implicit-names")
    {
        send_names(dc, ch);
    }
}

pub fn send_topic(dc: &mut DownstreamConn, ch: &UpstreamChannel) {
    let nick = dc.nick.clone();
    if !ch.topic.is_empty() {
        dc.send_message(msg!(
            RPL_TOPIC,
            nick.clone(),
            ch.name.clone(),
            ch.topic.clone()
        ));
        if let Some(who) = &ch.topic_who {
            let t = ch.topic_time.map(|t| t.timestamp()).unwrap_or(-62135596800);
            dc.send_message(msg!(
                RPL_TOPICWHOTIME,
                nick,
                ch.name.clone(),
                who.to_string(),
                t.to_string()
            ));
        }
    } else {
        dc.send_message(msg!(RPL_NOTOPIC, nick, ch.name.clone(), "No topic is set"));
    }
}

pub fn send_names(dc: &mut DownstreamConn, ch: &UpstreamChannel) {
    let multi_prefix = dc.caps.is_enabled("multi-prefix");
    let members: Vec<String> = ch
        .members
        .iter()
        .map(|(nick, ms)| format!("{}{nick}", ircutil::format_member_prefix(ms, multi_prefix)))
        .collect();
    for m in xirc::generate_names_reply(&ch.name, ch.status, &members) {
        dc.send_message(m);
    }
}

pub fn get_message_target_metadata(mt: &db::MessageTarget) -> Vec<(&'static str, &'static str)> {
    let b = |v: bool| if v { "1" } else { "0" };
    vec![
        ("soju.im/pinned", b(mt.pinned)),
        ("soju.im/muted", b(mt.muted)),
        ("soju.im/blocked", b(mt.blocked)),
    ]
}

pub fn send_target_metadata(dc: &mut DownstreamConn, mt: &db::MessageTarget) {
    if !dc.caps.is_enabled("draft/metadata-2") {
        return;
    }
    let m = get_message_target_metadata(mt);
    let msgs: Vec<Message> = m
        .into_iter()
        .filter(|(k, _)| dc.metadata_subs.contains(*k))
        .map(|(k, v)| msg!("METADATA", mt.target.clone(), k, "*", v))
        .collect();
    dc.send_batch("metadata", &[], msgs);
}

/// Handles METADATA SUB/UNSUB/SUBS. Returns None if the subcommand is
/// something else. targets is the list of message targets of the network,
/// if bound to one.
pub fn handle_metadata_sub(
    dc: &mut DownstreamConn,
    msg: &Message,
    targets: Option<&dyn Fn() -> Result<Vec<db::MessageTarget>>>,
) -> Option<Result<()>> {
    if msg.params.len() < 2 {
        return Some(Err(need_more_params(&msg.command).into()));
    }
    let subcommand = msg.params[1].as_str();
    match subcommand {
        "SUB" | "UNSUB" => {
            // Listed once, for all keys
            let mut mts: Option<Vec<db::MessageTarget>> = None;
            for k in &msg.params[2..] {
                let k = k.to_lowercase();
                if !matches!(
                    k.as_str(),
                    "soju.im/pinned" | "soju.im/muted" | "soju.im/blocked"
                ) {
                    dc.send_message(msg!(
                        "FAIL",
                        msg.command.clone(),
                        "KEY_INVALID",
                        k,
                        "Invalid key"
                    ));
                    continue;
                }
                if subcommand == "SUB" {
                    dc.metadata_subs.insert(k.clone());

                    // Technically we are only required to send the METADATA
                    // of channels we are joined to, and users we are sharing
                    // a channel with or are MONITOR-ing, but it is easier to
                    // send all METADATA we are aware of, and the spec
                    // guarantees that servers MAY send these messages at any
                    // time.
                    if let Some(targets) = targets {
                        if mts.is_none() {
                            mts = match targets() {
                                Ok(t) => Some(t),
                                Err(e) => {
                                    logf!(
                                        dc.logger,
                                        "failed to list message targets for METADATA SUB: {e}"
                                    );
                                    return Some(Err(fail(&[
                                        "METADATA",
                                        "INTERNAL_ERROR",
                                        "*",
                                        "Internal error",
                                    ])));
                                }
                            };
                        }
                        for mt in mts.iter().flatten() {
                            for (key, v) in get_message_target_metadata(mt) {
                                if key == k {
                                    dc.send_message(msg!(
                                        "METADATA",
                                        mt.target.clone(),
                                        key,
                                        "*",
                                        v
                                    ));
                                }
                            }
                        }
                    }
                    dc.send_message(msg!(RPL_METADATASUBOK, "*", k));
                } else {
                    dc.metadata_subs.remove(&k);
                    dc.send_message(msg!(RPL_METADATAUNSUBOK, "*", k));
                }
            }
            Some(Ok(()))
        }
        "SUBS" => {
            let subs: Vec<String> = dc.metadata_subs.iter().cloned().collect();
            for k in subs {
                dc.send_message(msg!(RPL_METADATASUBS, "*", k));
            }
            Some(Ok(()))
        }
        _ => None,
    }
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

    #[test]
    fn sasl_mechanisms() {
        let mut s = DownstreamSasl {
            kind: SaslServerKind::Plain,
            mechanism: "PLAIN".into(),
            done: false,
            oauth_fail: None,
            plain: None,
            oauth_bearer: None,
            pending_resp: String::new(),
        };
        assert_eq!(s.next(None).unwrap(), Some(vec![]));
        assert_eq!(s.next(Some(b"\0alice\0pw")).unwrap(), None);
        assert_eq!(s.plain.as_ref().unwrap().username, "alice");
        assert!(s.next(Some(b"x")).is_err());

        let mut s = DownstreamSasl {
            kind: SaslServerKind::OAuthBearer,
            mechanism: "OAUTHBEARER".into(),
            done: false,
            oauth_fail: None,
            plain: None,
            oauth_bearer: None,
            pending_resp: String::new(),
        };
        assert_eq!(
            s.next(Some(b"n,a=alice,\x01auth=Bearer tok\x01\x01"))
                .unwrap(),
            None
        );
        let o = s.oauth_bearer.unwrap();
        assert_eq!((o.username.as_str(), o.token.as_str()), ("alice", "tok"));
    }
}
