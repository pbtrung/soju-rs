//! Command handlers for registered downstream connections (port of
//! upstream's downstreamConn.handleMessageRegistered).

use anyhow::{Result, anyhow};
use chrono::Utc;
use sha2::{Digest, Sha512};

use crate::database as db;
use crate::downstream::*;
use crate::irc::{Message, Prefix, Tags, numerics::*};
use crate::ircutil;
use crate::msgstore::{LoadMessageOptions, SearchMessageOptions};
use crate::upstream;
use crate::user::{Event, User, dcs_for};
use crate::xirc::{self, CaseMappingMap, WhoxInfo};
use crate::{debugf, logf, msg};

macro_rules! dc {
    ($u:expr, $i:expr) => {
        $u.downstreams[$i]
    };
}

fn fail(params: &[&str]) -> anyhow::Error {
    irc_error(Message::new(
        "FAIL",
        params.iter().map(|s| s.to_string()).collect(),
    ))
}

pub fn handle_message_registered(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    match msg.command.as_str() {
        "CAP" => return handle_cap(u, dc_id, msg),
        "PING" => {
            need_params(msg, 1)?;
            let source = msg.params[0].clone();
            let destination = msg.params.get(1).cloned().unwrap_or_default();
            let hostname = u.srv.config().basic.hostname.clone();
            if !destination.is_empty() && destination != hostname {
                let nick = dc!(u, i).nick.clone();
                return Err(irc_error(msg!(
                    ERR_NOSUCHSERVER,
                    nick,
                    destination,
                    "No such server"
                )));
            }
            let p = u.srv.prefix();
            dc!(u, i).send_message(msg!("PONG", hostname, source).with_prefix(p));
        }
        "PONG" => {
            let Some(token) = msg.params.last() else {
                return Err(need_more_params(&msg.command).into());
            };
            handle_pong(u, i, token);
        }
        "USER" => {
            let nick = dc!(u, i).nick.clone();
            return Err(irc_error(msg!(
                ERR_ALREADYREGISTERED,
                nick,
                "You may not reregister"
            )));
        }
        "NICK" => handle_nick(u, i, msg)?,
        "SETNAME" => handle_setname(u, i, msg)?,
        "JOIN" => handle_join(u, i, msg)?,
        "PART" => handle_part(u, i, msg)?,
        "KICK" | "INVITE" => {
            let ni = upstream_for_command(u, i, &msg.command)?;
            send_upstream_labeled(u, i, ni, msg.clone());
        }
        "MODE" => handle_mode(u, i, msg)?,
        "TOPIC" => {
            need_params(msg, 1)?;
            let name = msg.params[0].clone();
            let ni = upstream_for_command(u, i, &msg.command)?;
            if msg.params.len() > 1 {
                // setting topic
                send_upstream_labeled(u, i, ni, msg!("TOPIC", name, msg.params[1].clone()));
            } else {
                // getting topic
                let uc = u.networks[ni].upstream().unwrap();
                match uc.channels.get(&name) {
                    // we're not on that channel, pass command to upstream
                    None => send_upstream_labeled(u, i, ni, msg.clone()),
                    Some(ch) => send_topic(&mut u.downstreams[i], ch),
                }
            }
        }
        "LIST" => {
            let ni = upstream_for_command(u, i, &msg.command)?;
            enqueue_upstream_command(u, i, ni, msg.clone());
        }
        "NAMES" => {
            let ni = upstream_for_command(u, i, &msg.command)?;
            if msg.params.is_empty() {
                let nick = dc!(u, i).nick.clone();
                dc!(u, i).send_message(msg!(RPL_ENDOFNAMES, nick, "*", "End of /NAMES list"));
                return Ok(());
            }
            let channels: Vec<String> = msg.params[0].split(',').map(str::to_owned).collect();
            if channels.len() > 1 {
                // We need to potentially mix local and remote messages so we
                // drop the label
                dc!(u, i).drop_label();
            }
            for name in channels {
                let uc = u.networks[ni].upstream().unwrap();
                match uc.channels.get(&name) {
                    Some(ch) => send_names(&mut u.downstreams[i], ch),
                    None => send_upstream_labeled(u, i, ni, msg!("NAMES", name)),
                }
            }
        }
        "WHO" => handle_who(u, i, msg)?,
        "WHOIS" => handle_whois(u, i, msg)?,
        "PRIVMSG" | "NOTICE" | "TAGMSG" | "REDACT" => handle_privmsg(u, dc_id, msg)?,
        "AUTHENTICATE" => handle_authenticate_registered(u, i, msg)?,
        "REGISTER" | "VERIFY" => {
            // Check number of params here, since we'll use that to save the
            // credentials on command success
            if (msg.command == "REGISTER" && msg.params.len() < 3)
                || (msg.command == "VERIFY" && msg.params.len() < 2)
            {
                return Err(need_more_params(&msg.command).into());
            }
            let ni = dc_network(u, i);
            let ok = ni
                .and_then(|ni| u.networks[ni].upstream())
                .is_some_and(|uc| uc.caps.is_enabled("draft/account-registration"));
            if !ok {
                return Err(fail(&[
                    &msg.command,
                    "TEMPORARILY_UNAVAILABLE",
                    "*",
                    "Upstream network account registration not supported",
                ]));
            }
            let ni = ni.unwrap();
            let uc = u.networks[ni].upstream().unwrap();
            logf!(
                uc.logger,
                "starting {} with account name {}",
                msg.command,
                msg.params[0]
            );
            enqueue_upstream_command(u, i, ni, msg.clone());
        }
        "AWAY" => {
            dc!(u, i).away = msg.params.first().cloned();
            let reply = ircutil::generate_away_reply(dc!(u, i).away.is_some());
            dc!(u, i).send_message(reply);
            if let Some(ni) = dc_network(u, i) {
                u.update_away(ni);
            }
        }
        "INFO" => {
            let Some(ni) = dc_network(u, i) else {
                let nick = dc!(u, i).nick.clone();
                dc!(u, i).send_message(msg!(RPL_INFO, nick.clone(), "soju <https://soju.im>"));
                dc!(u, i).send_message(msg!(RPL_ENDOFINFO, nick, "End of INFO"));
                return Ok(());
            };
            if u.networks[ni].upstream().is_none() {
                let nick = dc!(u, i).nick.clone();
                return Err(irc_error(msg!(
                    ERR_UNKNOWNCOMMAND,
                    nick,
                    msg.command.clone(),
                    "Disconnected from upstream network"
                )));
            }
            send_upstream_labeled(u, i, ni, msg.clone());
        }
        "MONITOR" => handle_monitor(u, i, msg)?,
        "METADATA" => handle_metadata(u, i, msg)?,
        "CHATHISTORY" => handle_chathistory(u, i, msg)?,
        "READ" | "MARKREAD" => handle_read(u, i, msg)?,
        "SEARCH" => handle_search(u, i, msg)?,
        "CLIENTCERT" => handle_clientcert(u, i, msg)?,
        "BOUNCER" => handle_bouncer(u, dc_id, msg)?,
        "WEBPUSH" => handle_webpush(u, i, msg)?,
        _ => {
            debugf!(dc!(u, i).logger, "unhandled message: {msg}");
            let Some(ni) = dc_network(u, i) else {
                return Err(unknown_command(&msg.command));
            };
            if u.networks[ni].upstream().is_none() {
                return Err(irc_error(msg!(
                    ERR_UNKNOWNCOMMAND,
                    "*",
                    msg.command.clone(),
                    "Disconnected from upstream network"
                )));
            }
            send_upstream_labeled(u, i, ni, msg.clone());
        }
    }
    Ok(())
}

