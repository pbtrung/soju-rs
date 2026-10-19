//! Configuration file loading.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use ipnet::IpNet;

use crate::scfg::{self, Block, Directive};

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
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.iter().any(|n| n.contains(&ip))
    }
}

/// The loopback networks 127.0.0.0/8 and ::1/128.
fn loopback_ips() -> Vec<IpNet> {
    vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()]
}

fn parse_duration(s: &str) -> Result<Duration> {
    let Some(v) = s.strip_suffix('d') else {
        bail!("missing 'd' suffix in duration");
    };
    let v: f64 = v.parse().map_err(|e| anyhow!("invalid duration: {e}"))?;
    if v < 0.0 {
        bail!("duration must be positive");
    }
    Ok(Duration::from_secs_f64(v * 24.0 * 3600.0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgStoreDriver {
    Memory,
    Db,
    Fs,
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
    pub upstream_user_ips: Vec<IpNet>,
    pub disable_inactive_users_delay: Duration,
    pub message_expiry: Duration,
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

pub fn system_hostname() -> String {
    match gethostname::gethostname().into_string() {
        Ok(h) if !h.is_empty() => h,
        _ => "localhost".to_owned(),
    }
}

impl Server {
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
                upstream_user_ips: Vec::new(),
                disable_inactive_users_delay: Duration::ZERO,
                message_expiry: Duration::ZERO,
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

pub fn load(filename: &str) -> Result<Server> {
    let s = std::fs::read_to_string(filename)?;
    parse(&s)
}

fn dir_err(d: &Directive, msg: &str) -> anyhow::Error {
    anyhow!("line {}, directive {:?}: {}", d.lineno, d.name, msg)
}

/// Gets the single directive with this name, if any. Mirrors go-scfg which
/// rejects non-repeatable directives specified more than once.
fn single<'a>(dirs: &[&'a Directive]) -> Result<Option<&'a Directive>> {
    if dirs.len() > 1 {
        return Err(dir_err(
            dirs[1],
            "directive must not be specified more than once",
        ));
    }
    let d = dirs.first().copied();
    if let Some(d) = d
        && d.children.is_some()
    {
        return Err(dir_err(d, "directive requires zero children"));
    }
    Ok(d)
}

fn single_string(dirs: &[&Directive]) -> Result<Option<String>> {
    match single(dirs)? {
        Some(d) => {
            if d.params.len() != 1 {
                return Err(dir_err(d, "directive requires exactly one parameter"));
            }
            Ok(Some(d.params[0].clone()))
        }
        None => Ok(None),
    }
}

fn single_list(dirs: &[&Directive]) -> Result<Option<Vec<String>>> {
    Ok(single(dirs)?.map(|d| d.params.clone()))
}

fn single_pair(dirs: &[&Directive]) -> Result<Option<(String, String)>> {
    match single(dirs)? {
        Some(d) => {
            if d.params.len() != 2 {
                return Err(dir_err(d, "directive requires exactly 2 parameters"));
            }
            Ok(Some((d.params[0].clone(), d.params[1].clone())))
        }
        None => Ok(None),
    }
}

fn parse_bool(s: &str) -> Result<bool> {
    // Same as Go's strconv.ParseBool
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => bail!("strconv.ParseBool: parsing {s:?}: invalid syntax"),
    }
}

fn parse_driver_source(name: &str, params: &[String]) -> Result<(String, String)> {
    match params.len() {
        1 => Ok((params[0].clone(), String::new())),
        2 => Ok((params[0].clone(), params[1].clone())),
        _ => bail!("directive {name} requires exactly 1 or 2 parameters"),
    }
}

const KNOWN_DIRECTIVES: &[&str] = &[
    "listen",
    "hostname",
    "title",
    "icon",
    "motd",
    "tls",
    "db",
    "message-store",
    "log",
    "auth",
    "file-upload",
    "http-origin",
    "http-ingress",
    "accept-proxy-ip",
    "accept-proxy-unix",
    "max-user-networks",
    "upstream-user-ip",
    "disable-inactive-user",
    "message-expiry",
    "enable-user-on-auth",
    "client-cert-auth",
];

pub fn parse(s: &str) -> Result<Server> {
    let block: Block = scfg::parse(s)?;

    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
    for d in &block {
        if !KNOWN_DIRECTIVES.contains(&d.name.as_str()) {
            return Err(dir_err(d, "unknown directive"));
        }
    }

    let mut srv = Server::defaults();

    for d in get("listen") {
        if d.children.is_some() {
            return Err(dir_err(d, "directive requires zero children"));
        }
        if d.params.len() != 1 {
            return Err(dir_err(d, "directive requires exactly one parameter"));
        }
        srv.listen.push(d.params[0].clone());
    }
    if let Some(h) = single_string(&get("hostname"))?
        && !h.is_empty()
    {
        srv.basic.hostname = h;
    }
    srv.basic.title = single_string(&get("title"))?.unwrap_or_default();
    srv.icon = single_string(&get("icon"))?.unwrap_or_default();
    srv.motd_path = single_string(&get("motd"))?.unwrap_or_default();
    if let Some((cert, key)) = single_pair(&get("tls"))? {
        srv.tls = Some(Tls {
            cert_path: cert,
            key_path: key,
        });
    }
    if let Some((driver, source)) = single_pair(&get("db"))? {
        match driver.as_str() {
            "sqlite3" => {}
            "postgres" => bail!("directive db: driver \"postgres\" is not supported"),
            _ => bail!("directive db: unknown driver {driver:?}"),
        }
        srv.db = Db { driver, source };
    }

    let mut msg_store = single_list(&get("message-store"))?;
    if msg_store.is_none() {
        msg_store = single_list(&get("log"))?;
    }
    if let Some(params) = msg_store {
        let (driver, source) = parse_driver_source("message-store", &params)?;
        let driver = match driver.as_str() {
            "memory" => MsgStoreDriver::Memory,
            "db" => MsgStoreDriver::Db,
            "fs" => MsgStoreDriver::Fs,
            _ => bail!("directive message-store: unknown driver {driver:?}"),
        };
        match driver {
            MsgStoreDriver::Memory | MsgStoreDriver::Db if !source.is_empty() => {
                bail!(
                    "directive message-store: driver {:?} requires zero parameters",
                    params[0]
                )
            }
            MsgStoreDriver::Fs if source.is_empty() => {
                bail!(
                    "directive message-store: driver {:?} requires a source",
                    params[0]
                )
            }
            _ => {}
        }
        srv.basic.msg_store = MsgStore { driver, source };
    }

    let auth_dirs = get("auth");
    if !auth_dirs.is_empty() {
        // drop default auth if we are explicitly defining any auth
        srv.auth.clear();
    }
    for d in auth_dirs {
        if d.children.is_some() {
            return Err(dir_err(d, "directive requires zero children"));
        }
        let (driver, source) = parse_driver_source("auth", &d.params)?;
        let drv = match driver.as_str() {
            "internal" => AuthDriver::Internal,
            "pam" => AuthDriver::Pam,
            "http" => AuthDriver::Http,
            "oauth2" => AuthDriver::OAuth2,
            _ => bail!("directive auth: unknown driver {driver:?}"),
        };
        match drv {
            AuthDriver::Internal | AuthDriver::Pam if !source.is_empty() => {
                bail!("directive auth: driver {driver:?} requires zero parameters")
            }
            AuthDriver::Http | AuthDriver::OAuth2 if source.is_empty() => {
                bail!("directive auth: driver {driver:?} requires a source")
            }
            _ => {}
        }
        srv.auth.push(Auth {
            driver: drv,
            source,
        });
    }

    if let Some(params) = single_list(&get("file-upload"))? {
        parse_driver_source("file-upload", &params)?;
        srv.file_upload = true;
    }

    let origins = single_list(&get("http-origin"))?.unwrap_or_default();
    for origin in &origins {
        if crate::util::path_match(origin, origin).is_err() {
            bail!("directive http-origin: syntax error in pattern");
        }
    }
    srv.basic.http_origins = origins;

    match single_string(&get("http-ingress"))? {
        Some(s) if !s.is_empty() => srv.basic.http_ingress = s,
        _ => srv.basic.http_ingress = format!("https://{}", srv.basic.hostname),
    }

    for s in single_list(&get("accept-proxy-ip"))?.unwrap_or_default() {
        if s == "localhost" {
            srv.basic.accept_proxy_ips.0.extend(loopback_ips());
            continue;
        }
        let n = parse_cidr(&s)
            .map_err(|e| anyhow!("directive accept-proxy-ip: failed to parse CIDR: {e}"))?;
        srv.basic.accept_proxy_ips.0.push(n);
    }
    if let Some(s) = single_string(&get("accept-proxy-unix"))?
        && !s.is_empty()
    {
        srv.basic.accept_proxy_unix =
            parse_bool(&s).map_err(|e| anyhow!("directive accept-proxy-unix: {e}"))?;
    }
    if let Some(s) = single_string(&get("max-user-networks"))? {
        let d = get("max-user-networks")[0];
        srv.basic.max_user_networks = s.parse().map_err(|_| {
            anyhow!(
                "line {}, directive {:?}, parameter 1: invalid int parameter: strconv.ParseInt: parsing {:?}: invalid syntax",
                d.lineno,
                d.name,
                s
            )
        })?;
    }
    let (mut has_v4, mut has_v6) = (false, false);
    for s in single_list(&get("upstream-user-ip"))?.unwrap_or_default() {
        let n = parse_cidr(&s)
            .map_err(|e| anyhow!("directive upstream-user-ip: failed to parse CIDR: {e}"))?;
        match n {
            IpNet::V4(_) => {
                if has_v4 {
                    bail!("directive upstream-user-ip: found two IPv4 CIDRs");
                }
                has_v4 = true;
            }
            IpNet::V6(_) => {
                if has_v6 {
                    bail!("directive upstream-user-ip: found two IPv6 CIDRs");
                }
                has_v6 = true;
            }
        }
        srv.basic.upstream_user_ips.push(n);
    }
    if let Some(s) = single_string(&get("disable-inactive-user"))?
        && !s.is_empty()
    {
        srv.basic.disable_inactive_users_delay =
            parse_duration(&s).map_err(|e| anyhow!("directive disable-inactive-user: {e}"))?;
    }
    if let Some(s) = single_string(&get("message-expiry"))?
        && !s.is_empty()
    {
        srv.basic.message_expiry =
            parse_duration(&s).map_err(|e| anyhow!("directive message-expiry: {e}"))?;
    }
    if let Some(s) = single_string(&get("enable-user-on-auth"))?
        && !s.is_empty()
    {
        srv.basic.enable_users_on_auth =
            parse_bool(&s).map_err(|e| anyhow!("directive enable-user-on-auth: {e}"))?;
    }
    if let Some(s) = single_string(&get("client-cert-auth"))?
        && !s.is_empty()
    {
        srv.basic.client_cert_auth =
            parse_bool(&s).map_err(|e| anyhow!("directive client-cert-auth: {e}"))?;
    }

    Ok(srv)
}

/// Parses a CIDR, returning the network (host bits cleared), like Go's
/// net.ParseCIDR.
pub fn parse_cidr(s: &str) -> Result<IpNet> {
    let n: IpNet = s
        .parse()
        .map_err(|_| anyhow!("invalid CIDR address: {s}"))?;
    Ok(n.trunc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_config() {
        let cfg = parse(
            r#"
listen ircs://
listen irc+insecure://:6667
hostname example.org
title "My bouncer"
tls cert.pem key.pem
db sqlite3 /var/lib/soju/main.db
message-store fs /var/lib/soju/logs
auth oauth2 https://id:secret@auth.example.org
http-origin https://*.example.org
accept-proxy-ip localhost 10.0.0.1/8
accept-proxy-unix true
max-user-networks 5
upstream-user-ip 192.168.0.0/24 fd00::/64
disable-inactive-user 30d
message-expiry 0.5d
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
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Fs);
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
        assert_eq!(cfg.basic.upstream_user_ips.len(), 2);
        assert_eq!(
            cfg.basic.disable_inactive_users_delay,
            Duration::from_secs(30 * 86400)
        );
        assert_eq!(cfg.basic.message_expiry, Duration::from_secs(43200));
        assert!(cfg.basic.enable_users_on_auth);
        assert!(cfg.basic.client_cert_auth);
    }

    #[test]
    fn defaults() {
        let cfg = parse("").unwrap();
        assert_eq!(cfg.db.driver, "sqlite3");
        assert_eq!(cfg.db.source, "soju.db");
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Db);
        assert_eq!(cfg.basic.max_user_networks, -1);
        assert_eq!(cfg.auth[0].driver, AuthDriver::Internal);
    }

    #[test]
    fn errors() {
        assert!(parse("unknown x").is_err());
        assert!(parse("hostname a\nhostname b").is_err());
        assert!(parse("tls a").is_err());
        assert!(parse("db postgres x").is_err());
        assert!(parse("message-store fs").is_err());
        assert!(parse("message-store db x").is_err());
        assert!(parse("auth http").is_err());
        assert!(parse("disable-inactive-user 30").is_err());
        assert!(parse("upstream-user-ip 1.2.3.0/24 4.5.6.0/24").is_err());
        assert!(parse("max-user-networks x").is_err());
        assert!(parse("enable-user-on-auth maybe").is_err());
    }

    #[test]
    fn log_alias() {
        let cfg = parse("log fs /tmp/logs").unwrap();
        assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Fs);
    }
}
