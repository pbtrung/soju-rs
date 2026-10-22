//! NICK, SETNAME, JOIN, PART and MODE from registered downstream
//! connections.

use super::*;

pub(super) fn handle_nick(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
    let nick = msg.params[0].clone();
    let dc_nick = dc!(u, i).nick.clone();
    if nick.is_empty() || nick.contains(ILLEGAL_NICK_CHARS) {
        return Err(irc_error(msg!(
            ERR_ERRONEUSNICKNAME,
            dc_nick,
            nick,
            "Nickname contains illegal characters"
        )));
    }
    if dc!(u, i).casemap(&nick) == SERVICE_NICK_CM {
        return Err(irc_error(msg!(
            ERR_NICKNAMEINUSE,
            dc_nick,
            nick,
            "Nickname reserved for bouncer service"
        )));
    }

    let ni = dc_network(u, i);
    let res = match ni {
        Some(ni) => {
            let mut record = u.networks[ni].record.clone();
            record.nick = nick.clone();
            u.srv.db.store_network(u.record.id, &mut record)
        }
        None => u.update_user(|r| {
            r.nick = nick.clone();
            Ok(())
        }),
    };
    if let Err(e) = res {
        logf!(dc!(u, i).logger, "failed to update nick: {e}");
        return Err(irc_error(msg!(
            ERR_UNKNOWNERROR,
            dc_nick,
            "NICK",
            "Failed to update nick"
        )));
    }

    match ni {
        Some(ni) => {
            u.networks[ni].record.nick = nick.clone();
            u.networks[ni].sync_snapshot();
            if u.networks[ni].upstream().is_some() {
                send_upstream_labeled(u, i, ni, msg!("NICK", nick));
            } else {
                let id = dc!(u, i).id;
                update_nick(u, id);
            }
        }
        None => {
            let ids: Vec<u64> = u
                .downstreams
                .iter()
                .filter(|d| d.network_id.is_none())
                .map(|d| d.id)
                .collect();
            for id in ids {
                update_nick(u, id);
            }
        }
    }
    Ok(())
}

pub(super) fn handle_setname(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
    let realname = msg.params[0].clone();
    if dc!(u, i).realname == realname {
        let prefix = dc!(u, i).prefix();
        dc!(u, i).send_message(msg!("SETNAME", realname).with_prefix(prefix));
        return Ok(());
    }

    let dc_id = dc!(u, i).id;
    let res = match dc_network(u, i) {
        Some(ni) => {
            // If the client just resets to the default, just wipe the
            // per-network preference
            let mut record = u.networks[ni].record.clone();
            record.realname = realname.clone();
            if realname == u.record.realname {
                record.realname.clear();
            }
            let setname = u.networks[ni]
                .upstream()
                .is_some_and(|uc| uc.caps.is_enabled("setname"));
            if setname {
                // Upstream will reply with a SETNAME message on success
                send_upstream_labeled(u, i, ni, msg!("SETNAME", realname.clone()));
                u.srv.db.store_network(u.record.id, &mut record).map(|_| ())
            } else {
                // This will disconnect then re-connect the upstream
                // connection
                u.update_network(record, false).map(|_| ())
            }
        }
        None => u.update_user(|r| {
            r.realname = realname.clone();
            Ok(())
        }),
    };
    if let Err(e) = res {
        if let Some(i) = u.dc_index(dc_id) {
            logf!(dc!(u, i).logger, "failed to update realname: {e}");
        }
        return Err(fail(&[
            "SETNAME",
            "CANNOT_CHANGE_REALNAME",
            "Failed to update realname",
        ]));
    }

    let Some(i) = u.dc_index(dc_id) else {
        return Ok(());
    };
    if dc!(u, i).network_id.is_none() {
        let ids: Vec<u64> = u
            .downstreams
            .iter()
            .filter(|d| d.network_id.is_none())
            .map(|d| d.id)
            .collect();
        for id in ids {
            update_realname(u, id);
        }
    }
    Ok(())
}

