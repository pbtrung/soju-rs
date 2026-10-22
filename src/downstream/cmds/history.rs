//! CHATHISTORY, READ/MARKREAD and SEARCH from registered downstream
//! connections.

use super::*;

/// History replies are refused while a client has this many messages it
/// hasn't read yet, so that a client which stops reading can't make us queue
/// replies without bounds.
const MAX_QUEUED_FOR_HISTORY: usize = 10 * CHAT_HISTORY_LIMIT;

fn check_queue(u: &User, i: usize, cmd: &str) -> Result<()> {
    if u.downstreams[i].conn.queued() > MAX_QUEUED_FOR_HISTORY {
        return Err(fail(&[
            cmd,
            "TEMPORARILY_UNAVAILABLE",
            "Too many messages waiting to be sent, try again later",
        ]));
    }
    Ok(())
}

pub(super) fn handle_chathistory(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
    check_queue(u, i, "CHATHISTORY")?;
    let subcommand = msg.params[0].clone();
    let (target, bounds_str, limit_str): (String, [String; 2], String) = match subcommand.as_str() {
        "AFTER" | "BEFORE" | "AROUND" | "LATEST" => {
            need_params(msg, 4)?;
            (
                msg.params[1].clone(),
                [msg.params[2].clone(), String::new()],
                msg.params[3].clone(),
            )
        }
        "BETWEEN" => {
            need_params(msg, 5)?;
            (
                msg.params[1].clone(),
                [msg.params[2].clone(), msg.params[3].clone()],
                msg.params[4].clone(),
            )
        }
        "TARGETS" => {
            if dc_network(u, i).is_none() {
                dc!(u, i).send_batch("draft/chathistory-targets", &[], Vec::new());
                return Ok(());
            }
            need_params(msg, 4)?;
            (
                String::new(),
                [msg.params[1].clone(), msg.params[2].clone()],
                msg.params[3].clone(),
            )
        }
        _ => {
            return Err(fail(&[
                "CHATHISTORY",
                "INVALID_PARAMS",
                &subcommand,
                "Unknown command",
            ]));
        }
    };

    // We don't save history for our service
    if dc!(u, i).casemap(&target) == SERVICE_NICK_CM {
        dc!(u, i).send_batch("chathistory", &[target], Vec::new());
        return Ok(());
    }

    if !u.has_chathistory_store() {
        let nick = dc!(u, i).nick.clone();
        return Err(irc_error(msg!(
            ERR_UNKNOWNCOMMAND,
            nick,
            "CHATHISTORY",
            "Chat history disabled"
        )));
    }

    let Some(ni) = dc_network(u, i) else {
        return Err(fail(&[
            "CHATHISTORY",
            "INVALID_TARGET",
            &subcommand,
            &target,
            "Cannot fetch chat history on bouncer connection",
        ]));
    };

    let target = u.networks[ni].casemap(&target);

    // TODO: support msgid criteria
    let mut bounds: [Option<chrono::DateTime<Utc>>; 2] = [None, None];
    bounds[0] = ircutil::parse_chathistory_bound(&bounds_str[0]);
    if subcommand == "LATEST" && bounds_str[0] == "*" {
        bounds[0] = None;
    } else if bounds[0].is_none() {
        return Err(fail(&[
            "CHATHISTORY",
            "INVALID_PARAMS",
            &subcommand,
            &bounds_str[0],
            "Invalid first bound",
        ]));
    }
    if !bounds_str[1].is_empty() {
        bounds[1] = ircutil::parse_chathistory_bound(&bounds_str[1]);
        if bounds[1].is_none() {
            return Err(fail(&[
                "CHATHISTORY",
                "INVALID_PARAMS",
                &subcommand,
                &bounds_str[1],
                "Invalid second bound",
            ]));
        }
    }

    let limit = match limit_str.parse::<i64>() {
        Ok(l) if l >= 0 && l <= CHAT_HISTORY_LIMIT as i64 => l as usize,
        _ => {
            return Err(fail(&[
                "CHATHISTORY",
                "INVALID_PARAMS",
                &subcommand,
                &limit_str,
                "Invalid limit",
            ]));
        }
    };

    let event_playback = dc!(u, i).caps.is_enabled("draft/event-playback");
    let net = &u.networks[ni];
    let options = LoadMessageOptions {
        network: &net.record,
        entity: &target,
        limit,
        events: event_playback,
    };
    let store = &mut u.msg_store;

    // No upper bound rather than now: if the upstream server's clock is
    // ahead of ours, its latest messages are in our future
    let history = match subcommand.as_str() {
        "BEFORE" => store.load_before_time(bounds[0], None, &options),
        "LATEST" => store.load_before_time(None, bounds[0], &options),
        "AFTER" => store.load_after_time(bounds[0], None, &options),
        "AROUND" => (|| {
            let after_limit = limit / 2;
            let before_limit = limit - after_limit;
            let before = store.load_before_time(
                bounds[0],
                None,
                &LoadMessageOptions {
                    limit: before_limit,
                    ..options
                },
            )?;
            let after_bound = match before.last() {
                Some(last) => {
                    xirc::parse_server_time(last.tags.get_or_empty("time")).ok_or_else(|| {
                        anyhow!(
                            "parsing time {:?}: cannot parse",
                            last.tags.get_or_empty("time")
                        )
                    })?
                }
                // There are no messages before, subtract an arbitrary amount
                // of time to make the bound inclusive
                None => bounds[0].unwrap() - chrono::Duration::seconds(1),
            };
            let after = store.load_after_time(
                Some(after_bound),
                None,
                &LoadMessageOptions {
                    limit: after_limit,
                    ..options
                },
            )?;
            let mut h = before;
            h.extend(after);
            Ok(h)
        })(),
        "BETWEEN" => {
            if bounds[0] < bounds[1] {
                store.load_after_time(bounds[0], bounds[1], &options)
            } else {
                store.load_before_time(bounds[0], bounds[1], &options)
            }
        }
        "TARGETS" => {
            let [Some(start), Some(end)] = bounds else {
                return Err(fail(&[
                    "CHATHISTORY",
                    "INVALID_PARAMS",
                    &subcommand,
                    "Invalid bounds",
                ]));
            };
            let targets = match store.list_targets(&net.record, start, end, limit, event_playback) {
                Ok(t) => t,
                Err(e) => {
                    logf!(
                        dc!(u, i).logger,
                        "failed fetching targets for chathistory: {e}"
                    );
                    return Err(fail(&[
                        "CHATHISTORY",
                        "MESSAGE_ERROR",
                        &subcommand,
                        "Failed to retrieve targets",
                    ]));
                }
            };
            let msgs = targets
                .into_iter()
                .filter(|t| !net.channels.get(&t.name).is_some_and(|c| c.detached))
                .map(|t| {
                    msg!(
                        "CHATHISTORY",
                        "TARGETS",
                        t.name,
                        xirc::format_server_time(t.latest_message)
                    )
                });
            u.downstreams[i].send_batch("draft/chathistory-targets", &[], msgs);
            return Ok(());
        }
        _ => unreachable!(),
    };

    let history = match history {
        Ok(h) => h,
        Err(e) => {
            logf!(
                dc!(u, i).logger,
                "failed fetching {target:?} messages for chathistory: {e}"
            );
            return Err(chathistory_error(&subcommand, &target));
        }
    };
    dc!(u, i).send_batch("chathistory", &[target], history);
    Ok(())
}

