//! Commands whose replies are routed back to the downstream which sent them.

use super::{
    DownstreamConn, Duration, ERR_SASLABORTED, ERR_UNKNOWNCOMMAND, EndedEarly, HashMap, Instant,
    Message, PendingCommand, PendingCommandKind, RPL_ENDOFWHO, RPL_ENDOFWHOIS, RPL_LISTEND, Result,
    UpstreamConn, User, logf, msg,
};

/// Replies to downstreams waiting on pending commands when the upstream
/// connection goes away.
pub fn abort_pending_commands<S: std::hash::BuildHasher>(
    dcs: &mut [DownstreamConn],
    pending: HashMap<String, Vec<PendingCommand>, S>,
) {
    for l in pending.into_values() {
        for cmd in l {
            let Some(dc) = dcs.iter_mut().find(|d| d.id == cmd.downstream_id) else {
                continue;
            };
            let previous_label = dc.label.take();
            if !cmd.downstream_label.is_empty() && dc.caps.is_enabled("labeled-response") {
                dc.label = Some(crate::downstream::LabelContext {
                    label: cmd.downstream_label.clone(),
                    ..Default::default()
                });
            }
            let nick = dc.nick.clone();
            match cmd.kind {
                PendingCommandKind::List => {
                    dc.send_message(msg!(RPL_LISTEND, nick, "Command aborted"));
                }
                PendingCommandKind::Who => {
                    let mask = cmd
                        .msg
                        .params
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "*".to_owned());
                    dc.send_message(msg!(RPL_ENDOFWHO, nick, mask, "Command aborted"));
                }
                PendingCommandKind::Whois => {
                    let target = cmd.msg.params.last().cloned().unwrap_or_default();
                    dc.send_message(msg!(RPL_ENDOFWHOIS, nick, target, "Command aborted"));
                }
                PendingCommandKind::Authenticate => {
                    dc.end_sasl(Some(msg!(
                        ERR_SASLABORTED,
                        nick,
                        "SASL authentication aborted"
                    )));
                }
                PendingCommandKind::Register | PendingCommandKind::Verify => {
                    let account = cmd
                        .msg
                        .params
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "*".to_owned());
                    dc.send_message(msg!(
                        "FAIL",
                        cmd.msg.command.clone(),
                        "TEMPORARILY_UNAVAILABLE",
                        account,
                        "Command aborted"
                    ));
                }
                PendingCommandKind::Unsupported => dc.send_message(msg!(
                    ERR_UNKNOWNCOMMAND,
                    nick,
                    cmd.msg.command.clone(),
                    "Unsupported pending command"
                )),
            }
            dc.flush_batch();
            dc.label = previous_label;
        }
    }
}

fn send_next_pending_command(uc: &mut UpstreamConn, cmd: &str) {
    let kind = PendingCommandKind::parse(cmd);
    uc.pending_deadlines.remove(&kind);
    let Some(first) = uc.pending_cmds.get_mut(cmd).and_then(|l| l.first_mut()) else {
        return;
    };
    first.sent_at = Some(Instant::now());
    let (id, label, msg) = (
        first.downstream_id,
        first.downstream_label.clone(),
        first.msg.clone(),
    );
    uc.pending_deadlines.insert(
        kind,
        super::PendingDeadline {
            at: tokio::time::Instant::now() + Duration::from_secs(30),
            epoch: std::sync::Arc::new(()),
        },
    );
    uc.send_message_labeled(id, &label, msg);
}

/// Maximum number of queued commands of one type, e.g. WHO, per upstream
/// connection.
const MAX_PENDING_COMMANDS: usize = 64;

/// Queues a command whose replies need to be routed to a downstream
/// connection. label is the downstream's label for the command.
///
/// When the command is unsupported or too many of its type are queued, it is
/// returned instead: the caller is expected to abort it.
///
/// # Errors
///
/// Returns command validation errors, unavailable connection-state errors, or backend failures encountered while handling the command.
pub fn enqueue_command(
    uc: &mut UpstreamConn,
    dc_id: u64,
    label: String,
    msg: Message,
) -> Result<(), Box<PendingCommand>> {
    let pending = PendingCommand {
        kind: PendingCommandKind::parse(&msg.command),
        downstream_id: dc_id,
        downstream_label: label,
        msg,
        sent_at: None,
    };
    if pending.kind == PendingCommandKind::Unsupported || uc.conn.is_closed() {
        return Err(Box::new(pending));
    }
    let cmd = pending.msg.command.clone();
    let l = uc.pending_cmds.entry(cmd.clone()).or_default();

    if l.len() >= MAX_PENDING_COMMANDS {
        return Err(Box::new(pending));
    }
    l.push(pending);

    // Send the head of the queue unless it's already waiting for replies.
    if l.first().is_some_and(|p| p.sent_at.is_none()) {
        // The queue was idle: the end reply of a command ended early would
        // have arrived by now
        clear_ended_early(uc, &cmd);
        send_next_pending_command(uc, &cmd);
    }
    Ok(())
}

/// Returns the current pending command.
pub(super) fn current_pending_command<'a>(
    u: &'a User,
    ni: usize,
    cmd: &str,
) -> Option<&'a PendingCommand> {
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        u.networks[ni].conn.as_ref()?.pending_cmds.get(cmd)?.first()
    }
}

