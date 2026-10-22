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
                    let account = cmd.msg.params.first().cloned().unwrap_or_default();
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

/// Queues a command whose replies need to be routed to a downstream
/// connection. label is the downstream's label for the command.
pub fn enqueue_command(uc: &mut UpstreamConn, dc_id: u64, label: String, msg: Message) {
    match msg.command.as_str() {
        "LIST" | "WHO" | "WHOIS" | "AUTHENTICATE" | "REGISTER" | "VERIFY" => {}
        c => panic!("Unsupported pending command {c:?}"),
    }
    let cmd = msg.command.clone();
    let l = uc.pending_cmds.entry(cmd.clone()).or_default();
    l.push(PendingCommand {
        downstream_id: dc_id,
        downstream_label: label,
        msg,
        sent_at: None,
    });

    // If we didn't get a reply after a while, just give up
    // TODO: consider sending an abort reply to downstream
    let mut timed_out = 0;
    while timed_out < l.len() {
        match l[timed_out].sent_at {
            Some(t) if t.elapsed() > Duration::from_secs(30) => {}
            _ => break,
        }
        logf!(
            uc.logger,
            "timed out waiting for reply to pending command: {:?}",
            l[timed_out].msg.to_string()
        );
        timed_out += 1;
    }
    if timed_out > 0 {
        l.drain(..timed_out);
    }

    // Send the head of the queue unless it's already waiting for replies.
    // After dropping timed out commands, it may not have been sent yet.
    if l.first().is_some_and(|p| p.sent_at.is_none()) {
        send_next_pending_command(uc, &cmd);
    }
}

/// Returns the downstream ID and message of the current pending command.
pub(super) fn current_pending_command(
    u: &mut User,
    ni: usize,
    cmd: &str,
) -> Option<(u64, Message)> {
    let uc = uc!(u, ni);
    let first = uc.pending_cmds.get(cmd)?.first()?;
    Some((first.downstream_id, first.msg.clone()))
}

pub(super) fn dequeue_command(u: &mut User, ni: usize, cmd: &str) -> Option<(u64, Message)> {
    let current = current_pending_command(u, ni, cmd);
    let uc = uc!(u, ni);
    if let Some(l) = uc.pending_cmds.get_mut(cmd)
        && !l.is_empty()
    {
        l.remove(0);
    }
    send_next_pending_command(uc, cmd);
    current
}