/// Rejects a target which can't be a middle parameter of our replies: it
/// would corrupt them, e.g. "a b" would be read as two parameters.
pub(super) fn check_target(cmd: &str, target: &str) -> Result<()> {
    if target.is_empty() || target.contains(' ') || target.starts_with(':') {
        return Err(fail(&[cmd, "INVALID_TARGET", "*", "Invalid target"]));
    }
    Ok(())
}

pub(super) fn handle_read(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if msg.params.is_empty() {
        return Err(fail(&[
            &msg.command,
            "NEED_MORE_PARAMS",
            "Missing parameters",
        ]));
    }
    let target = msg.params[0].clone();
    check_target(&msg.command, &target)?;
    let criteria = msg.params.get(1).cloned().unwrap_or_default();

    // We don't save read receipts for our service
    if dc!(u, i).casemap(&target) == SERVICE_NICK_CM {
        let prefix = dc!(u, i).prefix();
        dc!(u, i).send_message(msg!(msg.command.clone(), target, "*").with_prefix(prefix));
        return Ok(());
    }

    let Some(ni) = dc_network(u, i) else {
        return Err(fail(&[
            &msg.command,
            "INTERNAL_ERROR",
            &target,
            "Cannot set read markers on bouncer connection",
        ]));
    };
    let net_id = u.networks[ni].id();
    let target_cm = u.networks[ni].casemap(&target);

    let mut r = match u.srv.db.get_read_receipt(net_id, &target_cm) {
        Ok(Some(r)) => r,
        Ok(None) => db::ReadReceipt {
            target: target_cm.clone(),
            ..Default::default()
        },
        Err(e) => {
            logf!(
                dc!(u, i).logger,
                "failed to get the read receipt for {target:?}: {e}"
            );
            return Err(internal_error(&msg.command, &target));
        }
    };

    let mut broadcast = false;
    if !criteria.is_empty() {
        // TODO: support msgid criteria
        let Some(("timestamp", ts)) = criteria.split_once('=') else {
            return Err(fail(&[
                &msg.command,
                "INVALID_PARAMS",
                &criteria,
                "Unknown criteria",
            ]));
        };
        let Some(mut timestamp) = xirc::parse_server_time(ts) else {
            return Err(fail(&[
                &msg.command,
                "INVALID_PARAMS",
                &criteria,
                "Invalid criteria",
            ]));
        };
        let now = Utc::now();
        if r.timestamp.is_none_or(|t| t < timestamp) && timestamp > now {
            // Only allow read markers after now if the upstream server's
            // clock is ahead of ours, i.e. if we have a message more recent
            // than now in history
            let mut limit = now;
            if u.msg_store.supports_chathistory() {
                let store = &mut u.msg_store;
                let options = LoadMessageOptions {
                    network: &u.networks[ni].record,
                    entity: &target_cm,
                    limit: 1,
                    events: true,
                };
                let latest = store.load_before_time(None, None, &options).and_then(|h| {
                    h.first()
                        .map(|m| {
                            let t = m.tags.get_or_empty("time");
                            xirc::parse_server_time(t)
                                .ok_or_else(|| anyhow!("parsing time {t:?}: cannot parse"))
                        })
                        .transpose()
                });
                match latest {
                    Ok(Some(latest)) if latest > limit => limit = latest,
                    Ok(_) => {}
                    Err(e) => {
                        logf!(
                            dc!(u, i).logger,
                            "failed to get the latest message time for {target:?}: {e}"
                        );
                        return Err(internal_error(&msg.command, &target));
                    }
                }
            }
            timestamp = timestamp.min(limit);
        }
        if r.timestamp.is_none_or(|t| t < timestamp) {
            r.timestamp = Some(timestamp);
            if let Err(e) = u.srv.db.store_read_receipt(net_id, &mut r) {
                logf!(
                    dc!(u, i).logger,
                    "failed to store receipt for {target:?}: {e}"
                );
                return Err(internal_error(&msg.command, &target));
            }
            broadcast = true;
        }
    }

    let timestamp_str = match r.timestamp {
        Some(t) => format!("timestamp={}", xirc::format_server_time(t)),
        None => "*".to_owned(),
    };
    let dc_id = dc!(u, i).id;
    for d in dcs_for(&mut u.downstreams, net_id) {
        if broadcast || d.id == dc_id {
            let cmd = if d.caps.is_enabled("draft/read-marker") {
                "MARKREAD"
            } else {
                "READ"
            };
            let prefix = d.prefix();
            d.send_message(msg!(cmd, target.clone(), timestamp_str.clone()).with_prefix(prefix));
        }
    }

    let mut push = false;
    {
        let mut pt = u.networks[ni].push_targets.lock().unwrap();
        if broadcast && pt.has(&target) {
            // TODO: only broadcast if draft/read-marker has been negotiated
            if let (Some(rt), Some(&t)) = (r.timestamp, pt.get(&target))
                && rt >= t
            {
                pt.del(&target);
            }
            push = true;
        }
    }
    if push {
        u.broadcast_web_push(ni, msg!("MARKREAD", target, timestamp_str));
    }
    Ok(())
}

