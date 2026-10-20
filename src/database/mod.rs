//! Persistent storage for users, networks, channels and messages.

mod sqlite;
mod sqlite_migrations;
mod utils;

pub use sqlite::Database;
pub use utils::strip_ansi;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateDeviceCertificate;

impl std::fmt::Display for DuplicateDeviceCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("duplicate device certificate")
    }
}

impl std::error::Error for DuplicateDeviceCertificate {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageTargetLast {
    pub name: String,
    pub latest_message: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default)]
pub struct MessageOptions {
    pub after_id: i64,
    pub after_time: Option<DateTime<Utc>>,
    pub before_time: Option<DateTime<Utc>>,
    pub limit: usize,
    pub events: bool,
    pub sender: String,
    pub text: String,
    pub take_last: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatabaseStats {
    pub users: i64,
    pub networks: i64,
    pub channels: i64,
}

/// bcrypt's default cost, as in golang.org/x/crypto/bcrypt.
pub const BCRYPT_DEFAULT_COST: u32 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    /// bcrypt hash, empty if password authentication is disabled
    pub password: String,
    pub nick: String,
    pub realname: String,
    pub admin: bool,
    pub enabled: bool,
    pub downstream_interacted_at: Option<DateTime<Utc>>,
    pub max_networks: i64,
}

impl User {
    pub fn new(username: &str) -> User {
        User {
            id: 0,
            username: username.to_owned(),
            password: String::new(),
            nick: String::new(),
            realname: String::new(),
            admin: false,
            enabled: true,
            downstream_interacted_at: None,
            max_networks: -1,
        }
    }

