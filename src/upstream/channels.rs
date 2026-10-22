//! Channels' state: membership, topics, modes and names.

use super::*;

/// Handles channel messages: membership, topics, modes, names, LIST and
/// invites. Returns false if the message isn't one of them.
pub(super) fn handle_message(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
) -> Result<bool> {
    let net_id = u.networks[ni].id();
    let cm = u.networks[ni].casemap;
    let prefix_name = msg.prefix_name().to_owned();

    match msg.command.as_str() {
        "JOIN" => {
            need_params(msg, 1)?;
            let channels = msg.params[0].clone();
            let prefix = msg.prefix.clone().unwrap_or_default();
            let uc = uc!(u, ni);
            let mut uu = UpstreamUser {
                username: prefix.user.clone(),
                hostname: prefix.host.clone(),
                ..Default::default()
            };
            if uc.caps.is_enabled("away-notify") {
                // we have enough info to build the user flags in a
                // best-effort manner:
                // - the H/G flag is set to Here first, will be replaced by
                //   Gone later if the user is AWAY
                uu.flags = "H".to_owned();
                // - the B (bot mode) flag is set if the JOIN comes from a
                //   bot. Note: we have no way to track the user bot mode
                //   after they have joined (we are not notified of the bot
                //   mode updates), but this is good enough.
                if msg.tags.contains("bot")
                    && let Some(Some(bot)) = uc.isupport.get("BOT")
                {
                    uu.flags.push_str(bot);
                }
                // TODO: add the server operator flag (`*`) if the message
                // has an oper-tag
            }
            if msg.params.len() > 2 {
                // extended-join
                uu.account = msg.params[1].clone();
                uu.realname = msg.params[2].clone();
            }
            uc.cache_user_info(&prefix_name, uu);

            for ch in channels.split(',') {
                let uc = uc!(u, ni);
                if is_our_nick(cm, uc, &prefix_name) {
                    logf!(uc.logger, "joined channel {ch:?}");
                    if let Some(mut old) = uc.channels.del(ch) {
                        old.update_auto_detach(None);
                    }
                    uc.channels.set(
                        ch,
                        UpstreamChannel {
                            name: ch.to_owned(),
                            topic: String::new(),
                            topic_who: None,
                            topic_time: None,
                            status: ChannelStatus::Public,
                            modes: None,
                            creation_time: String::new(),
                            members: CaseMappingMap::new(cm),
                            complete: false,
                            detach_timer: None,
                        },
                    );
                    update_channel_auto_detach(u, ni, ch);
                    uc!(u, ni).send_message(msg!("MODE", ch));
                } else {
                    let Some(uch) = uc.channels.get_mut(ch) else {
                        bail!("unknown channel {ch:?}");
                    };
                    uch.members.set(&prefix_name, MembershipSet::default());
                }

                let mut ch_msg = msg.clone();
                ch_msg.params[0] = ch.to_owned();
                produce(u, ni, ch, &ch_msg, 0, false);
            }
        }
        "PART" => {
            need_params(msg, 1)?;
            let channels = msg.params[0].clone();
            for ch in channels.split(',') {
                let uc = uc!(u, ni);
                if is_our_nick(cm, uc, &prefix_name) {
                    logf!(uc.logger, "parted channel {ch:?}");
                    if let Some(mut uch) = uc.channels.del(ch) {
                        uch.update_auto_detach(None);
                        for nick in uch.members.keys() {
                            if !should_cache_user_info(cm, uc, nick) {
                                uc.users.del(nick);
                            }
                        }
                    }
                } else {
                    let Some(uch) = uc.channels.get_mut(ch) else {
                        bail!("unknown channel {ch:?}");
                    };
                    uch.members.del(&prefix_name);
                    if !should_cache_user_info(cm, uc, &prefix_name) {
                        uc.users.del(&prefix_name);
                    }
                }
                let mut ch_msg = msg.clone();
                ch_msg.params[0] = ch.to_owned();
                produce(u, ni, ch, &ch_msg, 0, false);
            }
        }
        "KICK" => {
            need_params(msg, 2)?;
            let channel = msg.params[0].clone();
            let user = msg.params[1].clone();
            let uc = uc!(u, ni);
            if is_our_nick(cm, uc, &user) {
                logf!(
                    uc.logger,
                    "kicked from channel {channel:?} by {prefix_name}"
                );
                if let Some(mut uch) = uc.channels.del(&channel) {
                    uch.update_auto_detach(None);
                    for nick in uch.members.keys() {
                        if !should_cache_user_info(cm, uc, nick) {
                            uc.users.del(nick);
                        }
                    }
                }
            } else {
                let Some(uch) = uc.channels.get_mut(&channel) else {
                    bail!("unknown channel {channel:?}");
                };
                uch.members.del(&user);
                if !should_cache_user_info(cm, uc, &user) {
                    uc.users.del(&user);
                }
            }
            produce(u, ni, &channel, msg, 0, false);
        }
        RPL_TOPIC | RPL_NOTOPIC => {
            need_params(msg, 3)?;
            let name = msg.params[1].clone();
            let topic = msg.params[2].clone();
            let uc = uc!(u, ni);
            match uc.channels.get_mut(&name) {
                None => forward_msg_by_id(u, net_id, downstream_id, msg),
                Some(ch) => {
                    ch.topic = if msg.command == RPL_TOPIC {
                        topic
                    } else {
                        String::new()
                    };
                }
            }
        }
        "TOPIC" => {
            need_params(msg, 1)?;
            let name = msg.params[0].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&name) else {
                bail!("unknown channel {name:?}");
            };
            if msg.params.len() > 1 {
                ch.topic = msg.params[1].clone();
                ch.topic_who = msg.prefix.clone();
                ch.topic_time = Some(
                    msg.tags
                        .get("time")
                        .and_then(xirc::parse_server_time)
                        .unwrap_or_else(Utc::now),
                );
            } else {
                ch.topic.clear();
            }
            let ch_name = ch.name.clone();
            produce(u, ni, &ch_name, msg, 0, false);
        }
        "MODE" => {
            need_params(msg, 2)?;
            let name = msg.params[0].clone();
            let mode_str = msg.params[1].clone();
            let uc = uc!(u, ni);
            if !uc.is_channel(&name) {
                // user mode change
                if name != uc.nick {
                    bail!("received MODE message for unknown nick {name:?}");
                }
                uc.modes.apply(&mode_str).map_err(|e| anyhow!(e))?;
                forward_message(u, net_id, msg);
            } else {
                // channel mode change
                let avail_memberships = uc.available_memberships.clone();
                let avail_modes = uc.available_channel_modes.clone();
                let Some(ch) = uc.channels.get_mut(&name) else {
                    bail!("unknown channel {name:?}");
                };
                ircutil::apply_channel_modes(
                    ch.modes.as_mut(),
                    &mut ch.members,
                    &avail_memberships,
                    &avail_modes,
                    &mode_str,
                    &msg.params[2..],
                )
                .map_err(|e| anyhow!(e))?;
                let ch_name = ch.name.clone();
                append_log(u, ni, &ch_name, msg);
                let detached = u.networks[ni]
                    .channels
                    .get(&name)
                    .is_some_and(|c| c.detached);
                if !detached {
                    forward_message(u, net_id, msg);
                }
            }
        }
        RPL_UMODEIS => {
            need_params(msg, 1)?;
            let mode_str = msg.params.get(1).cloned().unwrap_or_default();
            let uc = uc!(u, ni);
            uc.modes = UserModes::default();
            uc.modes.apply(&mode_str).map_err(|e| anyhow!(e))?;
            forward_message(u, net_id, msg);
        }
        RPL_CHANNELMODEIS => {
            need_params(msg, 2)?;
            let channel = msg.params[1].clone();
            let mode_str = msg.params.get(2).cloned().unwrap_or_default();
            let mode_args: Vec<String> =
                msg.params.get(3..).map(|s| s.to_vec()).unwrap_or_default();
            let uc = uc!(u, ni);
            let avail_memberships = uc.available_memberships.clone();
            let avail_modes = uc.available_channel_modes.clone();
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            };
            let first_mode = ch.modes.is_none();
            ch.modes = Some(ChannelModes::new());
            ircutil::apply_channel_modes(
                ch.modes.as_mut(),
                &mut ch.members,
                &avail_memberships,
                &avail_modes,
                &mode_str,
                &mode_args,
            )
            .map_err(|e| anyhow!(e))?;
            let detached = u.networks[ni]
                .channels
                .get(&channel)
                .is_some_and(|c| c.detached);
            if first_mode && !detached {
                forward_message(u, net_id, msg);
            }
        }
        RPL_CREATIONTIME => {
            need_params(msg, 3)?;
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            };
            let first = ch.creation_time.is_empty();
            ch.creation_time = msg.params[2].clone();
            let detached = u.networks[ni]
                .channels
                .get(&channel)
                .is_some_and(|c| c.detached);
            if first && !detached {
                forward_message(u, net_id, msg);
            }
        }
        RPL_TOPICWHOTIME => {
            need_params(msg, 4)?;
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            };
            let first = ch.topic_who.is_none();
            ch.topic_who = Some(Prefix::parse(&msg.params[2]));
            let sec: i64 = msg.params[3]
                .parse()
                .map_err(|e| anyhow!("failed to parse topic time: {e}"))?;
            ch.topic_time = chrono::DateTime::from_timestamp(sec, 0);
            let detached = u.networks[ni]
                .channels
                .get(&channel)
                .is_some_and(|c| c.detached);
            if first && !detached {
                forward_message(u, net_id, msg);
            }
        }
        RPL_LISTSTART | RPL_LIST => {
            let Some((dc_id, _)) = current_pending_command(u, ni, "LIST") else {
                bail!("unexpected RPL_LIST: no matching pending LIST");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_LISTEND => {
            let Some((dc_id, _)) = dequeue_command(u, ni, "LIST") else {
                bail!("unexpected RPL_LISTEND: no matching pending LIST");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_NAMREPLY => {
            need_params(msg, 4)?;
            let status_str = msg.params[1].clone();
            let name = msg.params[2].clone();
            let members = msg.params[3].clone();
            let uc = uc!(u, ni);
            if !uc.channels.has(&name) {
                // NAMES on a channel we have not joined, forward to downstream
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            }
            let status = ChannelStatus::parse(&status_str).map_err(|e| anyhow!(e))?;
            let parsed: Vec<(MembershipSet, String)> = members
                .split(' ')
                .filter(|s| !s.is_empty())
                .map(|s| uc.parse_membership_prefix(s))
                .collect();
            let ch = uc.channels.get_mut(&name).unwrap();
            ch.status = status;
            for (ms, nick) in parsed {
                ch.members.set(&nick, ms);
            }
        }
        RPL_ENDOFNAMES => {
            need_params(msg, 2)?;
            let name = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&name) else {
                // NAMES on a channel we have not joined, forward to downstream
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            };
            if ch.complete {
                bail!("received unexpected RPL_ENDOFNAMES");
            }
            ch.complete = true;
            let detached = u.networks[ni]
                .channels
                .get(&name)
                .is_some_and(|c| c.detached);
            if !detached {
                let net = &u.networks[ni];
                let uch = net.conn.as_ref().unwrap().channels.get(&name).unwrap();
                for dc in u
                    .downstreams
                    .iter_mut()
                    .filter(|d| d.network_id == Some(net_id))
                {
                    downstream::forward_channel(&u.srv.db, dc, uch, net_id, cm);
                }
            }
        }
        "INVITE" => {
            need_params(msg, 2)?;
            let nick = msg.params[0].clone();
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let we_are_invited = is_our_nick(cm, uc, &nick);
            let mut blocked = false;
            if we_are_invited {
                let joined = uc.channels.has(&channel);
                if !joined && let Some(c) = u.networks[ni].channels.get(&channel) {
                    // Automatically join a saved channel when we are invited
                    let msgs = xirc::generate_join(
                        std::slice::from_ref(&c.name),
                        std::slice::from_ref(&c.key),
                    );
                    let uc = uc!(u, ni);
                    for m in msgs {
                        uc.send_message(m);
                    }
                    return Ok(true);
                }
                let source_cm = cm(&prefix_name);
                match u.srv.db.get_message_target(net_id, &source_cm) {
                    Ok(mt) => blocked = mt.blocked,
                    Err(e) => logf!(
                        u.networks[ni].logger,
                        "failed to get the message target for {source_cm:?}: {e}"
                    ),
                }
            }
            for dc in dcs_for(&mut u.downstreams, net_id) {
                if !we_are_invited && !dc.caps.is_enabled("invite-notify") {
                    continue;
                }
                if blocked && !dc.metadata_subs.contains("soju.im/blocked") {
                    continue;
                }
                dc.send_message(msg.clone());
            }
            if we_are_invited {
                u.broadcast_web_push(ni, msg.clone());
            }
        }
        RPL_INVITING => {
            need_params(msg, 3)?;
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn update_channel_auto_detach(u: &mut User, ni: usize, name: &str) {
    let net = &mut u.networks[ni];
    let Some(ch) = net.channels.get(name) else {
        return;
    };
    if ch.detached {
        return;
    }
    let dur = ch.detach_after;
    let Some(uc) = net.upstream_mut() else {
        // Go's updateChannelAutoDetach also runs on registering
        // connections, which have no channels yet
        return;
    };
    let timer = if dur.is_zero() {
        None
    } else {
        let uch_name = match uc.channels.get(name) {
            Some(uch) => uch.name.clone(),
            None => return,
        };
        Some(uc.start_detach_timer(&uch_name, dur))
    };
    if let Some(uch) = uc.channels.get_mut(name) {
        uch.update_auto_detach(timer);
    } else if let Some(t) = timer {
        t.abort();
    }
}