pub(super) fn handle_join(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    let ni = upstream_for_command(u, i, &msg.command)?;
    need_params(msg, 1)?;
    let keys: Vec<String> = msg
        .params
        .get(1)
        .map(|k| k.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let names: Vec<String> = msg.params[0].split(',').map(str::to_owned).collect();
    if names.len() > 1 {
        // We need to potentially mix local and remote messages so we drop
        // the label
        dc!(u, i).drop_label();
    }
    let net_id = u.networks[ni].id();
    for (idx, name) in names.into_iter().enumerate() {
        let key = keys.get(idx).cloned().unwrap_or_default();
        let nick = dc!(u, i).nick.clone();
        if name.is_empty() || name.contains(ILLEGAL_CHAN_CHARS) {
            dc!(u, i).send_message(msg!(ERR_BADCHANMASK, nick, name, "Invalid channel name"));
            continue;
        }
        let uc = u.networks[ni].upstream().unwrap();
        if !uc.is_channel(&name) {
            dc!(u, i).send_message(msg!(ERR_NOSUCHCHANNEL, nick, name, "Not a channel name"));
            continue;
        }

        // Most servers ignore duplicate JOIN messages. We ignore them here
        // because some clients automatically send JOIN messages in bulk when
        // reconnecting to the bouncer. We don't want to flood the upstream
        // connection with these.
        if !uc.channels.has(&name) {
            let mut params = vec![name.clone()];
            if !key.is_empty() {
                params.push(key.clone());
            }
            send_upstream_labeled(u, i, ni, Message::new("JOIN", params));
        }

        let net = &mut u.networks[ni];
        if let Some(ch) = net.channels.get_mut(&name) {
            // Don't clear the channel key if there's one set
            // TODO: add a way to unset the channel key
            if !key.is_empty() {
                ch.key = key;
            }
            u.attach_channel(ni, &name);
        } else {
            net.channels.set(
                &name,
                db::Channel {
                    name: name.clone(),
                    key,
                    ..Default::default()
                },
            );
        }
        if let Some(ch) = u.networks[ni].channels.get_mut(&name)
            && let Err(e) = u.srv.db.store_channel(net_id, ch)
        {
            logf!(
                dc!(u, i).logger,
                "failed to create or update channel {name:?}: {e}"
            );
        }
    }
    Ok(())
}

pub(super) fn handle_part(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    let ni = upstream_for_command(u, i, &msg.command)?;
    need_params(msg, 1)?;
    let reason = msg.params.get(1).cloned().unwrap_or_default();
    let names: Vec<String> = msg.params[0].split(',').map(str::to_owned).collect();
    if names.len() > 1 {
        dc!(u, i).drop_label();
    }
    let net_id = u.networks[ni].id();
    for name in names {
        if reason.eq_ignore_ascii_case("detach") {
            if u.networks[ni].channels.has(&name) {
                u.detach_channel(ni, &name);
            } else {
                u.networks[ni].channels.set(
                    &name,
                    db::Channel {
                        name: name.clone(),
                        detached: true,
                        ..Default::default()
                    },
                );
            }
            if let Some(ch) = u.networks[ni].channels.get_mut(&name)
                && let Err(e) = u.srv.db.store_channel(net_id, ch)
            {
                logf!(
                    dc!(u, i).logger,
                    "failed to create or update channel {name:?}: {e}"
                );
            }
        } else {
            let mut params = vec![name.clone()];
            if !reason.is_empty() {
                params.push(reason.clone());
            }
            send_upstream_labeled(u, i, ni, Message::new("PART", params));
            if let Err(e) = u.delete_channel(ni, &name) {
                logf!(dc!(u, i).logger, "failed to delete channel {name:?}: {e}");
            }
            u.networks[ni].push_targets.lock().unwrap().del(&name);
        }
    }
    Ok(())
}

pub(super) fn handle_mode(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
    let name = msg.params[0].clone();
    let mode_str = msg.params.get(1).cloned().unwrap_or_default();
    let nick = dc!(u, i).nick.clone();

    if dc!(u, i).casemap(&name) == dc!(u, i).nick_cm {
        if !mode_str.is_empty() {
            let ni = upstream_for_command(u, i, &msg.command)?;
            send_upstream_labeled(u, i, ni, msg.clone());
        } else {
            let user_mode = dc_network(u, i)
                .and_then(|ni| u.networks[ni].upstream())
                .map(|uc| uc.modes.0.clone())
                .unwrap_or_default();
            dc!(u, i).send_message(msg!(RPL_UMODEIS, nick, format!("+{user_mode}")));
        }
        return Ok(());
    }

    let ni = upstream_for_command(u, i, &msg.command)?;
    let uc = u.networks[ni].upstream().unwrap();
    if !uc.is_channel(&name) {
        return Err(irc_error(msg!(
            ERR_USERSDONTMATCH,
            nick,
            "Cannot change mode for other users"
        )));
    }

    if !mode_str.is_empty() {
        let mut params = vec![name, mode_str];
        params.extend_from_slice(&msg.params[2..]);
        send_upstream_labeled(u, i, ni, Message::new("MODE", params));
    } else {
        let Some(ch) = uc.channels.get(&name) else {
            // we're not on that channel, pass command to upstream
            send_upstream_labeled(u, i, ni, msg.clone());
            return Ok(());
        };
        let Some(modes) = &ch.modes else {
            // we haven't received the initial RPL_CHANNELMODEIS yet, ignore
            // the request, we will broadcast the modes later when we receive
            // RPL_CHANNELMODEIS
            return Ok(());
        };
        let (mode_str, mode_params) = ircutil::format_channel_modes(modes);
        let mut params = vec![nick.clone(), name.clone(), mode_str];
        params.extend(mode_params);
        let creation_time = ch.creation_time.clone();
        dc!(u, i).send_message(Message::new(RPL_CHANNELMODEIS, params));
        if !creation_time.is_empty() {
            dc!(u, i).send_message(msg!(RPL_CREATIONTIME, nick, name, creation_time));
        }
    }
    Ok(())
}