    /// Checks a password. Returns true if the hash has been upgraded and the
    /// user record needs to be stored.
    pub fn check_password(&mut self, password: &str) -> Result<bool> {
        if self.password.is_empty() {
            bail!("password auth disabled");
        }
        let ok =
            bcrypt::verify(password, &self.password).map_err(|e| anyhow!("wrong password: {e}"))?;
        if !ok {
            bail!(
                "wrong password: crypto/bcrypt: hashedPassword is not the hash of the given password"
            );
        }
        let cost = bcrypt_cost(&self.password)?;
        if cost < BCRYPT_DEFAULT_COST {
            self.set_password(password)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn set_password(&mut self, password: &str) -> Result<()> {
        // The bcrypt crate would silently truncate longer passwords, Go's
        // bcrypt rejects them
        if password.len() > 72 {
            bail!("failed to hash password: bcrypt: password length exceeds 72 bytes");
        }
        let hashed = bcrypt::hash_with_result(password, BCRYPT_DEFAULT_COST)
            .map_err(|e| anyhow!("failed to hash password: {e}"))?;
        // Go's bcrypt uses the "$2a$" prefix
        self.password = hashed.format_for_version(bcrypt::Version::TwoA);
        Ok(())
    }
}

fn bcrypt_cost(hash: &str) -> Result<u32> {
    // $2a$10$...
    let parts: Vec<&str> = hash.split('$').collect();
    if parts.len() < 4 {
        bail!("invalid password cost: malformed hash");
    }
    parts[2]
        .parse()
        .map_err(|e| anyhow!("invalid password cost: {e}"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sasl {
    pub mechanism: String,
    pub plain_username: String,
    pub plain_password: String,
    /// X.509 certificate in DER form.
    pub external_cert: Option<Vec<u8>>,
    /// PKCS#8 private key in DER form.
    pub external_key: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub id: i64,
    pub name: String,
    pub addr: String,
    pub nick: String,
    pub username: String,
    pub realname: String,
    pub pass: String,
    pub connect_commands: Vec<String>,
    pub certfp: String,
    pub sasl: Sasl,
    pub auto_away: bool,
    pub enabled: bool,
}

/// A parsed network address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkUrl {
    pub scheme: String,
    /// host[:port], empty for Unix sockets
    pub host: String,
    pub path: String,
    pub has_user: bool,
    pub query: String,
    pub fragment: String,
}

impl Network {
    pub fn new(addr: &str) -> Network {
        Network {
            id: 0,
            name: String::new(),
            addr: addr.to_owned(),
            nick: String::new(),
            username: String::new(),
            realname: String::new(),
            pass: String::new(),
            connect_commands: Vec::new(),
            certfp: String::new(),
            sasl: Sasl::default(),
            auto_away: true,
            enabled: true,
        }
    }

    pub fn get_name(&self) -> &str {
        if !self.name.is_empty() {
            &self.name
        } else {
            &self.addr
        }
    }

    pub fn url(&self) -> Result<NetworkUrl> {
        let mut s = self.addr.clone();
        if !s.contains("://") {
            // This is a raw domain name, make it a URL with the default scheme
            s = format!("ircs://{s}");
        }
        let mut u =
            parse_url(&s).map_err(|e| anyhow!("failed to parse upstream server URL: {e}"))?;
        if u.scheme == "irc+unix" || u.scheme == "unix" {
            u.path = format!("{}{}", u.host, u.path);
            u.host = String::new();
        }
        Ok(u)
    }
}

/// A minimal URL parser following Go's net/url for the URLs soju uses
/// (scheme://[userinfo@]host[:port][/path][?query][#fragment]).
pub fn parse_url(s: &str) -> Result<NetworkUrl> {
    let (s, fragment) = match s.split_once('#') {
        Some((a, f)) => (a, f.to_owned()),
        None => (s, String::new()),
    };
    let (s, query) = match s.split_once('?') {
        Some((a, q)) => (a, q.to_owned()),
        None => (s, String::new()),
    };
    let Some((scheme, rest)) = s.split_once("://") else {
        bail!("missing scheme");
    };
    if scheme.is_empty()
        || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        bail!("first path segment in URL cannot contain colon");
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (has_user, host) = match authority.rfind('@') {
        Some(i) => (true, &authority[i + 1..]),
        None => (false, authority),
    };
    if host.contains(' ') {
        bail!("invalid character \" \" in host name");
    }
    Ok(NetworkUrl {
        scheme: scheme.to_lowercase(),
        host: host.to_owned(),
        path: path.to_owned(),
        has_user,
        query,
        fragment,
    })
}

/// Splits host:port like Go's net.SplitHostPort.
pub fn split_host_port(hostport: &str) -> Result<(String, String)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            bail!("address {hostport}: missing ']' in address");
        };
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let Some(port) = after.strip_prefix(':') else {
            bail!("address {hostport}: missing port in address");
        };
        return Ok((host.to_owned(), port.to_owned()));
    }
    let Some(i) = hostport.rfind(':') else {
        bail!("address {hostport}: missing port in address");
    };
    let host = &hostport[..i];
    if host.contains(':') {
        bail!("address {hostport}: too many colons in address");
    }
    Ok((host.to_owned(), hostport[i + 1..].to_owned()))
}

/// Joins host and port like Go's net.JoinHostPort.
pub fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn get_nick<'a>(user: &'a User, net: Option<&'a Network>) -> &'a str {
    if let Some(net) = net
        && !net.nick.is_empty()
    {
        return &net.nick;
    }
    if !user.nick.is_empty() {
        return &user.nick;
    }
    &user.username
}

pub fn get_username<'a>(user: &'a User, net: Option<&'a Network>) -> &'a str {
    if let Some(net) = net
        && !net.username.is_empty()
    {
        return &net.username;
    }
    get_nick(user, net)
}

pub fn get_realname<'a>(user: &'a User, net: Option<&'a Network>) -> &'a str {
    if let Some(net) = net
        && !net.realname.is_empty()
    {
        return &net.realname;
    }
    if !user.realname.is_empty() {
        return &user.realname;
    }
    get_nick(user, net)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MessageFilter {
    // TODO: use customizable user defaults for FilterDefault
    #[default]
    Default = 0,
    None = 1,
    Highlight = 2,
    Message = 3,
}

impl MessageFilter {
    pub fn from_i64(v: i64) -> MessageFilter {
        match v {
            1 => MessageFilter::None,
            2 => MessageFilter::Highlight,
            3 => MessageFilter::Message,
            _ => MessageFilter::Default,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub key: String,
    pub detached: bool,
    pub detached_internal_msg_id: String,
    pub relay_detached: MessageFilter,
    pub reattach_on: MessageFilter,
    pub detach_after: Duration,
    pub detach_on: MessageFilter,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceCertificate {
    pub id: i64,
    pub label: String,
    /// SHA-512 hash
    pub fingerprint: Vec<u8>,
    pub last_used: Option<DateTime<Utc>>,
    pub last_ip: String,
}

impl DeviceCertificate {
    pub fn mark_used(&mut self, addr: Option<SocketAddr>) {
        self.last_used = Some(Utc::now());
        self.last_ip = match addr {
            Some(a) => a.ip().to_canonical().to_string(),
            None => String::new(),
        };
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub id: i64,
    /// channel or nick
    pub target: String,
    pub client: String,
    pub internal_msg_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadReceipt {
    pub id: i64,
    /// channel or nick
    pub target: String,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebPushConfig {
    pub id: i64,
    pub vapid_public: String,
    pub vapid_private: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebPushKeys {
    pub auth: String,
    pub p256dh: String,
    pub vapid: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebPushSubscription {
    pub id: i64,
    pub endpoint: String,
    /// read-only
    pub created_at: Option<DateTime<Utc>>,
    /// read-only
    pub updated_at: Option<DateTime<Utc>>,
    pub keys: WebPushKeys,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageTarget {
    pub id: i64,
    pub target: String,
    pub pinned: bool,
    pub muted: bool,
    pub blocked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_url() {
        let n = Network::new("irc.libera.chat");
        let u = n.url().unwrap();
        assert_eq!(u.scheme, "ircs");
        assert_eq!(u.host, "irc.libera.chat");
        let n = Network::new("irc+unix:///run/ircd.sock");
        let u = n.url().unwrap();
        assert_eq!(u.host, "");
        assert_eq!(u.path, "/run/ircd.sock");
        let n = Network::new("unix://foo/bar");
        assert_eq!(n.url().unwrap().path, "foo/bar");
        let u = Network::new("ircs://user:pass@host:6697?x#y")
            .url()
            .unwrap();
        assert!(u.has_user);
        assert_eq!(u.host, "host:6697");
        assert_eq!(u.query, "x");
        assert_eq!(u.fragment, "y");
    }

    #[test]
    fn host_port() {
        assert_eq!(
            split_host_port("a:1").unwrap(),
            ("a".to_owned(), "1".to_owned())
        );
        assert_eq!(
            split_host_port("[::1]:6697").unwrap(),
            ("::1".to_owned(), "6697".to_owned())
        );
        assert!(split_host_port("a").is_err());
        assert!(split_host_port("::1").is_err());
        assert_eq!(join_host_port("::1", "1"), "[::1]:1");
    }

    #[test]
    fn password() {
        let mut u = User::new("a");
        u.set_password("secret").unwrap();
        assert!(u.password.starts_with("$2a$10$"));
        assert!(!u.check_password("secret").unwrap());
        assert!(u.check_password("wrong").is_err());
        u.set_password(&"x".repeat(72)).unwrap();
        assert!(u.set_password(&"x".repeat(73)).is_err());
        // Hash generated by Go's bcrypt with cost 4: gets upgraded
        let mut u = User::new("a");
        u.password = bcrypt::hash("pw", 4).unwrap();
        assert!(u.check_password("pw").unwrap());
        assert!(u.password.starts_with("$2a$10$"));
    }
}