pub(super) fn handle_search(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    check_queue(u, i, "SEARCH")?;
    if !u.msg_store.supports_search() {
        let nick = dc!(u, i).nick.clone();
        return Err(irc_error(msg!(
            ERR_UNKNOWNCOMMAND,
            nick,
            "SEARCH",
            "Unknown command"
        )));
    }
    need_params(msg, 1)?;
    let attrs = Tags::parse(&msg.params[0]);

    const SEARCH_MAX_LIMIT: usize = 100;
    let mut network = None;
    let mut opts = SearchMessageOptions {
        limit: SEARCH_MAX_LIMIT,
        ..Default::default()
    };
    for (name, value) in attrs.iter() {
        match name {
            "before" | "after" => {
                let Some(t) = xirc::parse_server_time(value) else {
                    return Err(fail(&[
                        "SEARCH",
                        "INVALID_PARAMS",
                        name,
                        "Invalid criteria",
                    ]));
                };
                if name == "after" {
                    opts.start = Some(t);
                } else {
                    opts.end = Some(t);
                }
            }
            "from" => opts.from = value.to_owned(),
            "in" => {
                let Some(ni) = dc_network(u, i) else {
                    return Err(fail(&[
                        "SEARCH",
                        "INVALID_PARAMS",
                        name,
                        "Cannot search on bouncer connection",
                    ]));
                };
                network = Some(ni);
                opts.r#in = u.networks[ni].casemap(value);
            }
            "text" => opts.text = value.to_owned(),
            "limit" => match value.parse::<i64>() {
                Ok(l) if l > 0 => opts.limit = l as usize,
                _ => return Err(fail(&["SEARCH", "INVALID_PARAMS", name, "Invalid limit"])),
            },
            _ => {}
        }
    }
    let Some(ni) = network else {
        return Err(fail(&[
            "SEARCH",
            "INVALID_PARAMS",
            "in",
            "The in parameter is mandatory",
        ]));
    };
    if opts.limit > SEARCH_MAX_LIMIT {
        opts.limit = SEARCH_MAX_LIMIT;
    }

    let messages = match u.msg_store.search(&u.networks[ni].record, &opts) {
        Ok(m) => m,
        Err(e) => {
            logf!(dc!(u, i).logger, "failed fetching messages for search: {e}");
            return Err(fail(&[
                "SEARCH",
                "INTERNAL_ERROR",
                "Messages could not be retrieved",
            ]));
        }
    };
    dc!(u, i).send_batch("soju.im/search", &[], messages);
    Ok(())
}