/// Returns the downstream ID of the current pending command.
pub(super) fn current_pending_downstream(u: &User, ni: usize, cmd: &str) -> Option<u64> {
    current_pending_command(u, ni, cmd).map(|p| p.downstream_id)
}

/// Removes the current pending command and sends the next one. Returns the
/// downstream ID and message of the removed command.
pub(super) fn dequeue_command(u: &mut User, ni: usize, cmd: &str) -> Option<(u64, Message)> {
    let uc = uc!(u, ni);
    let removed = uc
        .pending_cmds
        .get_mut(cmd)
        .filter(|l| !l.is_empty())
        .map(|l| l.remove(0));
    send_next_pending_command(uc, cmd);
    removed.map(|p| (p.downstream_id, p.msg))
}

/// Dequeues a command ended by an early error reply, e.g. `ERR_NOSUCHNICK`
/// for WHOIS or `RPL_TRYAGAIN` for WHO. Servers usually still send the end
/// reply of the command afterwards: `take_ended_early` routes it to the same
/// downstream, instead of letting it end the next command in the queue.
pub(super) fn end_command_early(u: &mut User, ni: usize, cmd: &str) -> Option<(u64, Message)> {
    let (dc_id, msg) = dequeue_command(u, ni, cmd)?;
    let target = match cmd {
        "WHO" => msg.params.first().map_or("*", String::as_str),
        "WHOIS" => msg.params.last().map_or("", String::as_str),
        _ => return Some((dc_id, msg)),
    };
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let target = u.networks[ni].casemap(target);
    uc!(u, ni).ended_early = Some(EndedEarly {
        command: cmd.to_owned(),
        downstream_id: dc_id,
        target,
    });
    Some((dc_id, msg))
}

/// Returns the downstream of the command ended early, if an end reply for
/// target belongs to it.
pub(super) fn take_ended_early(u: &mut User, ni: usize, cmd: &str, target: &str) -> Option<u64> {
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let target = u.networks[ni].casemap(target);
    let uc = uc!(u, ni);
    let e = uc.ended_early.as_ref()?;
    if e.command != cmd || e.target != target {
        return None;
    }
    uc.ended_early.take().map(|e| e.downstream_id)
}

/// Forgets the command ended early, once replies to a later command of the
/// same type arrive: servers reply in order, so its end reply won't come.
pub(super) fn clear_ended_early(uc: &mut UpstreamConn, cmd: &str) {
    if uc.ended_early.as_ref().is_some_and(|e| e.command == cmd) {
        uc.ended_early = None;
    }
}

impl UpstreamConn {
    pub(crate) fn clear_pending_deadlines(&mut self) {
        self.pending_deadlines.clear();
    }
}

impl User {
    pub(crate) fn next_pending_deadline(&self) -> Option<tokio::time::Instant> {
        self.networks
            .iter()
            .filter_map(|network| network.conn.as_ref())
            .filter(|conn| !conn.conn.is_closed())
            .flat_map(|conn| conn.pending_deadlines.values().map(|deadline| deadline.at))
            .min()
    }

    pub(crate) fn expired_pending_event(&self) -> Option<crate::user::Event> {
        self.networks
            .iter()
            .filter_map(|network| network.conn.as_ref())
            .filter(|conn| !conn.conn.is_closed())
            .flat_map(|conn| {
                conn.pending_deadlines
                    .iter()
                    .map(move |(kind, deadline)| (conn, *kind, deadline))
            })
            .filter(|(_, _, deadline)| deadline.at <= tokio::time::Instant::now())
            .min_by_key(|(_, _, deadline)| deadline.at)
            .map(
                |(conn, kind, deadline)| crate::user::Event::PendingCommandTimeout {
                    net_id: conn.net_id,
                    conn_id: conn.id,
                    kind,
                    epoch: deadline.epoch.clone(),
                },
            )
    }
}

/// Aborts every serial queue on this upstream and reconnects after an active
/// command deadline. Closing the transport isolates late unlabelled replies
/// from subsequent requests; connection/command epochs reject stale events.
pub fn pending_command_timeout(
    u: &mut User,
    net_id: i64,
    conn_id: u64,
    kind: PendingCommandKind,
    epoch: &std::sync::Arc<()>,
) {
    let Some(conn) = u
        .networks
        .iter_mut()
        .find(|network| network.id() == net_id)
        .and_then(|network| network.conn.as_mut())
    else {
        return;
    };
    if conn.id != conn_id
        || conn.conn.is_closed()
        || !conn.pending_deadlines.get(&kind).is_some_and(|deadline| {
            std::sync::Arc::ptr_eq(&deadline.epoch, epoch)
                && deadline.at <= tokio::time::Instant::now()
        })
    {
        return;
    }
    logf!(
        conn.logger,
        "pending command timed out; reconnecting upstream: {kind:?}"
    );
    crate::metrics::inc(&u.srv.metrics.pending_command_timeouts_total);
    let pending = std::mem::take(&mut conn.pending_cmds);
    conn.pending_deadlines.clear();
    conn.conn.close();
    abort_pending_commands(&mut u.downstreams, pending);
}
