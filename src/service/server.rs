//! BouncerServ server commands.

use anyhow::{Result, bail};

use super::ServiceContext;
use crate::user::{Event, User};
use crate::{logf, msg};

pub(super) fn handle_server_status(
    _u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if !params.is_empty() {
        bail!("expected no argument");
    }
    let db_stats = ctx.srv.db.stats()?;
    let s = ctx.srv.stats();
    ctx.print(format!(
        "{}/{} users, {} downstreams, {} upstreams, {} networks, {} channels",
        s.users, db_stats.users, s.downstreams, s.upstreams, db_stats.networks, db_stats.channels
    ));
    Ok(())
}

pub(super) fn handle_server_notice(
    u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let text = &params[0];
    let logger = match &u {
        Some(u) => u.logger.clone(),
        None => ctx.srv.logger.clone(),
    };
    logf!(logger, "broadcasting bouncer-wide NOTICE: {text}");

    let hostname = ctx.srv.config().basic.hostname.clone();
    let m = msg!("NOTICE", format!("${hostname}"), text.clone())
        .with_prefix(crate::downstream::service_prefix());
    let mut sent = 0;
    let mut total = 0;
    ctx.srv.for_each_user(|h| {
        total += 1;
        if h.send(Event::Broadcast(m.clone())) {
            sent += 1;
        }
    });
    logf!(
        logger,
        "broadcast bouncer-wide NOTICE to {sent}/{total} downstreams"
    );
    ctx.print(format!("sent to {sent}/{total} downstream connections"));
    Ok(())
}

pub(super) fn handle_server_debug(
    _u: Option<&mut User>,
    ctx: &mut ServiceContext,
    params: &[String],
) -> Result<()> {
    if params.len() != 1 {
        bail!("expected exactly one argument");
    }
    let Some(enabled) = crate::flags::parse_go_bool(&params[0]) else {
        bail!("strconv.ParseBool: parsing {:?}: invalid syntax", params[0]);
    };
    let previous = ctx.srv.logger.set_debug(enabled);
    if previous != enabled {
        if enabled {
            logf!(ctx.srv.logger, "enabling debug logging");
        } else {
            logf!(ctx.srv.logger, "disabling debug logging");
        }
    }
    Ok(())
}
