//! Users' state: nicknames, WHO replies, MONITOR, away and account status.

use super::*;

/// Handles user messages: nickname and host changes, WHO, MONITOR, away and
/// account status. Returns false if the message isn't one of them.
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
        "NICK" => {
            need_params(msg, 1)?;
            let new_nick = msg.params[0].clone();
            let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
            let uc = uc!(u, ni);
            let mut me = false;
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(uc.logger, "changed nick from {:?} to {new_nick:?}", uc.nick);
                me = true;
                uc.nick = new_nick.clone();
                if cm(&uc.pending_regain_nick) == cm(&new_nick) {
                    uc.pending_regain_nick.clear();
                    uc.stop_regain_nick_timer();
                }
                if cm(&want_nick) == cm(&new_nick) {
                    uc.has_desired_nick = true;
                }
            }

            let mut log_channels = Vec::new();
            for ch in uc.channels.values_mut() {
                if let Some(ms) = ch.members.del(&prefix_name) {
                    ch.members.set(&new_nick, ms);
                    log_channels.push(ch.name.clone());
                }
            }
            uc.cache_user_info(
                &prefix_name,
                UpstreamUser {
                    nickname: new_nick.clone(),
                    ..Default::default()
                },
            );
            for ch in log_channels {
                append_log(u, ni, &ch, msg);
            }

            if !me {
                forward_message(u, net_id, msg);
            } else {
                for id in dc_ids_for(&u.downstreams, net_id) {
                    downstream::update_nick(u, id);
                }
                update_monitor(u, ni);
            }
        }
        "SETNAME" => {
            need_params(msg, 1)?;
            let new_realname = msg.params[0].clone();
            let uc = uc!(u, ni);
            uc.cache_user_info(
                &prefix_name,
                UpstreamUser {
                    realname: new_realname.clone(),
                    ..Default::default()
                },
            );
            // TODO: consider appending this message to logs
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(
                    uc.logger,
                    "changed realname from {:?} to {new_realname:?}",
                    uc.realname
                );
                uc.realname = new_realname;
                for id in dc_ids_for(&u.downstreams, net_id) {
                    downstream::update_realname(u, id);
                }
            } else {
                forward_message(u, net_id, msg);
            }
        }
        "CHGHOST" => {
            need_params(msg, 2)?;
            let uc = uc!(u, ni);
            // The server sends CHGHOST instead of QUIT/JOIN: keep cached WHO
            // replies up to date
            uc.cache_user_info(
                &prefix_name,
                UpstreamUser {
                    username: msg.params[0].clone(),
                    hostname: msg.params[1].clone(),
                    ..Default::default()
                },
            );
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(
                    uc.logger,
                    "changed prefix from {:?} to {:?}",
                    msg.prefix
                        .as_ref()
                        .map(|p| p.to_string())
                        .unwrap_or_default(),
                    Prefix::full(
                        uc.nick.clone(),
                        msg.params[0].clone(),
                        msg.params[1].clone()
                    )
                    .to_string()
                );
                uc.username = msg.params[0].clone();
                uc.hostname = msg.params[1].clone();
                for id in dc_ids_for(&u.downstreams, net_id) {
                    downstream::update_host(u, id);
                }
            } else {
                // TODO: add fallback with QUIT/JOIN/MODE messages
                forward_message(u, net_id, msg);
            }
        }
        "QUIT" => {
            let uc = uc!(u, ni);
            if is_our_nick(cm, uc, &prefix_name) {
                logf!(uc.logger, "quit");
            }
            let mut log_channels = Vec::new();
            for ch in uc.channels.values_mut() {
                if ch.members.del(&prefix_name).is_some() {
                    log_channels.push(ch.name.clone());
                }
            }
            uc.users.del(&prefix_name);
            let our_nick = uc.nick.clone();
            for ch in log_channels {
                append_log(u, ni, &ch, msg);
            }
            if prefix_name != our_nick {
                forward_message(u, net_id, msg);
            }
        }
        RPL_WHOREPLY => {
            need_params(msg, 8)?;
            let (username, host, server, nick, flags, trailing) = (
                msg.params[2].clone(),
                msg.params[3].clone(),
                msg.params[4].clone(),
                msg.params[5].clone(),
                msg.params[6].clone(),
                msg.params[7].clone(),
            );
            let Some(dc_id) = current_pending_downstream(u, ni, "WHO") else {
                bail!("unexpected RPL_WHOREPLY: no matching pending WHO");
            };
            clear_ended_early(uc!(u, ni), "WHO");
            let Some((_, realname)) = trailing.split_once(' ') else {
                bail!("malformed RPL_WHOREPLY: failed to parse real name");
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            } else {
                return Ok(true);
            }
            let uc = uc!(u, ni);
            if should_cache_user_info(cm, uc, &nick) {
                let flags = ircutil::strip_member_prefixes(&flags, &uc.available_memberships);
                uc.cache_user_info(
                    &nick,
                    UpstreamUser {
                        username,
                        hostname: host,
                        server,
                        nickname: nick.clone(),
                        flags,
                        realname: realname.to_owned(),
                        ..Default::default()
                    },
                );
            }
        }
        RPL_WHOSPCRPL => {
            let Some(cmd) = current_pending_command(u, ni, "WHO") else {
                bail!("unexpected RPL_WHOSPCRPL: no matching pending WHO");
            };
            let dc_id = cmd.downstream_id;
            let options = cmd.msg.params.get(1).cloned();
            clear_ended_early(uc!(u, ni), "WHO");
            match u.dc_index(dc_id) {
                Some(i) => u.downstreams[i].send_message(msg.clone()),
                None => return Ok(true),
            }
            if let Some(options) = options {
                let (fields, _) = xirc::parse_whox_options(&options);
                if !fields.contains('n') {
                    return Ok(true);
                }
                let info = xirc::parse_whox_reply(msg, &fields).map_err(|e| anyhow!(e))?;
                let uc = uc!(u, ni);
                if should_cache_user_info(cm, uc, &info.nickname) {
                    let flags =
                        ircutil::strip_member_prefixes(&info.flags, &uc.available_memberships);
                    uc.cache_user_info(
                        &info.nickname.clone(),
                        UpstreamUser {
                            nickname: info.nickname,
                            username: info.username,
                            hostname: info.hostname,
                            server: info.server,
                            flags,
                            account: info.account,
                            realname: info.realname,
                        },
                    );
                }
            }
        }
        RPL_ENDOFWHO => {
            // Some servers send RPL_TRYAGAIN followed by RPL_ENDOFWHO: the
            // WHO already ended
            let dc_id = match take_ended_early(u, ni, "WHO", msg.param(1)) {
                Some(id) => Some(id),
                None => dequeue_command(u, ni, "WHO").map(|(id, _)| id),
            };
            if let Some(i) = dc_id.and_then(|id| u.dc_index(id)) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_WHOISCERTFP | RPL_WHOISREGNICK | RPL_WHOISUSER | RPL_WHOISSERVER
        | RPL_WHOISCHANNELS | RPL_WHOISOPERATOR | RPL_WHOISIDLE | RPL_WHOISSPECIAL
        | RPL_WHOISACCOUNT | RPL_WHOISACTUALLY | RPL_WHOISHOST | RPL_WHOISMODES
        | RPL_WHOISSECURE => {
            let Some(dc_id) = current_pending_downstream(u, ni, "WHOIS") else {
                bail!(
                    "unexpected WHOIS reply {:?}: no matching pending WHOIS",
                    msg.command
                );
            };
            clear_ended_early(uc!(u, ni), "WHOIS");
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_ENDOFWHOIS => {
            // Servers send RPL_ENDOFWHOIS after ERR_NOSUCHNICK: the WHOIS
            // already ended
            let dc_id = match take_ended_early(u, ni, "WHOIS", msg.param(1)) {
                Some(id) => id,
                None => match dequeue_command(u, ni, "WHOIS") {
                    Some((id, _)) => id,
                    None => bail!("unexpected RPL_ENDOFWHOIS: no matching pending WHOIS"),
                },
            };
            if let Some(i) = u.dc_index(dc_id) {
                u.downstreams[i].send_message(msg.clone());
            }
        }
        RPL_MONONLINE | RPL_MONOFFLINE => {
            need_params(msg, 2)?;
            let targets: Vec<String> = msg.params[1].split(',').map(str::to_owned).collect();
            let online = msg.command == RPL_MONONLINE;
            let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
            let uc = uc!(u, ni);
            for target in &targets {
                let prefix = Prefix::parse(target);
                uc.monitored.set(&prefix.name, online);
            }

            // Check if the nick we want is now free
            if !online && !is_our_nick(cm, uc, &want_nick) && !uc.has_desired_nick {
                let found = targets
                    .iter()
                    .any(|t| cm(&Prefix::parse(t).name) == cm(&want_nick));
                if found {
                    logf!(uc.logger, "desired nick {want_nick:?} is now available");
                    uc.send_message(msg!("NICK", want_nick));
                }
            }

            for dc in dcs_for(&mut u.downstreams, net_id) {
                for target in &targets {
                    let prefix = Prefix::parse(target);
                    if dc.monitored.has(&prefix.name) {
                        let nick = dc.nick.clone();
                        dc.send_message(Message::new(
                            msg.command.clone(),
                            vec![nick, target.clone()],
                        ));
                    }
                }
            }
        }
        ERR_MONLISTFULL => {
            need_params(msg, 3)?;
            let limit = msg.params[1].clone();
            let targets: Vec<String> = msg.params[2].split(',').map(str::to_owned).collect();
            for dc in dcs_for(&mut u.downstreams, net_id) {
                for target in &targets {
                    if dc.monitored.has(target) {
                        let nick = dc.nick.clone();
                        dc.send_message(Message::new(
                            msg.command.clone(),
                            vec![nick, limit.clone(), target.clone()],
                        ));
                    }
                }
            }
        }
        RPL_AWAY => forward_msg_by_id(u, net_id, downstream_id, msg),
        "AWAY" => {
            // Update user flags, if we already have the flags cached
            let uc = uc!(u, ni);
            if let Some(uu) = uc.users.get(&prefix_name)
                && !uu.flags.is_empty()
            {
                let flags = if !msg.params.is_empty() {
                    uu.flags.replace('H', "G")
                } else {
                    uu.flags.replace('G', "H")
                };
                uc.cache_user_info(
                    &prefix_name,
                    UpstreamUser {
                        flags,
                        ..Default::default()
                    },
                );
            }
            forward_message(u, net_id, msg);
        }
        "ACCOUNT" => {
            need_params(msg, 1)?;
            let account = msg.params[0].clone();
            uc!(u, ni).cache_user_info(
                &prefix_name,
                UpstreamUser {
                    account,
                    ..Default::default()
                },
            );
            forward_message(u, net_id, msg);
        }
        RPL_BANLIST | RPL_INVITELIST | RPL_EXCEPTLIST | RPL_ENDOFBANLIST | RPL_ENDOFINVITELIST
        | RPL_ENDOFEXCEPTLIST => forward_msg_by_id(u, net_id, downstream_id, msg),
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn update_monitor(u: &mut User, ni: usize) {
    let net = &u.networks[ni];
    let net_id = net.id();
    let cm = net.casemap;
    let want_nick = get_nick(&u.record, Some(&net.record)).to_owned();
    let Some(uc) = u.networks[ni].conn.as_mut() else {
        return;
    };
    if !uc.isupport.contains_key("MONITOR") {
        return;
    }

    let mut add: HashSet<String> = HashSet::new();
    let mut add_list = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dc in u
        .downstreams
        .iter()
        .filter(|d| d.network_id == Some(net_id))
    {
        for target in dc.monitored.keys() {
            let target_cm = cm(target);
            if target_cm == downstream::SERVICE_NICK_CM {
                continue;
            }
            if !uc.monitored.has(&target_cm) {
                if add.insert(target_cm.clone()) {
                    add_list.push(target_cm);
                }
            } else {
                seen.insert(target_cm);
            }
        }
    }

    // Monitor our desired nick, to regain it once it's available
    let want_nick_cm = cm(&want_nick);
    if !is_our_nick(cm, uc, &want_nick) && !uc.has_desired_nick {
        if uc.monitored.has(&want_nick) {
            seen.insert(want_nick_cm);
        } else if add.insert(want_nick_cm.clone()) {
            add_list.push(want_nick_cm);
        }
    }

    let mut remove_all = true;
    let mut remove_list = Vec::new();
    for nick in uc.monitored.keys() {
        if seen.contains(&cm(nick)) {
            remove_all = false;
        } else {
            remove_list.push(nick.to_owned());
        }
    }

    // TODO: better handle the case where len(uc.monitored) + len(add_list)
    // exceeds the limit, probably by immediately sending ERR_MONLISTFULL?

    if remove_all && add_list.is_empty() && !remove_list.is_empty() {
        // Optimization when the last MONITOR-aware downstream disconnects
        uc.send_message(msg!("MONITOR", "C"));
    } else {
        let mut msgs = xirc::generate_monitor("-", &remove_list);
        msgs.extend(xirc::generate_monitor("+", &add_list));
        for m in msgs {
            uc.send_message(m);
        }
    }

    for target in remove_list {
        uc.monitored.del(&target);
        if !should_cache_user_info(cm, uc, &target) {
            uc.users.del(&target);
        }
    }
}

pub(super) fn start_regain_nick_timer(u: &mut User, ni: usize) {
    let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
    let cm = u.networks[ni].casemap;
    let uc = uc!(u, ni);
    if uc.regain_nick_backoff.is_some() || uc.regain_nick_timer.is_some() {
        panic!("start_regain_nick_timer called twice");
    }
    if is_our_nick(cm, uc, &want_nick) {
        return;
    }
    let mut backoff = Backoffer::new(
        Duration::from_secs(15),
        Duration::from_secs(600),
        Duration::from_secs(10),
    );
    let delay = backoff.next_delay();
    uc.regain_nick_backoff = Some(backoff);
    uc.schedule_regain_nick(delay, want_nick);
}

pub fn try_regain_nick(u: &mut User, ni: usize, nick: &str) {
    let want_nick = get_nick(&u.record, Some(&u.networks[ni].record)).to_owned();
    let cm = u.networks[ni].casemap;
    let uc = uc!(u, ni);
    if uc.regain_nick_timer.is_none() {
        return;
    }
    // Maybe the user has updated their desired nick
    if want_nick != nick || is_our_nick(cm, uc, &want_nick) {
        uc.stop_regain_nick_timer();
        return;
    }
    let delay = uc
        .regain_nick_backoff
        .as_mut()
        .map(|b| b.next_delay())
        .unwrap_or(Duration::from_secs(600));
    uc.schedule_regain_nick(delay, want_nick.clone());
    if !uc.pending_regain_nick.is_empty() {
        return;
    }
    uc.send_message(msg!("NICK", want_nick.clone()));
    uc.pending_regain_nick = want_nick;
}
