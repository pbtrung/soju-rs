//! Configuration file loading.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use ipnet::IpNet;

mod directives;
pub use directives::parse;

/// Default config file path. Empty means none.
pub const DEFAULT_PATH: &str = match option_env!("SOJU_CONFIG_PATH") {
    Some(p) => p,
    None => "",
};

pub const DEFAULT_UNIX_ADMIN_PATH: &str = match option_env!("SOJU_ADMIN_SOCKET_PATH") {
    Some(p) => p,
    None => "/run/soju/admin",
};

/// A set of IP networks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IpSet(pub Vec<IpNet>);

impl IpSet {
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.iter().any(|n| n.contains(&ip))
    }
}

/// The loopback networks 127.0.0.0/8 and `::1/128`.
#[expect(
    clippy::expect_used,
    reason = "Both literal loopback CIDRs are audited constants and cannot fail to parse."
)]
fn loopback_ips() -> Vec<IpNet> {
    vec![
        "127.0.0.0/8".parse().expect("literal loopback CIDR"),
        "::1/128".parse().expect("literal loopback CIDR"),
    ]
}

fn parse_duration(s: &str) -> Result<Duration> {
    let Some(v) = s.strip_suffix('d') else {
        bail!("missing 'd' suffix in duration");
    };
    let v: f64 = v.parse().map_err(|e| anyhow!("invalid duration: {e}"))?;
    // Upstream's conversion to a time.Duration (int64 nanoseconds)
    // overflows to a negative duration for values out of range (including
    // inf and NaN)
    let secs = v * 24.0 * 3600.0;
    match Duration::try_from_secs_f64(secs) {
        Ok(d) if v >= 0.0 && secs <= crate::service::MAX_GO_DURATION_SECS => Ok(d),
        _ => bail!("duration must be positive"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgStoreDriver {
    Memory,
    Db,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsgStore {
    pub driver: MsgStoreDriver,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tls {
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Db {
    pub driver: String,
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthDriver {
    Internal,
    Pam,
    Http,
    OAuth2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    pub driver: AuthDriver,
    pub source: String,
}

/// The subset of the configuration which can be reloaded at run time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasicServer {
    pub hostname: String,
    pub title: String,
    pub msg_store: MsgStore,
    pub http_origins: Vec<String>,
    pub http_ingress: String,
    pub accept_proxy_ips: IpSet,
    pub accept_proxy_unix: bool,
    pub max_user_networks: i64,
    /// Global accepted physical-session cap, including incomplete handshakes.
    pub max_accepted_connections: usize,
    pub upstream_user_ips: Vec<IpNet>,
    pub disable_inactive_users_delay: Duration,
    pub message_expiry: Duration,
    /// The maximum number of messages per CHATHISTORY reply
    pub chat_history_limit: usize,
    pub enable_users_on_auth: bool,
    pub client_cert_auth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub basic: BasicServer,
    pub listen: Vec<String>,
    pub tls: Option<Tls>,
    pub icon: String,
    pub motd_path: String,
    pub db: Db,
    pub auth: Vec<Auth>,
    /// Set if the config contains a file-upload directive (unsupported).
    pub file_upload: bool,
}

#[must_use]
pub fn system_hostname() -> String {
    match gethostname::gethostname().into_string() {
        Ok(h) if !h.is_empty() => h,
        _ => "localhost".to_owned(),
    }
}

impl Server {
    #[must_use]
    pub fn defaults() -> Server {
        let hostname = system_hostname();
        Server {
            basic: BasicServer {
                http_ingress: format!("https://{hostname}"),
                hostname,
                title: String::new(),
                msg_store: MsgStore {
                    driver: MsgStoreDriver::Db,
                    source: String::new(),
                },
                http_origins: Vec::new(),
                accept_proxy_ips: IpSet::default(),
                accept_proxy_unix: false,
                max_user_networks: -1,
                max_accepted_connections: 256,
                upstream_user_ips: Vec::new(),
                disable_inactive_users_delay: Duration::ZERO,
                message_expiry: Duration::ZERO,
                chat_history_limit: MAX_CHAT_HISTORY_LIMIT,
                enable_users_on_auth: false,
                client_cert_auth: false,
            },
            listen: Vec::new(),
            tls: None,
            icon: String::new(),
            motd_path: String::new(),
            db: Db {
                driver: "sqlite3".to_owned(),
                source: "soju.db".to_owned(),
            },
            auth: vec![Auth {
                driver: AuthDriver::Internal,
                source: String::new(),
            }],
            file_upload: false,
        }
    }
}

///
/// # Errors
///
/// Returns an error for invalid configuration values/directives or configuration I/O failures.
pub fn load(filename: &str) -> Result<Server> {
    let s = std::fs::read_to_string(filename)?;
    parse(&s)
}

/// The default and maximum chathistory-limit, the same limit as upstream.
pub const MAX_CHAT_HISTORY_LIMIT: usize = 1000;
/// The minimum chathistory-limit: clients would need too many requests to
/// fetch history with a lower one.
pub const MIN_CHAT_HISTORY_LIMIT: usize = 100;

/// Parses a CIDR, returning the network (host bits cleared), like Go's
/// net.ParseCIDR.
///
/// # Errors
///
/// Returns an error for invalid configuration values/directives or configuration I/O failures.
pub fn parse_cidr(s: &str) -> Result<IpNet> {
    let n: IpNet = s
        .parse::<IpNet>()
        .map_err(|error| anyhow::anyhow!(error).context(anyhow!("invalid CIDR address: {s}")))?;
    Ok(n.trunc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_error_precedence_is_stable() {
        for (source, expected) in [
            ("unknown value\nlisten", "unknown directive"),
            (
                "auth missing\nlisten",
                "directive \"listen\": directive requires exactly one parameter",
            ),
            (
                "auth missing\ndb postgres source",
                "driver \"postgres\" is not supported",
            ),
            (
                "message-store memory unused\nauth missing",
                "requires zero parameters",
            ),
            ("auth http\nhttp-origin [", "requires a source"),
            (
                "hostname one\nhostname two",
                "directive must not be specified more than once",
            ),
            (
                "auth internal {\nchild\n}\n",
                "directive requires zero children",
            ),
        ] {
            let error = parse(source).err().map(|error| error.to_string());
            assert!(
                error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected)),
                "{source:?}: {error:?}"
            );
        }
        let config = parse(
            "hostname example.org\nlog memory\nmessage-store db\nauth internal\nauth oauth2 https://issuer.example",
        );
        assert!(config.is_ok());
        if let Ok(config) = config {
            assert_eq!(config.basic.http_ingress, "https://example.org");
            assert_eq!(config.basic.msg_store.driver, MsgStoreDriver::Db);
            assert_eq!(config.auth.len(), 2);
        }
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    #[expect(
        clippy::indexing_slicing,
        reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
    )]
    fn full_config() {
        let cfg = parse(
            r#"
listen ircs://
listen irc+insecure://:6667
hostname example.org
title "My bouncer"
tls cert.pem key.pem
db sqlite3 /var/lib/soju/main.db
message-store db
auth oauth2 https://id:secret@auth.example.org
http-origin https://*.example.org
accept-proxy-ip localhost 10.0.0.1/8
accept-proxy-unix true
max-user-networks 5
upstream-user-ip 192.168.0.0/24 fd00::/64
disable-inactive-user 30d
message-expiry 0.5d
chathistory-limit 100
enable-user-on-auth true
client-cert-auth true
"#,
        )
        .unwrap();
        assert_eq!(cfg.listen, vec!["ircs://", "irc+insecure://:6667"]);
        assert_eq!(cfg.basic.hostname, "example.org");
        assert_eq!(cfg.basic.title, "My bouncer");
        assert_eq!(cfg.basic.http_ingress, "https://example.org");
        assert_eq!(cfg.db.source, "/var/lib/soju/main.db");
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Db);
        assert_eq!(cfg.auth.len(), 1);
        assert_eq!(cfg.auth[0].driver, AuthDriver::OAuth2);
        assert!(
            cfg.basic
                .accept_proxy_ips
                .contains("127.0.0.1".parse().unwrap())
        );
        assert!(
            cfg.basic
                .accept_proxy_ips
                .contains("::ffff:10.1.2.3".parse().unwrap())
        );
        assert!(
            !cfg.basic
                .accept_proxy_ips
                .contains("11.0.0.1".parse().unwrap())
        );
        assert!(cfg.basic.accept_proxy_unix);
        assert_eq!(cfg.basic.max_user_networks, 5);
        assert_eq!(cfg.basic.chat_history_limit, 100);
        assert_eq!(cfg.basic.upstream_user_ips.len(), 2);
        assert_eq!(
            cfg.basic.disable_inactive_users_delay,
            Duration::from_hours(720)
        );
        assert_eq!(cfg.basic.message_expiry, Duration::from_hours(12));
        assert!(cfg.basic.enable_users_on_auth);
        assert!(cfg.basic.client_cert_auth);
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    #[expect(
        clippy::indexing_slicing,
        reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
    )]
    fn defaults() {
        let cfg = parse("").unwrap();
        assert_eq!(cfg.db.driver, "sqlite3");
        assert_eq!(cfg.db.source, "soju.db");
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Db);
        assert_eq!(cfg.basic.max_user_networks, -1);
        assert_eq!(cfg.basic.max_accepted_connections, 256);
        assert_eq!(cfg.basic.chat_history_limit, 1000);
        assert_eq!(cfg.auth[0].driver, AuthDriver::Internal);
    }

    #[test]
    fn errors() {
        assert!(parse("unknown x").is_err());
        assert!(parse("hostname a\nhostname b").is_err());
        assert!(parse("tls a").is_err());
        assert!(parse("db postgres x").is_err());
        assert!(parse("message-store db x").is_err());
        assert!(parse("auth http").is_err());
        assert!(parse("disable-inactive-user 30").is_err());
        assert!(parse("upstream-user-ip 1.2.3.0/24 4.5.6.0/24").is_err());
        assert!(parse("max-user-networks x").is_err());
        for value in ["0", "-1", "x", "18446744073709551616"] {
            assert!(parse(&format!("max-accepted-connections {value}")).is_err());
        }
        assert!(
            parse("max-accepted-connections 17")
                .is_ok_and(|cfg| cfg.basic.max_accepted_connections == 17)
        );
        for bad in ["0", "99", "1001", "-1", "x"] {
            assert!(parse(&format!("chathistory-limit {bad}")).is_err(), "{bad}");
        }
        assert!(parse("enable-user-on-auth maybe").is_err());
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn durations() {
        assert_eq!(parse_duration("0d").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("1.5d").unwrap(), Duration::from_hours(36));
        assert!(parse_duration("5").is_err());
        for s in ["-1d", "infd", "nand", "1e20d", "1e9d"] {
            let err = parse_duration(s).unwrap_err().to_string();
            assert_eq!(err, "duration must be positive", "{s}");
        }
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn log_alias() {
        let cfg = parse("log memory").unwrap();
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Memory);
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn fs_message_store_unsupported() {
        for c in [
            "message-store fs /tmp/logs",
            "log fs /tmp/logs",
            "message-store fs",
        ] {
            let err = parse(c).unwrap_err().to_string();
            assert_eq!(
                err, "directive message-store: driver \"fs\" is not supported",
                "{c}"
            );
        }
    }
}
