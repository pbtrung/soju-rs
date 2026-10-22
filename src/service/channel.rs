//! BouncerServ channel commands.

use anyhow::{Result, anyhow, bail};

use super::duration::{format_go_duration, parse_go_duration};
use super::{ServiceContext, no_args, parse_flags};
use crate::database as db;
use crate::downstream::ILLEGAL_CHAN_CHARS;
use crate::flags::{FlagSet, Parsed};
use crate::msg;
use crate::user::User;

pub(super) fn handle_channel_status(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    let default_name = ctx
        .network_id
        .and_then(|id| u.net_index(id))
        .map(|ni| u.networks[ni].get_name().to_owned())
        .unwrap_or_default();
    let p = parse_flags(&FlagSet::new().string("network"), params)?;
    no_args(&p)?;
    let network_name = p.string_or("network", &default_name);

    let mut n = 0;
    let mut send_network = |ctx: &mut ServiceContext, net: &crate::user::Network| {
        let mut channels: Vec<&db::Channel> = net.channels.values().collect();
        channels.sort_by_key(|c| c.name.replace('#', ""));
        for ch in channels {
            let uc = net.upstream();
            let mut name = ch.name.clone();
            if network_name.is_empty() {
                name = format!("{name}/{}", net.get_name());
            }
            let mut status = match uc {
                Some(uc) if uc.channels.has(&ch.name) => "joined",
                Some(_) => "parted",
                None => "disconnected",
            }
            .to_owned();
            if ch.detached {
                status.push_str(", detached");
            }
            ctx.print(format!(
                "{name} [{status}]: relay-detached: {}, reattach-on: {}, detach-after: {}, detach-on: {}",
                filter_name(ch.relay_detached),
                filter_name(ch.reattach_on),
                format_go_duration(ch.detach_after),
                filter_name(ch.detach_on),
            ));
            n += 1;
        }
    };

    if network_name.is_empty() {
        for net in &u.networks {
            send_network(ctx, net);
        }
    } else {
        let Some(ni) = u.get_network(&network_name) else {
            bail!("unknown network {network_name:?}");
        };
        send_network(ctx, &u.networks[ni]);
    }
    if n == 0 {
        ctx.print("No channel configured.");
    }
    Ok(())
}

fn parse_filter(filter: &str) -> Result<db::MessageFilter> {
    Ok(match filter {
        "default" => db::MessageFilter::Default,
        "none" => db::MessageFilter::None,
        "highlight" => db::MessageFilter::Highlight,
        "message" => db::MessageFilter::Message,
        _ => bail!("unknown filter: {filter:?}"),
    })
}

fn filter_name(filter: db::MessageFilter) -> &'static str {
    match filter {
        db::MessageFilter::Default => "default",
        db::MessageFilter::None => "none",
        db::MessageFilter::Highlight => "highlight",
        db::MessageFilter::Message => "message",
    }
}

fn channel_flag_set() -> FlagSet {
    FlagSet::new()
        .bool_value("detached")
        .string("relay-detached")
        .string("reattach-on")
        .string("detach-after")
        .string("detach-on")
}

fn update_channel_from_flags(p: &Parsed, ch: &mut db::Channel) -> Result<()> {
    if let Some(v) = p.get("relay-detached") {
        ch.relay_detached = parse_filter(v)?;
    }
    if let Some(v) = p.get("reattach-on") {
        ch.reattach_on = parse_filter(v)?;
    }
    if let Some(v) = p.get("detach-after") {
        ch.detach_after = parse_go_duration(v).ok_or_else(|| {
            anyhow!(
                "unknown duration for -detach-after {v:?} (duration format: 0, 300s, 22h30m, ...)"
            )
        })?;
    }
    if let Some(v) = p.get("detach-on") {
        ch.detach_on = parse_filter(v)?;
    }
    Ok(())
}

/// Splits "name/network" when the caller isn't bound to a network.
fn strip_network_suffix(u: &User, ctx: &ServiceContext, name: &str) -> Result<(String, usize)> {
    if let Some(ni) = ctx.network_id.and_then(|id| u.net_index(id)) {
        return Ok((name.to_owned(), ni));
    }
    let Some((name, net_name)) = name.split_once('/') else {
        bail!("missing network name");
    };
    match u.networks.iter().position(|n| n.get_name() == net_name) {
        Some(ni) => Ok((name.to_owned(), ni)),
        None => bail!("unknown network {net_name:?}"),
    }
}

pub(super) fn handle_channel_create(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.is_empty() {
        bail!("expected at least one argument");
    }
    let p = parse_flags(&channel_flag_set(), &params[1..])?;
    no_args(&p)?;
    let (name, ni) = strip_network_suffix(u, ctx, &params[0])?;
    if name.is_empty() || name.contains(ILLEGAL_CHAN_CHARS) {
        bail!("invalid channel name: {name}");
    }
    let net = &mut u.networks[ni];
    if let Some(uc) = net.upstream()
        && !uc.is_channel(&name)
    {
        bail!("not a channel name: {name}");
    }
    if net.channels.has(&name) {
        bail!("channel {name:?} already exists");
    }
    let mut ch = db::Channel {
        name: name.clone(),
        ..Default::default()
    };
    update_channel_from_flags(&p, &mut ch)?;
    // The channel isn't joined yet: no need to detach it from clients
    ch.detached = p.get_bool("detached").unwrap_or(false);
    let net_id = net.id();
    net.channels.set(&name, ch);
    let ch = net.channels.get_mut(&name).unwrap();
    ctx.srv
        .db
        .store_channel(net_id, ch)
        .map_err(|e| anyhow!("failed to create channel: {e}"))?;
    if let Some(uc) = net.upstream() {
        uc.send_message(msg!("JOIN", name.clone()));
    }
    ctx.print(format!("created channel {name:?}"));
    Ok(())
}

pub(super) fn handle_channel_update(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.is_empty() {
        bail!("expected at least one argument");
    }
    let p = parse_flags(&channel_flag_set(), &params[1..])?;
    no_args(&p)?;
    let (name, ni) = strip_network_suffix(u, ctx, &params[0])?;
    let Some(ch) = u.networks[ni].channels.get_mut(&name) else {
        bail!("unknown channel {name:?}");
    };
    update_channel_from_flags(&p, ch)?;
    let detached = ch.detached;
    if let Some(d) = p.get_bool("detached")
        && d != detached
    {
        if d {
            u.detach_channel(ni, &name);
        } else {
            u.attach_channel(ni, &name);
        }
    }
    crate::upstream::update_channel_auto_detach(u, ni, &name);
    let net_id = u.networks[ni].id();
    let ch = u.networks[ni].channels.get_mut(&name).unwrap();
    ctx.srv
        .db
        .store_channel(net_id, ch)
        .map_err(|e| anyhow!("failed to update channel: {e}"))?;
    ctx.print(format!("updated channel {name:?}"));
    Ok(())
}

pub(super) fn handle_channel_delete(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    let u = u.unwrap();
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let (name, ni) = strip_network_suffix(u, ctx, &params[0])?;
    u.delete_channel(ni, &name)
        .map_err(|e| anyhow!("failed to delete channel: {e}"))?;
    if let Some(uc) = u.networks[ni].upstream()
        && uc.channels.has(&name)
    {
        uc.send_message(msg!("PART", name.clone()));
    }
    ctx.print(format!("deleted channel {name:?}"));
    Ok(())
}
