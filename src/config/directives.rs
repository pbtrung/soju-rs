//! Configuration directive arity, driver validation and ordered setting groups.
use super::{
    Auth, AuthDriver, Db, MAX_CHAT_HISTORY_LIMIT, MIN_CHAT_HISTORY_LIMIT, MsgStore, MsgStoreDriver,
    Server, Tls, loopback_ips, parse_cidr, parse_duration,
};
use crate::scfg::{self, Block, Directive};
use anyhow::{Result, anyhow, bail};
use ipnet::IpNet;

fn dir_err(d: &Directive, msg: &str) -> anyhow::Error {
    anyhow!("line {}, directive {:?}: {}", d.lineno, d.name, msg)
}

/// Gets the single directive with this name, if any. Mirrors go-scfg which
/// rejects non-repeatable directives specified more than once.
fn single<'a>(dirs: &[&'a Directive]) -> Result<Option<&'a Directive>> {
    if dirs.len() > 1 {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Directive arity is checked before selecting its argument."
            )]
            return Err(dir_err(
                dirs[1],
                "directive must not be specified more than once",
            ));
        }
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
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                Ok(Some(d.params[0].clone()))
            }
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
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                Ok(Some((d.params[0].clone(), d.params[1].clone())))
            }
        }
        None => Ok(None),
    }
}

fn parse_bool(s: &str) -> Result<bool> {
    crate::flags::parse_go_bool(s)
        .ok_or_else(|| anyhow!("strconv.ParseBool: parsing {s:?}: invalid syntax"))
}

fn parse_driver_source(name: &str, params: &[String]) -> Result<(String, String)> {
    match params.len() {
        #[expect(
            clippy::indexing_slicing,
            reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
        )]
        1 => Ok((params[0].clone(), String::new())),
        #[expect(
            clippy::indexing_slicing,
            reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
        )]
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
    "max-accepted-connections",
    "upstream-user-ip",
    "disable-inactive-user",
    "message-expiry",
    "chathistory-limit",
    "enable-user-on-auth",
    "client-cert-auth",
];

/// Loads and validates directives in their established error-precedence order.
///
/// # Errors
///
/// Returns an error for malformed, unknown, duplicate or invalid directives.
pub fn parse(source: &str) -> Result<Server> {
    let block = scfg::parse(source)?;
    for directive in &block {
        if !KNOWN_DIRECTIVES.contains(&directive.name.as_str()) {
            return Err(dir_err(directive, "unknown directive"));
        }
    }
    let mut server = Server::defaults();
    parse_identity(&mut server, &block)?;
    parse_storage(&mut server, &block)?;
    parse_authentication(&mut server, &block)?;
    parse_http(&mut server, &block)?;
    parse_connections(&mut server, &block)?;
    parse_lifecycle(&mut server, &block)?;
    Ok(server)
}

fn parse_identity(srv: &mut Server, block: &Block) -> Result<()> {
    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
    for d in get("listen") {
        if d.children.is_some() {
            return Err(dir_err(d, "directive requires zero children"));
        }
        if d.params.len() != 1 {
            return Err(dir_err(d, "directive requires exactly one parameter"));
        }
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            srv.listen.push(d.params[0].clone());
        }
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
    Ok(())
}

fn parse_storage(srv: &mut Server, block: &Block) -> Result<()> {
    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
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
            "fs" => bail!("directive message-store: driver \"fs\" is not supported"),
            _ => bail!("directive message-store: unknown driver {driver:?}"),
        };
        match driver {
            _ if !source.is_empty() => {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    bail!(
                        "directive message-store: driver {:?} requires zero parameters",
                        params[0]
                    )
                }
            }
            _ => {}
        }
        srv.basic.msg_store = MsgStore { driver, source };
    }

    Ok(())
}

fn parse_authentication(srv: &mut Server, block: &Block) -> Result<()> {
    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
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

    Ok(())
}

fn parse_http(srv: &mut Server, block: &Block) -> Result<()> {
    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
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

    Ok(())
}

fn parse_connections(srv: &mut Server, block: &Block) -> Result<()> {
    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
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
    if let Some(value) = single_string(&get("max-accepted-connections"))? {
        let limit = value.parse::<usize>()?;
        if limit == 0 {
            bail!("max-accepted-connections must be positive");
        }
        srv.basic.max_accepted_connections = limit;
    }
    let d = get("max-user-networks");
    if let Some(s) = single_string(&d)? {
        #[expect(
            clippy::indexing_slicing,
            reason = "Directive arity is checked before selecting its argument."
        )]
        let d = d[0];
        srv.basic.max_user_networks = s.parse::<i64>().map_err(|error| {
            anyhow::anyhow!(error).context({
            anyhow!(
                "line {}, directive {:?}, parameter 1: invalid int parameter: strconv.ParseInt: \
                    parsing {:?}: invalid syntax",
                d.lineno,
                d.name,
                s
            )
        })
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
    Ok(())
}

fn parse_lifecycle(srv: &mut Server, block: &Block) -> Result<()> {
    let get = |name: &str| -> Vec<&Directive> { block.iter().filter(|d| d.name == name).collect() };
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
    if let Some(s) = single_string(&get("chathistory-limit"))? {
        srv.basic.chat_history_limit = match s.parse() {
            Ok(n) if (MIN_CHAT_HISTORY_LIMIT..=MAX_CHAT_HISTORY_LIMIT).contains(&n) => n,
            _ => bail!(
                "directive chathistory-limit: must be an integer between {MIN_CHAT_HISTORY_LIMIT} \
                    and {MAX_CHAT_HISTORY_LIMIT}"
            ),
        };
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
    Ok(())
}
