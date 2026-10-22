//! Keeping downstream connections in sync with upstream state: nick,
//! host, channels, topics and metadata.

use super::*;

/// The nick of a downstream connection: our nick on its network, or the
/// user's for a bouncer connection.
pub(super) fn current_nick(u: &User, net_idx: Option<usize>) -> String {
    match net_idx {
        Some(ni) => match u.networks[ni].upstream() {
            Some(uc) => uc.nick.clone(),
            None => get_nick(&u.record, Some(&u.networks[ni].record)).to_owned(),
        },
        None => get_nick(&u.record, None).to_owned(),
    }
}

pub fn update_nick(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let nick = current_nick(u, net_idx);
    let dc = &mut u.downstreams[i];
    if nick == dc.nick {
        return;
    }
    let prefix = dc.prefix();
    dc.send_message(msg!("NICK", nick.clone()).with_prefix(prefix));
    dc.nick_cm = dc.casemap(&nick);
    dc.nick = nick;
}

pub fn update_host(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let Some(uc) = net_idx.and_then(|ni| u.networks[ni].upstream()) else {
        return;
    };
    if uc.hostname.is_empty() {
        return;
    }
    let (host, user) = (uc.hostname.clone(), uc.username.clone());
    let dc = &mut u.downstreams[i];
    if host == dc.hostname && user == dc.username {
        return;
    }
    let prefix = dc.prefix();
    if dc.caps.is_enabled("chghost") {
        dc.send_message(msg!("CHGHOST", user.clone(), host.clone()).with_prefix(prefix));
    } else if host != dc.hostname {
        let nick = dc.nick.clone();
        dc.send_message(msg!(
            RPL_VISIBLEHOST,
            nick,
            host.clone(),
            "is now your visible host"
        ));
    }
    dc.hostname = host;
    dc.username = user;
}

pub fn update_realname(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    if !u.downstreams[i].caps.is_enabled("setname") {
        return;
    }
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let realname = match net_idx {
        Some(ni) => match u.networks[ni].upstream() {
            Some(uc) => uc.realname.clone(),
            None => get_realname(&u.record, Some(&u.networks[ni].record)).to_owned(),
        },
        None => get_realname(&u.record, None).to_owned(),
    };
    let dc = &mut u.downstreams[i];
    if realname != dc.realname {
        let prefix = dc.prefix();
        dc.send_message(msg!("SETNAME", realname.clone()).with_prefix(prefix));
        dc.realname = realname;
    }
}

pub fn update_account(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let account = match u.downstreams[i].network_id {
        None => u.record.username.clone(),
        Some(id) => {
            let ni = u.net_index(id);
            match ni.and_then(|ni| u.networks[ni].upstream()) {
                Some(uc) => uc.account.clone(),
                None => return,
            }
        }
    };
    let dc = &mut u.downstreams[i];
    if dc.account == account || !dc.caps.is_enabled("sasl") {
        return;
    }
    let nick = dc.nick.clone();
    let prefix = dc.prefix().to_string();
    if !account.is_empty() {
        dc.send_message(msg!(
            RPL_LOGGEDIN,
            nick,
            prefix,
            account.clone(),
            format!("You are logged in as {account}")
        ));
    } else {
        dc.send_message(msg!(RPL_LOGGEDOUT, nick, prefix, "You are logged out"));
    }
    dc.account = account;
}

/// Sends the state of a channel after a JOIN.
pub fn forward_channel(
    db: &db::Database,
    dc: &mut DownstreamConn,
    ch: &UpstreamChannel,
    net_id: i64,
    cm: CaseMapping,
) {
    assert!(ch.complete, "Tried to forward a partial channel");

    // RPL_NOTOPIC shouldn't be sent on JOIN
    if !ch.topic.is_empty() {
        send_topic(dc, ch);
    }

    let mark_read_cmd = if dc.caps.is_enabled("draft/read-marker") {
        Some("MARKREAD")
    } else if dc.caps.is_enabled("soju.im/read") {
        Some("READ")
    } else {
        None
    };
    if let Some(cmd) = mark_read_cmd {
        match db.get_read_receipt(net_id, &cm(&ch.name)) {
            Err(e) => logf!(
                dc.logger,
                "failed to get the read receipt for {:?}: {e}",
                ch.name
            ),
            Ok(r) => {
                let ts = match r.and_then(|r| r.timestamp) {
                    Some(t) => format!("timestamp={}", xirc::format_server_time(t)),
                    None => "*".to_owned(),
                };
                let prefix = dc.prefix();
                dc.send_message(msg!(cmd, ch.name.clone(), ts).with_prefix(prefix));
            }
        }
    }

    if !dc.caps.is_enabled("soju.im/no-implicit-names")
        && !dc.caps.is_enabled("no-implicit-names")
        && !dc.caps.is_enabled("draft/no-implicit-names")
    {
        send_names(dc, ch);
    }
}

