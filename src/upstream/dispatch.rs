//! Message validation, label routing, multiline assembly and handler dispatch.

use super::*;

pub(super) fn parse_label(label: &str) -> Result<(u64, String)> {
    if label.is_empty() {
        return Ok((0, String::new()));
    }
    let parts: Vec<&str> = label.splitn(4, '-').collect();
    let res = if parts.len() < 4 {
        Err(anyhow!("not enough arguments"))
    } else if parts[0] != "sd" {
        Err(anyhow!("expected sd, got {}", parts[0]))
    } else {
        parts[1].parse::<u64>().map_err(|e| anyhow!("{e}"))
    };
    match res {
        Ok(id) => Ok((id, parts[3].to_owned())),
        Err(e) => {
            bail!("unexpected message label: invalid downstream reference for label {label:?}: {e}")
        }
    }
}

pub(super) fn need_params(msg: &Message, n: usize) -> Result<()> {
    if msg.params.len() < n {
        return Err(downstream::need_more_params(&msg.command).into());
    }
    Ok(())
}

/// Forwards a message to the downstreams of a network, or only one of them
/// if id isn't 0.
pub(super) fn forward_msg_by_id(u: &mut User, net_id: i64, id: u64, msg: &Message) {
    for dc in dcs_for(&mut u.downstreams, net_id) {
        if id != 0 && id != dc.id {
            continue;
        }
        dc.send_message(msg.clone());
    }
}

pub(super) fn forward_message(u: &mut User, net_id: i64, msg: &Message) {
    forward_msg_by_id(u, net_id, 0, msg);
}

/// The result of feeding a message to the draft/multiline assembler.
enum Multiline {
    /// The message isn't part of a multiline batch.
    Passthrough,
    /// The message has been buffered.
    Consumed,
    /// A multiline batch is complete: handle this logical message instead.
    Complete(Message),
}

/// The maximum number of batches a server may have open at once.
const MAX_OPEN_BATCHES: usize = 64;

fn assemble_multiline(uc: &mut UpstreamConn, msg: &Message) -> Result<Multiline> {
    if msg.command == "BATCH" {
        let tag = msg.param(0);
        if tag.starts_with('+') && uc.batches.len() >= MAX_OPEN_BATCHES {
            bail!("too many open batches");
        }
        if let Some(r) = tag.strip_prefix('+')
            && msg.param(1) == xirc::MULTILINE_BATCH_TYPE
        {
            if uc.batches.contains_key(r) {
                bail!("unexpected BATCH reference tag: batch was already defined: {r:?}");
            }
            let batch = UpstreamBatch {
                label: String::new(),
                multiline: Some(xirc::MultilineBatch::new(msg.clone())),
            };
            uc.batches.insert(r.to_owned(), batch);
            return Ok(Multiline::Consumed);
        }
        if let Some(r) = tag.strip_prefix('-')
            && uc.batches.get(r).is_some_and(|b| b.multiline.is_some())
        {
            let batch = uc.batches.remove(r).unwrap().multiline.unwrap();
            return match batch.finish() {
                Ok(m) => Ok(Multiline::Complete(m)),
                Err(e) => {
                    logf!(uc.logger, "ignoring invalid multiline batch: {}", e.code());
                    Ok(Multiline::Consumed)
                }
            };
        }
        return Ok(Multiline::Passthrough);
    }
    if let Some(r) = msg.tags.get("batch")
        && let Some(UpstreamBatch {
            multiline: Some(batch),
            ..
        }) = uc.batches.get_mut(r)
    {
        // The server's limits only apply to what we send, use our own
        batch.push(msg, &xirc::MultilineLimits::default());
        return Ok(Multiline::Consumed);
    }
    Ok(Multiline::Passthrough)
}

/// Handles a message from the upstream server. Like upstream, it may remove
/// tags from the message (e.g. label and batch).
pub fn handle_message(u: &mut User, ni: usize, msg: &mut Message) -> Result<()> {
    match assemble_multiline(uc!(u, ni), msg)? {
        Multiline::Passthrough => {}
        Multiline::Consumed => return Ok(()),
        Multiline::Complete(mut m) => return handle_message(u, ni, &mut m),
    }

    let net_id = u.networks[ni].id();
    let mut label = msg.tags.remove("label").unwrap_or_default();

    let mut batch_name = String::new();
    let mut batch_label = false;
    if let Some(bn) = msg.tags.remove("batch") {
        let uc = uc!(u, ni);
        let Some(b) = uc.batches.get(&bn) else {
            bail!("unexpected batch reference: batch was not defined: {bn:?}");
        };
        // Messages in a labeled batch belong to its labeled response
        if label.is_empty() {
            batch_label = true;
            label = b.label.clone();
        }
        batch_name = bn;
    }

    let (downstream_id, downstream_label) = parse_label(&label)?;
    let mut labeled_dcs = Vec::new();
    if downstream_id != 0 {
        for dc in dcs_for(&mut u.downstreams, net_id) {
            if dc.id != downstream_id {
                continue;
            }
            dc.label = Some(LabelContext {
                label: downstream_label.clone(),
                batch: if batch_label {
                    batch_name.clone()
                } else {
                    String::new()
                },
                pending_msg: None,
            });
            labeled_dcs.push(dc.id);
        }
    }

    let res = handle_message_inner(u, ni, msg, downstream_id, &downstream_label, &label);

    for id in labeled_dcs {
        if let Some(i) = u.dc_index(id) {
            if !batch_label {
                u.downstreams[i].flush_batch();
            }
            u.downstreams[i].label = None;
        }
    }
    res
}

fn handle_message_inner(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
    downstream_label: &str,
    label: &str,
) -> Result<()> {
    let net_id = u.networks[ni].id();

    if msg.prefix.is_none() {
        msg.prefix = Some(uc!(u, ni).server_prefix.clone());
    }

    if !ircutil::is_numeric(&msg.command) {
        let t = msg
            .tags
            .get("time")
            .and_then(xirc::parse_server_time)
            .unwrap_or_else(Utc::now);
        msg.tags.set("time", xirc::format_server_time(t));
    }

    let handled = messages::handle_message(u, ni, msg, downstream_id, downstream_label, label)?
        || registration::handle_message(u, ni, msg, downstream_id)?
        || users::handle_message(u, ni, msg, downstream_id)?
        || channels::handle_message(u, ni, msg, downstream_id)?;
    if !handled {
        debugf!(uc!(u, ni).logger, "unhandled message: {msg}");
        forward_msg_by_id(u, net_id, downstream_id, msg);
    }
    Ok(())
}
