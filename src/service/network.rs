//! BouncerServ network commands.

use anyhow::{Result, anyhow, bail};

use super::{ServiceContext, no_args, parse_flags, pop_arg};
use crate::database as db;
use crate::flags::{FlagSet, Parsed};
use crate::irc::Message;
use crate::user::User;

/// Returns the network named by the first argument, or the current one.
fn get_network_from_arg<'a>(
    u: &User,
    ctx: &ServiceContext,
    params: &'a [String],
) -> Result<(usize, &'a [String])> {
    let (name, rest) = pop_arg(params);
    if name.is_empty() {
        let Some(ni) = ctx.network_id.and_then(|id| u.net_index(id)) else {
            bail!("no network selected, a name argument is required");
        };
        return Ok((ni, rest));
    }
    let Some(ni) = u.get_network(&name) else {
        bail!("unknown network {name:?}");
    };
    Ok((ni, rest))
}

/// Returns the network named by the -network flag, or the current one.
pub(super) fn get_network_from_flag(u: &User, ctx: &ServiceContext, p: &Parsed) -> Result<usize> {
    let name = p.get("network").unwrap_or("");
    if name.is_empty() {
        return ctx
            .network_id
            .and_then(|id| u.net_index(id))
            .ok_or_else(|| anyhow!("no network selected, -network is required"));
    }
    u.get_network(name)
        .ok_or_else(|| anyhow!("unknown network {name:?}"))
}

fn network_flag_set() -> FlagSet {
    FlagSet::new()
        .string("addr")
        .string("name")
        .string("nick")
        .string("username")
        .string("pass")
        .string("realname")
        .string("certfp")
        .bool_value("auto-away")
        .bool_value("enabled")
        .bool("ignore-limit")
        .string("connect-command")
}

fn update_network_from_flags(p: &Parsed, network: &mut db::Network) -> Result<()> {
    if let Some(addr) = p.get("addr") {
        if let Some((scheme, _)) = addr.split_once("://")
            && !matches!(scheme, "ircs" | "irc+insecure" | "irc+unix")
        {
            bail!("unknown scheme {scheme:?} (supported schemes: ircs, irc+insecure, irc+unix)");
        }
        network.addr = addr.to_owned();
    }
    if let Some(name) = p.get("name") {
        if name == "*" {
            bail!("the network name {name:?} is reserved");
        }
        network.name = name.to_owned();
    }
    if let Some(v) = p.get("nick") {
        network.nick = v.to_owned();
    }
    if let Some(v) = p.get("username") {
        network.username = v.to_owned();
    }
    if let Some(v) = p.get("pass") {
        network.pass = v.to_owned();
    }
    if let Some(v) = p.get("realname") {
        network.realname = v.to_owned();
    }
    if let Some(v) = p.get("certfp") {
        let certfp = v.replace(':', "").to_lowercase();
        if hex::decode(&certfp).is_err() {
            bail!("the certificate fingerprint must be hex-encoded");
        }
        network.certfp = match certfp.len() {
            0 => String::new(),
            64 => format!("sha-256:{certfp}"),
            128 => format!("sha-512:{certfp}"),
            _ => bail!("the certificate fingerprint must be a SHA256 or SHA512 hash"),
        };
    }
    if let Some(v) = p.get_bool("auto-away") {
        network.auto_away = v;
    }
    if let Some(v) = p.get_bool("enabled") {
        network.enabled = v;
    }
    if let Some(cmds) = p.get_all("connect-command") {
        if cmds.len() == 1 && cmds[0].is_empty() {
            network.connect_commands.clear();
        } else {
            if cmds.len() > 20 {
                bail!("too many -connect-command flags supplied");
            }
            for command in cmds {
                if let Err(e) = Message::parse(command) {
                    bail!(
                        "flag -connect-command must be a valid raw irc command string: {command:?}: {e}"
                    );
                }
            }
            network.connect_commands = cmds.to_vec();
        }
    }
    Ok(())
}

pub(super) fn handle_network_create(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let p = parse_flags(&network_flag_set(), params)?;
    no_args(&p)?;
    let Some(addr) = p.get("addr") else {
        bail!("flag -addr is required");
    };
    let ignore_limit = p.get_bool("ignore-limit").unwrap_or(false);
    if ignore_limit && !ctx.admin {
        bail!("you must be an admin to use the flag -ignore-limit");
    }
    let mut record = db::Network::new(addr);
    update_network_from_flags(&p, &mut record)?;
    let ni = u
        .create_network(record, !ignore_limit)
        .map_err(|e| anyhow!("could not create network: {e}"))?;
    ctx.print(format!("created network {:?}", u.networks[ni].get_name()));
    Ok(())
}

