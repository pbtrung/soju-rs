//! WHO, WHOIS and MONITOR from registered downstream connections.

use super::{
    CaseMappingMap, ERR_MONLISTFULL, ERR_NONICKNAMEGIVEN, Message, RPL_ENDOFMONLIST, RPL_ENDOFWHO,
    RPL_ENDOFWHOIS, RPL_MONLIST, RPL_MONOFFLINE, RPL_MONONLINE, RPL_WHOISACCOUNT, RPL_WHOISBOT,
    RPL_WHOISOPERATOR, RPL_WHOISSERVER, RPL_WHOISUSER, Result, SERVICE_NICK, SERVICE_NICK_CM,
    SERVICE_REALNAME, User, WhoxInfo, dc_network, enqueue_upstream_command, irc_error, ircutil,
    msg, need_params, service_prefix, unknown_command, upstream_for_command, xirc,
};

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
pub(super) fn handle_who(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if msg.params.is_empty() {
        dc!(u, i).send_message(msg!(RPL_ENDOFWHO, "*", "*", "End of /WHO list"));
        return Ok(());
    }

    // Clients will use the first mask to match RPL_ENDOFWHO
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let end_of_who_token = msg.params[0].clone();
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let mask = msg.params[0].clone();
    let options = msg.params.get(1).cloned().unwrap_or_default();
    let (fields, whox_token) = xirc::parse_whox_options(&options);
    let hostname = u.srv.config().basic.hostname.clone();

    // TODO: support mixed bouncer/upstream WHO queries
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let dc = &u.downstreams[i];
    let mask_cm = dc.casemap(&mask);
    let ni = dc_network(u, i);
    if ni.is_none() && mask_cm == dc.nick_cm {
        let mut flags = if dc.away.is_some() { "G" } else { "H" }.to_owned();
        if u.record.admin {
            flags.push('*');
        }
        let info = WhoxInfo {
            token: whox_token,
            username: u.record.username.clone(),
            hostname: dc.hostname.clone(),
            server: hostname,
            nickname: dc.nick.clone(),
            flags,
            account: u.record.username.clone(),
            realname: dc.realname.clone(),
            ..Default::default()
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        dc.send_message(xirc::generate_whox_reply(&fields, &info));
        dc.send_message(msg!(
            RPL_ENDOFWHO,
            "*",
            end_of_who_token,
            "End of /WHO list"
        ));
        return Ok(());
    }
    if mask_cm == SERVICE_NICK_CM {
        let mut flags = "H*".to_owned();
        match ni {
            None => flags.push('B'),
            Some(ni) => {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    if let Some(uc) = u.networks[ni].upstream()
                        && let Some(Some(bot)) = uc.isupport.get("BOT")
                        && bot.len() == 1
                    {
                        flags.push_str(bot);
                    }
                }
            }
        }
        let sp = service_prefix();
        let info = WhoxInfo {
            token: whox_token,
            username: sp.user,
            hostname: sp.host,
            server: hostname,
            nickname: SERVICE_NICK.to_owned(),
            flags,
            account: SERVICE_NICK.to_owned(),
            realname: SERVICE_REALNAME.to_owned(),
            ..Default::default()
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        dc.send_message(xirc::generate_whox_reply(&fields, &info));
        dc.send_message(msg!(
            RPL_ENDOFWHO,
            "*",
            end_of_who_token,
            "End of /WHO list"
        ));
        return Ok(());
    }
    if ni.is_none() {
        dc!(u, i).send_message(msg!(
            RPL_ENDOFWHO,
            "*",
            end_of_who_token,
            "End of /WHO list"
        ));
        return Ok(());
    }

    let ni = upstream_for_command(u, i, &msg.command)?;

    // Check if we have the reply cached
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let uc = u.networks[ni]
        .upstream()
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    if let Some(l) = uc.get_cached_who(&mask, &fields) {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let multi_prefix = u.downstreams[i].caps.is_enabled("multi-prefix");
        let mut replies = Vec::new();
        for uu in l {
            let mut info = WhoxInfo {
                token: whox_token.clone(),
                username: uu.username.clone(),
                hostname: uu.hostname.clone(),
                server: uu.server.clone(),
                nickname: uu.nickname.clone(),
                flags: uu.flags.clone(),
                account: uu.account.clone(),
                realname: uu.realname.clone(),
                ..Default::default()
            };
            if uc.is_channel(&mask) {
                info.channel.clone_from(&mask);
                // Set channel membership prefixes from cached NAMES reply
                let prefixes = uc
                    .channels
                    .get(&info.channel)
                    .and_then(|ch| ch.members.get(&info.nickname))
                    .map_or_default(|ms| ircutil::format_member_prefix(ms, multi_prefix));
                // Channel membership prefixes are listed after away status
                // ('G'/'H') and optional server operator indicator ('*')
                match info.flags.find(|f| f != 'G' && f != 'H' && f != '*') {
                    None => info.flags.push_str(&prefixes),
                    Some(idx) => info.flags.insert_str(idx, &prefixes),
                }
            }
            replies.push(xirc::generate_whox_reply(&fields, &info));
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        for r in replies {
            dc.send_message(r);
        }
        dc.send_message(msg!(
            RPL_ENDOFWHO,
            "*",
            end_of_who_token,
            "End of /WHO list"
        ));
        return Ok(());
    }

    enqueue_upstream_command(u, i, ni, msg.clone());
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
pub(super) fn handle_whois(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    let nick = dc!(u, i).nick.clone();
    if msg.params.is_empty() {
        return Err(irc_error(msg!(
            ERR_NONICKNAMEGIVEN,
            nick,
            "No nickname given"
        )));
    }
    let mut mask = if msg.params.len() == 1 {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            msg.params[0].clone()
        }
    } else {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            msg.params[1].clone()
        }
    };
    // TODO: support multiple WHOIS users
    if let Some(idx) = mask.find(',') {
        mask.truncate(idx);
    }
    let hostname = u.srv.config().basic.hostname.clone();

    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let dc = &u.downstreams[i];
    if dc_network(u, i).is_none() && dc.casemap(&mask) == dc.nick_cm {
        let (host, realname) = (dc.hostname.clone(), dc.realname.clone());
        let username = u.record.username.clone();
        let admin = u.record.admin;
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        dc.send_message(msg!(
            RPL_WHOISUSER,
            nick.clone(),
            nick.clone(),
            username.clone(),
            host,
            "*",
            realname
        ));
        dc.send_message(msg!(
            RPL_WHOISSERVER,
            nick.clone(),
            nick.clone(),
            hostname,
            "soju"
        ));
        if admin {
            dc.send_message(msg!(
                RPL_WHOISOPERATOR,
                nick.clone(),
                nick.clone(),
                "is a bouncer administrator"
            ));
        }
        dc.send_message(msg!(
            RPL_WHOISACCOUNT,
            nick.clone(),
            nick.clone(),
            username,
            "is logged in as"
        ));
        dc.send_message(msg!(
            RPL_ENDOFWHOIS,
            nick.clone(),
            nick,
            "End of /WHOIS list"
        ));
        return Ok(());
    }
    if dc.casemap(&mask) == SERVICE_NICK_CM {
        let sp = service_prefix();
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        dc.send_message(msg!(
            RPL_WHOISUSER,
            nick.clone(),
            SERVICE_NICK,
            sp.user,
            sp.host,
            "*",
            SERVICE_REALNAME
        ));
        dc.send_message(msg!(
            RPL_WHOISSERVER,
            nick.clone(),
            SERVICE_NICK,
            hostname,
            "soju"
        ));
        dc.send_message(msg!(
            RPL_WHOISOPERATOR,
            nick.clone(),
            SERVICE_NICK,
            "is the bouncer service"
        ));
        dc.send_message(msg!(
            RPL_WHOISACCOUNT,
            nick.clone(),
            SERVICE_NICK,
            SERVICE_NICK,
            "is logged in as"
        ));
        dc.send_message(msg!(RPL_WHOISBOT, nick.clone(), SERVICE_NICK, "is a bot"));
        dc.send_message(msg!(
            RPL_ENDOFWHOIS,
            nick,
            SERVICE_NICK,
            "End of /WHOIS list"
        ));
        return Ok(());
    }

    let ni = upstream_for_command(u, i, &msg.command)?;
    enqueue_upstream_command(u, i, ni, msg.clone());
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
pub(super) fn handle_monitor(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    let Some(ni) = dc_network(u, i) else {
        return Err(unknown_command(&msg.command));
    };
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let Some(uc) = u.networks[ni].upstream() else {
        return Err(unknown_command(&msg.command));
    };
    if !uc.isupport.contains_key("MONITOR") {
        return Err(unknown_command(&msg.command));
    }
    need_params(msg, 1)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let subcommand = msg.params[0].clone();
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let cm = u.networks[ni].casemap;
    let nick = dc!(u, i).nick.clone();

    // TODO: support MONITOR labeled-response through upstream
    match subcommand.to_uppercase().as_str() {
        "+" | "-" => {
            need_params(msg, 2)?;
            for target in msg
                .params
                .get(1)
                .ok_or_else(|| super::need_more_params(&msg.command))?
                .split(',')
            {
                // An empty or spaced name (e.g. "a,,b", or a trailing
                // parameter "a b") isn't a nickname: it would be relayed to
                // the upstream server, and listed back in replies
                if target.is_empty() || target.contains(' ') {
                    continue;
                }
                if subcommand == "+" {
                    // Hard limit, just to avoid having downstreams fill our
                    // map
                    if dc!(u, i).monitored.len() >= 1000 {
                        dc!(u, i).send_message(msg!(
                            ERR_MONLISTFULL,
                            nick.clone(),
                            "1000",
                            target,
                            "Bouncer monitor list is full"
                        ));
                        continue;
                    }
                    dc!(u, i).monitored.set(target, ());
                    if cm(target) == SERVICE_NICK_CM {
                        // BouncerServ is never tired
                        dc!(u, i).send_message(msg!(RPL_MONONLINE, nick.clone(), target));
                        continue;
                    }
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    let uc = u.networks[ni].upstream().ok_or_else(|| {
                        anyhow::anyhow!("required user or connection state is unavailable")
                    })?;
                    if let Some(&online) = uc.monitored.get(target) {
                        let cmd = if online {
                            RPL_MONONLINE
                        } else {
                            RPL_MONOFFLINE
                        };
                        dc!(u, i).send_message(msg!(cmd, nick.clone(), target));
                    }
                } else if subcommand == "-" {
                    dc!(u, i).monitored.del(target);
                }
            }
            u.update_monitor(ni);
        }
        "C" => {
            dc!(u, i).monitored = CaseMappingMap::new(cm);
            u.update_monitor(ni);
        }
        "L" => {
            // TODO: be less lazy and pack the list
            let names: Vec<String> = dc!(u, i).monitored.keys().map(str::to_owned).collect();
            for name in names {
                dc!(u, i).send_message(msg!(RPL_MONLIST, nick.clone(), name));
            }
            dc!(u, i).send_message(msg!(RPL_ENDOFMONLIST, nick, "End of MONITOR list"));
        }
        "S" => {
            // TODO: be less lazy and pack the lists
            let names: Vec<String> = dc!(u, i).monitored.keys().map(str::to_owned).collect();
            for target in names {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                let uc = u.networks[ni].upstream().ok_or_else(|| {
                    anyhow::anyhow!("required user or connection state is unavailable")
                })?;
                let mut cmd = if uc.monitored.get(&target).copied().unwrap_or(false) {
                    RPL_MONONLINE
                } else {
                    RPL_MONOFFLINE
                };
                if cm(&target) == SERVICE_NICK_CM {
                    cmd = RPL_MONONLINE;
                }
                dc!(u, i).send_message(msg!(cmd, nick.clone(), target));
            }
        }
        _ => {}
    }
    Ok(())
}
