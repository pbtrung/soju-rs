//! Commands whose replies are routed back to the downstream which sent them.

use super::*;

/// Replies to downstreams waiting on pending commands when the upstream
/// connection goes away.
pub fn abort_pending_commands(
    dcs: &mut [DownstreamConn],
    pending: HashMap<String, Vec<PendingCommand>>,
) {
    // TODO: support labeled-response
    for l in pending.into_values() {
        for cmd in l {
            let Some(dc) = dcs.iter_mut().find(|d| d.id == cmd.downstream_id) else {
                continue;
            };
            let nick = dc.nick.clone();
            match cmd.msg.command.as_str() {
                "LIST" => dc.send_message(msg!(RPL_LISTEND, nick, "Command aborted")),
                "WHO" => {
                    let mask = cmd
                        .msg
                        .params
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "*".to_owned());
                    dc.send_message(msg!(RPL_ENDOFWHO, nick, mask, "Command aborted"));
                }
                "WHOIS" => {
                    let target = cmd.msg.params.last().cloned().unwrap_or_default();
                    dc.send_message(msg!(RPL_ENDOFWHOIS, nick, target, "Command aborted"));
                }
                "AUTHENTICATE" => {
                    dc.end_sasl(Some(msg!(
                        ERR_SASLABORTED,
                        nick,
                        "SASL authentication aborted"
                    )));
                }
                "REGISTER" | "VERIFY" => {
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
                c => panic!("Unsupported pending command {c:?}"),
            }
        }
    }
}

fn send_next_pending_command(uc: &mut UpstreamConn, cmd: &str) {
    let Some(first) = uc.pending_cmds.get_mut(cmd).and_then(|l| l.first_mut()) else {
        return;
    };
    first.sent_at = Some(Instant::now());
    let (id, label, msg) = (
        first.downstream_id,
        first.downstream_label.clone(),
        first.msg.clone(),
    );
    uc.send_message_labeled(id, &label, msg);
}

/// Maximum number of queued commands of one type, e.g. WHO, per upstream
/// connection.
const MAX_PENDING_COMMANDS: usize = 64;

/// Queues a command whose replies need to be routed to a downstream
/// connection. label is the downstream's label for the command.
///
/// When too many commands of this type are already queued, the command is
/// returned instead: the caller is expected to abort it.
pub fn enqueue_command(
    uc: &mut UpstreamConn,
    dc_id: u64,
    label: String,
    msg: Message,
) -> Result<(), Box<PendingCommand>> {
    match msg.command.as_str() {
        "LIST" | "WHO" | "WHOIS" | "AUTHENTICATE" | "REGISTER" | "VERIFY" => {}
        c => panic!("Unsupported pending command {c:?}"),
    }
    let cmd = msg.command.clone();
    let l = uc.pending_cmds.entry(cmd.clone()).or_default();

    // If we didn't get a reply after a while, just give up
    // TODO: consider sending an abort reply to downstream
    let mut timed_out = 0;
    while timed_out < l.len() {
        match l[timed_out].sent_at {
            Some(t) if t.elapsed() > Duration::from_secs(30) => {}
            _ => break,
        }
        // Only log the command name: parameters may be secrets, e.g. the
        // password of REGISTER
        logf!(
            uc.logger,
            "timed out waiting for reply to pending command: {}",
            l[timed_out].msg.command
        );
        timed_out += 1;
    }
    if timed_out > 0 {
        l.drain(..timed_out);
    }

    let pending = PendingCommand {
        downstream_id: dc_id,
        downstream_label: label,
        msg,
        sent_at: None,
    };
    if l.len() >= MAX_PENDING_COMMANDS {
        return Err(Box::new(pending));
    }
    l.push(pending);

    // Send the head of the queue unless it's already waiting for replies.
    // After dropping timed out commands, it may not have been sent yet.
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
    u.networks[ni].conn.as_ref()?.pending_cmds.get(cmd)?.first()
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

/// Dequeues a command ended by an early error reply, e.g. ERR_NOSUCHNICK
/// for WHOIS or RPL_TRYAGAIN for WHO. Servers usually still send the end
/// reply of the command afterwards: take_ended_early routes it to the same
/// downstream, instead of letting it end the next command in the queue.
pub(super) fn end_command_early(u: &mut User, ni: usize, cmd: &str) -> Option<(u64, Message)> {
    let (dc_id, msg) = dequeue_command(u, ni, cmd)?;
    let target = match cmd {
        "WHO" => msg.params.first().map_or("*", String::as_str),
        "WHOIS" => msg.params.last().map_or("", String::as_str),
        _ => return Some((dc_id, msg)),
    };
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