pub fn send_topic(dc: &mut DownstreamConn, ch: &UpstreamChannel) {
    let nick = dc.nick.clone();
    if !ch.topic.is_empty() {
        dc.send_message(msg!(
            RPL_TOPIC,
            nick.clone(),
            ch.name.clone(),
            ch.topic.clone()
        ));
        if let Some(who) = &ch.topic_who {
            let t = ch.topic_time.map(|t| t.timestamp()).unwrap_or(-62135596800);
            dc.send_message(msg!(
                RPL_TOPICWHOTIME,
                nick,
                ch.name.clone(),
                who.to_string(),
                t.to_string()
            ));
        }
    } else {
        dc.send_message(msg!(RPL_NOTOPIC, nick, ch.name.clone(), "No topic is set"));
    }
}

pub fn send_names(dc: &mut DownstreamConn, ch: &UpstreamChannel) {
    let multi_prefix = dc.caps.is_enabled("multi-prefix");
    let members: Vec<String> = ch
        .members
        .iter()
        .map(|(nick, ms)| format!("{}{nick}", ircutil::format_member_prefix(ms, multi_prefix)))
        .collect();
    for m in xirc::generate_names_reply(&ch.name, ch.status, &members) {
        dc.send_message(m);
    }
}

pub fn get_message_target_metadata(mt: &db::MessageTarget) -> Vec<(&'static str, &'static str)> {
    let b = |v: bool| if v { "1" } else { "0" };
    vec![
        ("soju.im/pinned", b(mt.pinned)),
        ("soju.im/muted", b(mt.muted)),
        ("soju.im/blocked", b(mt.blocked)),
    ]
}

pub fn send_target_metadata(dc: &mut DownstreamConn, mt: &db::MessageTarget) {
    if !dc.caps.is_enabled("draft/metadata-2") {
        return;
    }
    let m = get_message_target_metadata(mt);
    let msgs: Vec<Message> = m
        .into_iter()
        .filter(|(k, _)| dc.metadata_subs.contains(*k))
        .map(|(k, v)| msg!("METADATA", mt.target.clone(), k, "*", v))
        .collect();
    dc.send_batch("metadata", &[], msgs);
}

/// Handles METADATA SUB/UNSUB/SUBS. Returns None if the subcommand is
/// something else. targets is the list of message targets of the network,
/// if bound to one.
pub fn handle_metadata_sub(
    dc: &mut DownstreamConn,
    msg: &Message,
    targets: Option<&dyn Fn() -> Result<Vec<db::MessageTarget>>>,
) -> Option<Result<()>> {
    if msg.params.len() < 2 {
        return Some(Err(need_more_params(&msg.command).into()));
    }
    let subcommand = msg.params[1].as_str();
    match subcommand {
        "SUB" | "UNSUB" => {
            // Listed once, for all keys
            let mut mts: Option<Vec<db::MessageTarget>> = None;
            for k in &msg.params[2..] {
                let k = k.to_lowercase();
                if !matches!(
                    k.as_str(),
                    "soju.im/pinned" | "soju.im/muted" | "soju.im/blocked"
                ) {
                    dc.send_message(msg!(
                        "FAIL",
                        msg.command.clone(),
                        "KEY_INVALID",
                        k,
                        "Invalid key"
                    ));
                    continue;
                }
                if subcommand == "SUB" {
                    dc.metadata_subs.insert(k.clone());

                    // Technically we are only required to send the METADATA
                    // of channels we are joined to, and users we are sharing
                    // a channel with or are MONITOR-ing, but it is easier to
                    // send all METADATA we are aware of, and the spec
                    // guarantees that servers MAY send these messages at any
                    // time.
                    if let Some(targets) = targets {
                        if mts.is_none() {
                            mts = match targets() {
                                Ok(t) => Some(t),
                                Err(e) => {
                                    logf!(
                                        dc.logger,
                                        "failed to list message targets for METADATA SUB: {e}"
                                    );
                                    return Some(Err(fail(&[
                                        "METADATA",
                                        "INTERNAL_ERROR",
                                        "*",
                                        "Internal error",
                                    ])));
                                }
                            };
                        }
                        for mt in mts.iter().flatten() {
                            for (key, v) in get_message_target_metadata(mt) {
                                if key == k {
                                    dc.send_message(msg!(
                                        "METADATA",
                                        mt.target.clone(),
                                        key,
                                        "*",
                                        v
                                    ));
                                }
                            }
                        }
                    }
                    dc.send_message(msg!(RPL_METADATASUBOK, "*", k));
                } else {
                    dc.metadata_subs.remove(&k);
                    dc.send_message(msg!(RPL_METADATAUNSUBOK, "*", k));
                }
            }
            Some(Ok(()))
        }
        "SUBS" => {
            let subs: Vec<String> = dc.metadata_subs.iter().cloned().collect();
            for k in subs {
                dc.send_message(msg!(RPL_METADATASUBS, "*", k));
            }
            Some(Ok(()))
        }
        _ => None,
    }
}