fn handle_cap(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    u.downstreams[i].handle_cap(msg)
}

fn handle_pong(u: &mut User, i: usize, token: &str) {
    let Some(msg_id) = token.strip_prefix("soju-msgid-") else {
        logf!(
            dc!(u, i).logger,
            "received unrecognized PONG token {token:?}"
        );
        return;
    };
    // Acknowledges that a message has been received
    let (net_id, entity) = match crate::msgstore::parse_msg_id(msg_id) {
        Ok(v) => v,
        Err(e) => {
            logf!(dc!(u, i).logger, "failed to ACK message ID {msg_id:?}: {e}");
            return;
        }
    };
    let Some(ni) = u.net_index(net_id) else {
        return;
    };
    let client = dc!(u, i).client_name.clone();
    u.networks[ni].delivered.store_id(&entity, &client, msg_id);
}

fn handle_nick(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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

fn handle_setname(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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

fn handle_join(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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

fn handle_part(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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

fn handle_mode(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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

fn handle_who(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if msg.params.is_empty() {
        dc!(u, i).send_message(msg!(RPL_ENDOFWHO, "*", "*", "End of /WHO list"));
        return Ok(());
    }

    // Clients will use the first mask to match RPL_ENDOFWHO
    let end_of_who_token = msg.params[0].clone();
    let mask = msg.params[0].clone();
    let options = msg.params.get(1).cloned().unwrap_or_default();
    let (fields, whox_token) = xirc::parse_whox_options(&options);
    let hostname = u.srv.config().basic.hostname.clone();

    // TODO: support mixed bouncer/upstream WHO queries
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
                if let Some(uc) = u.networks[ni].upstream()
                    && let Some(Some(bot)) = uc.isupport.get("BOT")
                    && bot.len() == 1
                {
                    flags.push_str(bot);
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
    let uc = u.networks[ni].upstream().unwrap();
    if let Some(l) = uc.get_cached_who(&mask, &fields) {
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
                info.channel = mask.clone();
                // Set channel membership prefixes from cached NAMES reply
                let prefixes = uc
                    .channels
                    .get(&info.channel)
                    .and_then(|ch| ch.members.get(&info.nickname))
                    .map(|ms| ircutil::format_member_prefix(ms, multi_prefix))
                    .unwrap_or_default();
                // Channel membership prefixes are listed after away status
                // ('G'/'H') and optional server operator indicator ('*')
                match info.flags.find(|f| f != 'G' && f != 'H' && f != '*') {
                    None => info.flags.push_str(&prefixes),
                    Some(idx) => info.flags.insert_str(idx, &prefixes),
                }
            }
            replies.push(xirc::generate_whox_reply(&fields, &info));
        }
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

fn handle_whois(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    let nick = dc!(u, i).nick.clone();
    if msg.params.is_empty() {
        return Err(irc_error(msg!(
            ERR_NONICKNAMEGIVEN,
            nick,
            "No nickname given"
        )));
    }
    let mut mask = if msg.params.len() == 1 {
        msg.params[0].clone()
    } else {
        msg.params[1].clone()
    };
    // TODO: support multiple WHOIS users
    if let Some(idx) = mask.find(',') {
        mask.truncate(idx);
    }
    let hostname = u.srv.config().basic.hostname.clone();

    let dc = &u.downstreams[i];
    if dc_network(u, i).is_none() && dc.casemap(&mask) == dc.nick_cm {
        let (host, realname) = (dc.hostname.clone(), dc.realname.clone());
        let username = u.record.username.clone();
        let admin = u.record.admin;
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

fn handle_privmsg(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    let is_text = msg.command == "PRIVMSG" || msg.command == "NOTICE";
    need_params(msg, if is_text { 2 } else { 1 })?;
    let text = if is_text {
        msg.params[1].clone()
    } else {
        String::new()
    };
    let tags = ircutil::copy_client_tags(&msg.tags);

    let targets: Vec<String> = msg.params[0].split(',').map(str::to_owned).collect();
    if targets.len() > 1 {
        // We need to potentially mix local and remote messages so we drop
        // the label
        dc!(u, i).drop_label();
    }
    let hostname = u.srv.config().basic.hostname.clone();
    for name in targets {
        let Some(i) = u.dc_index(dc_id) else {
            return Ok(());
        };
        let mut params = vec![name.clone()];
        params.extend_from_slice(&msg.params[1..]);
        let bouncer_conn = dc!(u, i).network_id.is_none();

        if name == format!("${hostname}") || (name == "$*" && bouncer_conn) {
            // "$" means a server mask follows. If it's the bouncer's
            // hostname, broadcast the message to all bouncer users.
            if !u.record.admin {
                let nick = dc!(u, i).nick.clone();
                return Err(irc_error(msg!(
                    ERR_BADMASK,
                    nick,
                    name,
                    "Permission denied to broadcast message to all bouncer users"
                )));
            }
            logf!(
                dc!(u, i).logger,
                "broadcasting bouncer-wide {}: {text}",
                msg.command
            );
            let mut btags = tags.clone();
            btags.set("time", u.format_server_time(Utc::now()));
            let bmsg = Message::new(msg.command.clone(), params)
                .with_tags(btags)
                .with_prefix(service_prefix());
            u.srv.for_each_user(|h| {
                h.send(Event::Broadcast(bmsg.clone()));
            });
            continue;
        }

        let name_cm = dc!(u, i).casemap(&name);
        if bouncer_conn && name_cm == dc!(u, i).nick_cm {
            let prefix = dc!(u, i).prefix();
            dc!(u, i).send_message(
                Message::new(msg.command.clone(), params)
                    .with_tags(msg.tags.clone())
                    .with_prefix(prefix),
            );
            continue;
        }

        if name_cm == SERVICE_NICK_CM {
            if dc!(u, i).caps.is_enabled("echo-message") {
                let mut etags = tags.clone();
                etags.set("time", u.format_server_time(Utc::now()));
                let prefix = dc!(u, i).prefix();
                dc!(u, i).send_message(
                    Message::new(msg.command.clone(), params.clone())
                        .with_tags(etags)
                        .with_prefix(prefix),
                );
            }
            if msg.command == "PRIVMSG" {
                let dc = &u.downstreams[i];
                let mut ctx = crate::service::ServiceContext::for_downstream(
                    u.srv.clone(),
                    u.record.admin,
                    dc,
                );
                let res = crate::service::handle_service_privmsg(u, &mut ctx, &text);
                let prints = ctx.take_prints();
                if let Some(i) = u.dc_index(dc_id) {
                    for line in prints {
                        send_service_privmsg(&mut u.downstreams[i], &line);
                    }
                    if let Err(e) = res {
                        send_service_privmsg(&mut u.downstreams[i], &format!("error: {e}"));
                    }
                }
            }
            continue;
        }

        let ni = upstream_for_command(u, i, &msg.command)?;
        let uc = u.networks[ni].upstream().unwrap();
        // Remove STATUSMSG sigils from the target used locally (logging,
        // auto-detach). The message itself keeps them: "@#chan" only
        // reaches channel operators.
        let name = name
            .trim_start_matches(|c| uc.available_status_msg.contains(c))
            .to_owned();
        let echo_supported = uc.caps.is_enabled("echo-message");
        let our_nick = uc.nick.clone();

        if msg.command == "PRIVMSG"
            && u.networks[ni].casemap(&name) == "nickserv"
            && let Some((username, password)) = parse_nickserv_credentials(&text, &our_nick)
        {
            u.auto_save_sasl_plain(ni, &username, &password);
        }

        // If upstream doesn't support echo-message, we will send it manually
        // ourselves below (with a potential label), so we don't want to
        // relay it to upstream (and risk getting an ACK).
        let label = if echo_supported {
            dc!(u, i).take_label()
        } else {
            String::new()
        };
        let uc = u.networks[ni].upstream_mut().unwrap();
        uc.send_message_labeled(
            dc_id,
            &label,
            Message::new(msg.command.clone(), params.clone()).with_tags(tags.clone()),
        );

        // If the upstream supports echo message, we'll produce the message
        // when it is echoed from the upstream. Otherwise, produce/log it
        // here because it's the last time we'll see it.
        if !echo_supported {
            let uc = u.networks[ni].upstream().unwrap();
            let mut etags = tags.clone();
            etags.set("time", u.format_server_time(Utc::now()));
            if !uc.account.is_empty() {
                etags.set("account", uc.account.clone());
            }
            let echo = Message::new(msg.command.clone(), params)
                .with_tags(etags)
                .with_prefix(Prefix::full(
                    uc.nick.clone(),
                    uc.username.clone(),
                    uc.hostname.clone(),
                ));
            upstream::produce(u, ni, &name, &echo, dc_id, false);
        }

        upstream::update_channel_auto_detach(u, ni, &name);
    }
    Ok(())
}

fn handle_authenticate_registered(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    // Post-connection-registration AUTHENTICATE is only supported if an
    // upstream is bound and supports SASL
    let nick = dc!(u, i).nick.clone();
    let ni = dc_network(u, i);
    let ok = ni
        .and_then(|ni| u.networks[ni].upstream())
        .is_some_and(|uc| uc.caps.is_enabled("sasl"));
    if !ok {
        return Err(irc_error(msg!(
            ERR_SASLFAIL,
            nick,
            "Upstream network authentication not supported"
        )));
    }
    let ni = ni.unwrap();

    let Some(credentials) = u.downstreams[i].handle_authenticate(msg)? else {
        return Ok(());
    };

    if u.networks[ni].upstream().unwrap().sasl_client.is_some() {
        dc!(u, i).end_sasl(Some(msg!(
            ERR_SASLFAIL,
            nick,
            "Another authentication attempt is already in progress"
        )));
        return Ok(());
    }

    match credentials.mechanism.as_str() {
        "PLAIN" => {
            let plain = credentials.plain.clone().unwrap();
            if !plain.identity.is_empty() && plain.identity != plain.username {
                return Err(irc_error(msg!(
                    ERR_SASLFAIL,
                    nick,
                    "SASL PLAIN identity not supported"
                )));
            }
            let uc = u.networks[ni].upstream_mut().unwrap();
            logf!(
                uc.logger,
                "starting post-registration SASL PLAIN authentication with username {:?}",
                plain.username
            );
            uc.sasl_client = Some(upstream::SaslClient::Plain {
                username: plain.username,
                password: plain.password,
            });
            enqueue_upstream_command(u, i, ni, msg!("AUTHENTICATE", "PLAIN"));
        }
        "ANONYMOUS" => {
            let dc_id = dc!(u, i).id;
            if !u.networks[ni].record.sasl.mechanism.is_empty() {
                let mut record = u.networks[ni].record.clone();
                record.sasl = db::Sasl::default();
                if u.update_network(record, false).is_err() {
                    let Some(i) = u.dc_index(dc_id) else {
                        return Ok(());
                    };
                    logf!(dc!(u, i).logger, "failed to clear SASL credentials");
                    dc!(u, i).end_sasl(Some(msg!(ERR_SASLFAIL, nick, "Internal server error")));
                    return Ok(());
                }
            }
            if let Some(i) = u.dc_index(dc_id) {
                dc!(u, i).end_sasl(None);
            }
        }
        _ => {
            dc!(u, i).end_sasl(Some(msg!(
                ERR_SASLFAIL,
                nick,
                "Unsupported SASL authentication mechanism"
            )));
        }
    }
    Ok(())
}

fn handle_monitor(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    let Some(ni) = dc_network(u, i) else {
        return Err(unknown_command(&msg.command));
    };
    let Some(uc) = u.networks[ni].upstream() else {
        return Err(unknown_command(&msg.command));
    };
    if !uc.isupport.contains_key("MONITOR") {
        return Err(unknown_command(&msg.command));
    }
    need_params(msg, 1)?;
    let subcommand = msg.params[0].clone();
    let cm = u.networks[ni].casemap;
    let nick = dc!(u, i).nick.clone();

    // TODO: support MONITOR labeled-response through upstream
    match subcommand.to_uppercase().as_str() {
        "+" | "-" => {
            need_params(msg, 2)?;
            for target in msg.params[1].split(',') {
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
                    let uc = u.networks[ni].upstream().unwrap();
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
                let uc = u.networks[ni].upstream().unwrap();
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

fn handle_metadata(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 2)?;
    let target = msg.params[0].clone();
    let subcommand = msg.params[1].clone();
    let net_idx = dc_network(u, i);
    let net_id = net_idx.map(|ni| u.networks[ni].id());

    let srv = u.srv.clone();
    let list =
        move || -> Result<Vec<db::MessageTarget>> { srv.db.list_message_targets(net_id.unwrap()) };
    let targets_fn: Option<&dyn Fn() -> Result<Vec<db::MessageTarget>>> =
        if net_id.is_some() { Some(&list) } else { None };
    if let Some(res) = handle_metadata_sub(&mut u.downstreams[i], msg, targets_fn) {
        return res;
    }

    let mut mt: Option<db::MessageTarget> = None;
    if let Some(ni) = net_idx
        && target != "*"
    {
        let target_cm = u.networks[ni].casemap(&target);
        match u.srv.db.get_message_target(u.networks[ni].id(), &target_cm) {
            Ok(m) => mt = Some(m),
            Err(e) => {
                logf!(
                    dc!(u, i).logger,
                    "failed to get the message target for {target:?}: {e}"
                );
                return Err(fail(&[
                    "METADATA",
                    "INTERNAL_ERROR",
                    &target,
                    "Internal error",
                ]));
            }
        }
    }

    match subcommand.as_str() {
        "LIST" | "GET" => {
            let m = mt
                .as_ref()
                .map(get_message_target_metadata)
                .unwrap_or_default();
            let mut msgs = Vec::new();
            if subcommand == "LIST" {
                for (k, v) in &m {
                    msgs.push(msg!(RPL_KEYVALUE, "*", target.clone(), *k, "*", *v));
                }
            } else {
                for k in &msg.params[2..] {
                    let k = k.to_lowercase();
                    match m.iter().find(|(key, _)| *key == k) {
                        Some((_, v)) => {
                            msgs.push(msg!(RPL_KEYVALUE, "*", target.clone(), k, "*", *v))
                        }
                        None => {
                            msgs.push(msg!("FAIL", "METADATA", "KEY_INVALID", k, "Invalid key"))
                        }
                    }
                }
            }
            dc!(u, i).send_batch("metadata", &[], msgs);
        }
        "SET" | "CLEAR" => {
            let changes: Vec<(String, Option<String>)> = if subcommand == "SET" {
                need_params(msg, 3)?;
                vec![(msg.params[2].to_lowercase(), msg.params.get(3).cloned())]
            } else {
                vec![
                    ("soju.im/pinned".to_owned(), None),
                    ("soju.im/muted".to_owned(), None),
                    ("soju.im/blocked".to_owned(), None),
                ]
            };
            let batch = if subcommand == "CLEAR" {
                Some(dc!(u, i).begin_batch("metadata", &[], Tags::new()))
            } else {
                None
            };
            set_message_target_metadata(u, i, &target, mt.as_mut(), &changes, batch.as_deref());
            if let Some(b) = batch {
                dc!(u, i).end_batch(&b);
            }
            if let (Some(mut mt), Some(net_id)) = (mt, net_id)
                && let Err(e) = u.srv.db.store_message_target(net_id, &mut mt)
            {
                logf!(
                    dc!(u, i).logger,
                    "failed to store the message target for {target:?}: {e}"
                );
                return Err(fail(&[
                    "METADATA",
                    "INTERNAL_ERROR",
                    &target,
                    "Internal error",
                ]));
            }
        }
        _ => {
            // TODO: support SYNC
            return Err(fail(&[
                "METADATA",
                "INVALID_PARAMS",
                &subcommand,
                "Unknown command",
            ]));
        }
    }
    Ok(())
}

fn parse_metadata_bool(v: &Option<String>) -> Option<(bool, &'static str)> {
    match v.as_deref() {
        None | Some("0") => Some((false, "0")),
        Some("1") => Some((true, "1")),
        _ => None,
    }
}

fn set_message_target_metadata(
    u: &mut User,
    i: usize,
    target: &str,
    mut mt: Option<&mut db::MessageTarget>,
    changes: &[(String, Option<String>)],
    batch: Option<&str>,
) {
    let tags = match batch {
        Some(b) => Tags::from_iter([("batch", b)]),
        None => Tags::new(),
    };
    let net_id = u.downstreams[i].network_id;
    for (k, v) in changes {
        let Some(mt) = mt.as_deref_mut() else {
            dc!(u, i).send_message(
                msg!("FAIL", "METADATA", "KEY_INVALID", k.clone(), "Invalid key")
                    .with_tags(tags.clone()),
            );
            continue;
        };
        let field = match k.as_str() {
            "soju.im/pinned" => &mut mt.pinned,
            "soju.im/muted" => &mut mt.muted,
            "soju.im/blocked" => &mut mt.blocked,
            _ => {
                dc!(u, i).send_message(
                    msg!("FAIL", "METADATA", "KEY_INVALID", k.clone(), "Invalid key")
                        .with_tags(tags.clone()),
                );
                continue;
            }
        };
        let Some((b, mv)) = parse_metadata_bool(v) else {
            dc!(u, i).send_message(
                msg!("FAIL", "METADATA", "VALUE_INVALID", "Invalid value").with_tags(tags.clone()),
            );
            continue;
        };
        *field = b;
        dc!(u, i).send_message(
            msg!(RPL_KEYVALUE, "*", target, k.clone(), "*", mv).with_tags(tags.clone()),
        );
        if let Some(net_id) = net_id {
            for d in dcs_for(&mut u.downstreams, net_id) {
                d.send_message(msg!("METADATA", target, k.clone(), "*", mv));
            }
        }
    }
}

fn handle_chathistory(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
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
    let store = u.msg_store.as_mut().unwrap();
    let now = Utc::now();

    let history = match subcommand.as_str() {
        "BEFORE" => store.load_before_time(bounds[0], None, &options),
        "LATEST" => store.load_before_time(Some(now), bounds[0], &options),
        "AFTER" => store.load_after_time(bounds[0], Some(now), &options),
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
                Some(now),
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
            let msgs: Vec<Message> = targets
                .into_iter()
                .filter(|t| !net.channels.get(&t.name).is_some_and(|c| c.detached))
                .map(|t| {
                    msg!(
                        "CHATHISTORY",
                        "TARGETS",
                        t.name,
                        xirc::format_server_time(t.latest_message)
                    )
                })
                .collect();
            dc!(u, i).send_batch("draft/chathistory-targets", &[], msgs);
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

fn handle_read(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if msg.params.is_empty() {
        return Err(fail(&[
            &msg.command,
            "NEED_MORE_PARAMS",
            "Missing parameters",
        ]));
    }
    let target = msg.params[0].clone();
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
            return Err(fail(&[
                &msg.command,
                "INTERNAL_ERROR",
                &target,
                "Internal error",
            ]));
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
        if timestamp > now {
            timestamp = now;
        }
        if r.timestamp.is_none_or(|t| t < timestamp) {
            r.timestamp = Some(timestamp);
            if let Err(e) = u.srv.db.store_read_receipt(net_id, &mut r) {
                logf!(
                    dc!(u, i).logger,
                    "failed to store receipt for {target:?}: {e}"
                );
                return Err(fail(&[
                    &msg.command,
                    "INTERNAL_ERROR",
                    &target,
                    "Internal error",
                ]));
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

fn handle_search(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if !matches!(&u.msg_store, Some(s) if s.supports_search()) {
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

    let messages = match u
        .msg_store
        .as_mut()
        .unwrap()
        .search(&u.networks[ni].record, &opts)
    {
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

fn handle_clientcert(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    need_params(msg, 1)?;
    let subcommand = msg.params[0].clone();
    if !u.srv.config().basic.client_cert_auth {
        return Err(unknown_command(&msg.command));
    }
    let user_id = u.record.id;
    match subcommand.to_uppercase().as_str() {
        "LIST" => {
            let certs = u.srv.db.list_device_certificates(user_id)?;
            let msgs: Vec<Message> = certs
                .into_iter()
                .map(|cert| {
                    let time = match cert.last_used {
                        Some(t) => xirc::format_server_time(t),
                        None => "0001-01-01T00:00:00.000Z".to_owned(),
                    };
                    let attrs = Tags::from_iter([
                        ("time", time),
                        ("ip", cert.last_ip),
                        ("name", cert.label),
                    ]);
                    msg!(
                        "CLIENTCERT",
                        "LIST",
                        hex::encode(&cert.fingerprint).to_uppercase(),
                        attrs.to_string()
                    )
                })
                .collect();
            dc!(u, i).send_batch("soju.im/client-cert", &[], msgs);
        }
        "CREATE" => {
            need_params(msg, 2)?;
            let name = msg.params[1].clone();
            let Some(pc) = dc!(u, i).info.peer_cert.clone() else {
                return Err(fail(&[
                    "CLIENTCERT",
                    "NOCERT",
                    "CREATE",
                    "No TLS client certificate is used",
                ]));
            };
            let mut cert = db::DeviceCertificate {
                label: name,
                fingerprint: Sha512::digest(&pc).to_vec(),
                ..Default::default()
            };
            cert.mark_used(
                dc!(u, i)
                    .info
                    .remote_ip
                    .map(|ip| std::net::SocketAddr::new(ip, 0)),
            );
            if let Err(e) = u.srv.db.store_device_certificate(user_id, &mut cert)
                && !e.is::<db::DuplicateDeviceCertificate>()
            {
                logf!(dc!(u, i).logger, "failed creating device certificate: {e}");
                return Err(fail(&[
                    "CLIENTCERT",
                    "INTERNAL_ERROR",
                    "CREATE",
                    "Could not create device certificate",
                ]));
            }
            dc!(u, i).send_message(msg!("CLIENTCERT", "CREATE"));
        }
        "DELETE" => {
            let (fingerprint, h) = if msg.params.len() > 1 {
                let fp = msg.params[1].clone();
                match hex::decode(&fp) {
                    Ok(h) if h.len() == 64 => (fp, h),
                    _ => {
                        return Err(fail(&[
                            "CLIENTCERT",
                            "INTERNAL_ERROR",
                            "DELETE",
                            "Invalid device certificate fingerprint",
                        ]));
                    }
                }
            } else {
                let Some(pc) = dc!(u, i).info.peer_cert.clone() else {
                    return Err(fail(&[
                        "CLIENTCERT",
                        "NOCERT",
                        "DELETE",
                        "No TLS client certificate is used",
                    ]));
                };
                let h = Sha512::digest(&pc).to_vec();
                (hex::encode(&h).to_uppercase(), h)
            };
            if let Err(e) = u.srv.db.delete_device_certificate(user_id, &h) {
                logf!(dc!(u, i).logger, "failed deleting device certificate: {e}");
                return Err(fail(&[
                    "CLIENTCERT",
                    "INTERNAL_ERROR",
                    "DELETE",
                    "Could not delete device certificate",
                ]));
            }
            dc!(u, i).send_message(msg!("CLIENTCERT", "DELETE", fingerprint));
        }
        _ => {
            return Err(fail(&[
                "CLIENTCERT",
                "UNKNOWN_COMMAND",
                &subcommand,
                "Unknown subcommand",
            ]));
        }
    }
    Ok(())
}

fn handle_bouncer(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    need_params(msg, 1)?;
    let subcommand = msg.params[0].clone();
    match subcommand.to_uppercase().as_str() {
        "BIND" => {
            return Err(fail(&[
                "BOUNCER",
                "REGISTRATION_IS_COMPLETED",
                "BIND",
                "Cannot bind to a network after registration",
            ]));
        }
        "LISTNETWORKS" => {
            let msgs: Vec<Message> = (0..u.networks.len())
                .map(|ni| {
                    msg!(
                        "BOUNCER",
                        "NETWORK",
                        u.networks[ni].id().to_string(),
                        u.network_attrs(ni).to_string()
                    )
                })
                .collect();
            dc!(u, i).send_batch("soju.im/bouncer-networks", &[], msgs);
        }
        "ADDNETWORK" => {
            need_params(msg, 2)?;
            let attrs = Tags::parse(&msg.params[1]);
            let mut record = db::Network::new("");
            record.nick = dc!(u, i).nick.clone();
            update_network_attrs(&mut record, &attrs, &subcommand)?;
            if record.nick == u.record.username {
                record.nick.clear();
            }
            if record.realname == u.record.realname {
                record.realname.clear();
            }
            let ni = match u.create_network(record, true) {
                Ok(ni) => ni,
                Err(e) => {
                    return Err(fail(&[
                        "BOUNCER",
                        "UNKNOWN_ERROR",
                        &subcommand,
                        &format!("Failed to create network: {e}"),
                    ]));
                }
            };
            let id = u.networks[ni].id();
            let i = u.dc_index(dc_id).unwrap();
            dc!(u, i).send_message(msg!("BOUNCER", "ADDNETWORK", id.to_string()));
        }
        "CHANGENETWORK" => {
            need_params(msg, 3)?;
            let id_str = msg.params[1].clone();
            let id = parse_bouncer_net_id(&subcommand, &id_str)?;
            let attrs = Tags::parse(&msg.params[2]);
            let Some(ni) = u.net_index(id) else {
                return Err(fail(&[
                    "BOUNCER",
                    "INVALID_NETID",
                    &subcommand,
                    &id_str,
                    "Invalid network ID",
                ]));
            };
            let mut record = u.networks[ni].record.clone();
            let was_enabled = record.enabled;
            update_network_attrs(&mut record, &attrs, &subcommand)?;
            if record.nick == u.record.username {
                record.nick.clear();
            }
            if record.realname == u.record.realname {
                record.realname.clear();
            }
            if let Err(e) = u.update_network(record, !was_enabled) {
                return Err(fail(&[
                    "BOUNCER",
                    "UNKNOWN_ERROR",
                    &subcommand,
                    &format!("Failed to update network: {e}"),
                ]));
            }
            if let Some(i) = u.dc_index(dc_id) {
                dc!(u, i).send_message(msg!("BOUNCER", "CHANGENETWORK", id_str));
            }
        }
        "DELNETWORK" => {
            need_params(msg, 2)?;
            let id_str = msg.params[1].clone();
            let id = parse_bouncer_net_id(&subcommand, &id_str)?;
            if u.net_index(id).is_none() {
                return Err(fail(&[
                    "BOUNCER",
                    "INVALID_NETID",
                    &subcommand,
                    &id_str,
                    "Invalid network ID",
                ]));
            }
            u.delete_network(id)?;
            if let Some(i) = u.dc_index(dc_id) {
                dc!(u, i).send_message(msg!("BOUNCER", "DELNETWORK", id_str));
            }
        }
        _ => {
            return Err(fail(&[
                "BOUNCER",
                "UNKNOWN_COMMAND",
                &subcommand,
                "Unknown subcommand",
            ]));
        }
    }
    Ok(())
}

fn handle_webpush(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if !dc!(u, i).caps.is_enabled("soju.im/webpush") {
        return Err(unknown_command(&msg.command));
    }
    need_params(msg, 1)?;
    let subcommand = msg.params[0].clone();
    let user_id = u.record.id;
    let network_id = dc!(u, i).network_id.unwrap_or(0);
    let Some(sender) = u.srv.web_push() else {
        return Err(fail(&[
            "WEBPUSH",
            "INTERNAL_ERROR",
            &subcommand,
            "Internal error",
        ]));
    };

    match subcommand.as_str() {
        "REGISTER" => {
            need_params(msg, 3)?;
            let endpoint = msg.params[1].clone();
            let raw_keys = Tags::parse(&msg.params[2]);
            let (Some(auth), Some(p256dh)) = (raw_keys.get("auth"), raw_keys.get("p256dh")) else {
                return Err(fail(&[
                    "WEBPUSH",
                    "INVALID_PARAMS",
                    &subcommand,
                    "Missing auth or p256dh key",
                ]));
            };
            let mut new_sub = db::WebPushSubscription {
                endpoint: endpoint.clone(),
                keys: db::WebPushKeys {
                    auth: auth.to_owned(),
                    p256dh: p256dh.to_owned(),
                    vapid: sender.vapid_public.clone(),
                },
                ..Default::default()
            };

            let subs = match u.srv.db.list_web_push_subscriptions(user_id, network_id) {
                Ok(s) => s,
                Err(e) => {
                    logf!(
                        dc!(u, i).logger,
                        "failed to fetch Web push subscription: {e}"
                    );
                    return Err(fail(&[
                        "WEBPUSH",
                        "INTERNAL_ERROR",
                        &subcommand,
                        "Internal error",
                    ]));
                }
            };
            if subs.len() > 25 {
                return Err(fail(&[
                    "WEBPUSH",
                    "INTERNAL_ERROR",
                    &subcommand,
                    "Too many subscriptions",
                ]));
            }

            let mut update_sub = true;
            if let Some(old) = subs.iter().find(|s| s.endpoint == endpoint) {
                // Update the old subscription instead of creating a new one
                new_sub.id = old.id;
                // Rate-limit subscription checks
                let stale = old
                    .updated_at
                    .is_none_or(|t| Utc::now() - t > chrono::Duration::hours(24));
                update_sub = stale || old.keys != new_sub.keys;
            }

            // Send a test Web Push message, to make sure the endpoint is
            // valid
            if update_sub {
                if let Err(e) = sanity_check_web_push_endpoint(&new_sub.endpoint) {
                    logf!(
                        dc!(u, i).logger,
                        "failed to sanity check Web push endpoint {:?}: {e}",
                        new_sub.endpoint
                    );
                    return Err(fail(&[
                        "WEBPUSH",
                        "INVALID_PARAMS",
                        &subcommand,
                        "Invalid endpoint",
                    ]));
                }
                let test = msg!(
                    "NOTE",
                    "WEBPUSH",
                    "REGISTERED",
                    "Push notifications enabled"
                );
                if let Err(e) =
                    crate::server::block_on(crate::user::send_web_push(&sender, &new_sub, &test))
                {
                    logf!(
                        dc!(u, i).logger,
                        "failed to send Web push notification to endpoint {:?}: {e}",
                        new_sub.endpoint
                    );
                    return Err(fail(&[
                        "WEBPUSH",
                        "INVALID_PARAMS",
                        &subcommand,
                        "Invalid endpoint",
                    ]));
                }
                if let Err(e) =
                    u.srv
                        .db
                        .store_web_push_subscription(user_id, network_id, &mut new_sub)
                {
                    logf!(
                        dc!(u, i).logger,
                        "failed to store Web push subscription: {e}"
                    );
                    return Err(fail(&[
                        "WEBPUSH",
                        "INTERNAL_ERROR",
                        &subcommand,
                        "Internal error",
                    ]));
                }
            }
            dc!(u, i).send_message(msg!("WEBPUSH", "REGISTER", endpoint));
        }
        "UNREGISTER" => {
            need_params(msg, 2)?;
            let endpoint = msg.params[1].clone();
            let subs = match u.srv.db.list_web_push_subscriptions(user_id, network_id) {
                Ok(s) => s,
                Err(e) => {
                    logf!(
                        dc!(u, i).logger,
                        "failed to fetch Web push subscription: {e}"
                    );
                    return Err(fail(&[
                        "WEBPUSH",
                        "INTERNAL_ERROR",
                        &subcommand,
                        "Internal error",
                    ]));
                }
            };
            if let Some(old) = subs.iter().find(|s| s.endpoint == endpoint)
                && let Err(e) = u.srv.db.delete_web_push_subscription(old.id)
            {
                logf!(
                    dc!(u, i).logger,
                    "failed to delete Web push subscription: {e}"
                );
                return Err(fail(&[
                    "WEBPUSH",
                    "INTERNAL_ERROR",
                    &subcommand,
                    "Internal error",
                ]));
            }
            dc!(u, i).send_message(msg!("WEBPUSH", "UNREGISTER", endpoint));
        }
        _ => {
            return Err(fail(&[
                "WEBPUSH",
                "INVALID_PARAMS",
                &subcommand,
                "Unknown command",
            ]));
        }
    }
    Ok(())
}

fn sanity_check_web_push_endpoint(endpoint: &str) -> Result<()> {
    let u = db::parse_url(endpoint)?;
    if u.scheme != "https" {
        anyhow::bail!("scheme must be HTTPS");
    }
    Ok(())
}