pub(super) fn handle_network_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if !params.is_empty() {
        bail!("expected no argument");
    }
    let mut n = 0;
    for net in &u.networks {
        let mut statuses = Vec::new();
        let mut details = String::new();
        if let Some(uc) = net.upstream() {
            if !ctx.nick.is_empty() && ctx.nick != uc.nick {
                statuses.push(format!("connected as {}", uc.nick));
            } else {
                statuses.push("connected".to_owned());
            }
            details = format!("{} channels", uc.channels.len());
        } else if !net.record.enabled {
            statuses.push("disabled".to_owned());
        } else {
            statuses.push("disconnected".to_owned());
            if let Some(e) = &net.last_error {
                details = e.clone();
            }
        }
        if Some(net.id()) == ctx.network_id {
            statuses.push("current".to_owned());
        }
        let mut name = net.get_name().to_owned();
        if name != net.record.addr {
            name = format!("{name} ({})", net.record.addr);
        }
        let mut s = format!("{name} [{}]", statuses.join(", "));
        if !details.is_empty() {
            s = format!("{s}: {details}");
        }
        ctx.print(s);
        n += 1;
    }
    if n == 0 {
        ctx.print(r#"No network configured, add one with "network create"."#);
    }
    Ok(())
}

pub(super) fn handle_network_info(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let (ni, rest) = get_network_from_arg(u, ctx, params)?;
    if let Some(a) = rest.first() {
        bail!("unexpected argument: {a}");
    }
    let net = &u.networks[ni].record;
    ctx.print(format!("name: {}", net.get_name()));
    ctx.print(format!("addr: {}", net.addr));
    ctx.print(format!("nick: {}", db::get_nick(&u.record, Some(net))));
    ctx.print(format!(
        "username: {}",
        db::get_username(&u.record, Some(net))
    ));
    ctx.print(format!(
        "realname: {}",
        db::get_realname(&u.record, Some(net))
    ));
    // Secrets are never printed
    if !net.pass.is_empty() {
        ctx.print("pass: (set)");
    }
    if !net.certfp.is_empty() {
        ctx.print(format!("certfp: {}", net.certfp));
    }
    match net.sasl.mechanism.as_str() {
        "" => ctx.print("sasl: none"),
        "PLAIN" => ctx.print(format!(
            "sasl: PLAIN (username: {})",
            net.sasl.plain_username
        )),
        mech => ctx.print(format!("sasl: {mech}")),
    }
    ctx.print(format!("auto-away: {}", net.auto_away));
    ctx.print(format!("enabled: {}", net.enabled));
    for cmd in &net.connect_commands {
        ctx.print(format!("connect-command: {}", redact_command(cmd)));
    }
    Ok(())
}

/// Shows a raw IRC command without its last parameters, which may contain
/// secrets, e.g. "PRIVMSG NickServ :IDENTIFY <password>".
pub(super) fn redact_command(raw: &str) -> String {
    let Ok(m) = Message::parse(raw) else {
        return "(invalid)".to_owned();
    };
    match m.params.first() {
        Some(target) if m.params.len() > 1 => format!("{} {target} (parameters hidden)", m.command),
        Some(_) => format!("{} (parameters hidden)", m.command),
        None => m.command,
    }
}

pub(super) fn handle_network_update(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let (ni, params) = get_network_from_arg(u, ctx, params)?;
    let p = parse_flags(&network_flag_set(), params)?;
    no_args(&p)?;
    let ignore_limit = p.get_bool("ignore-limit").unwrap_or(false);
    if ignore_limit && !ctx.admin {
        bail!("you must be an admin to use the flag -ignore-limit");
    }
    let mut record = u.networks[ni].record.clone();
    let was_enabled = record.enabled;
    update_network_from_flags(&p, &mut record)?;
    let ni = u
        .update_network(record, !ignore_limit && !was_enabled)
        .map_err(|e| anyhow!("could not update network: {e}"))?;
    ctx.print(format!("updated network {:?}", u.networks[ni].get_name()));
    Ok(())
}

pub(super) fn handle_network_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let (ni, _) = get_network_from_arg(u, ctx, params)?;
    let id = u.networks[ni].id();
    let name = u.networks[ni].get_name().to_owned();
    u.delete_network(id)?;
    ctx.print(format!("deleted network {name:?}"));
    Ok(())
}

pub(super) fn handle_network_quote(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 && params.len() != 2 {
        bail!("expected one or two arguments");
    }
    let raw = params.last().unwrap().clone();
    let (ni, _) = get_network_from_arg(u, ctx, &params[..params.len() - 1])?;
    let name = u.networks[ni].get_name().to_owned();
    let Some(uc) = u.networks[ni].upstream() else {
        bail!("network {name:?} is not currently connected");
    };
    let m = Message::parse(&raw).map_err(|e| anyhow!("failed to parse command {raw:?}: {e}"))?;
    uc.send_message(m);
    ctx.print(format!("sent command to {name:?}"));
    Ok(())
}
