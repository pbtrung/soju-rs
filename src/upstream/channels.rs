//! Channels' state: membership, topics, modes and names.

use super::{
    CaseMapping, CaseMappingMap, ChannelModes, ChannelStatus, Membership, MembershipSet, Message,
    Prefix, RPL_CHANNELMODEIS, RPL_CREATIONTIME, RPL_ENDOFNAMES, RPL_INVITING, RPL_LIST,
    RPL_LISTEND, RPL_LISTSTART, RPL_NAMREPLY, RPL_NOTOPIC, RPL_TOPIC, RPL_TOPICWHOTIME,
    RPL_UMODEIS, Result, Timestamp, UpstreamChannel, UpstreamConn, UpstreamUser, User, UserModes,
    anyhow, append_log, bail, current_pending_downstream, dcs_for, dequeue_command, downstream,
    forward_message, forward_msg_by_id, ircutil, is_our_nick, logf, msg, need_params, produce,
    should_cache_user_info, xirc,
};

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
/// Handles channel messages: membership, topics, modes, names, LIST and
/// invites. Returns false if the message isn't one of them.
pub(super) fn handle_message(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
) -> Result<bool> {
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net_id = u.networks[ni].id();
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let cm = u.networks[ni].casemap;
    let prefix_name = msg.prefix_name().to_owned();

    match msg.command.as_str() {
        "JOIN" => {
            need_params(msg, 1)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
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
                "H".clone_into(&mut uu.flags);
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
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    uu.account.clone_from(&msg.params[1]);
                }
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    uu.realname.clone_from(&msg.params[2]);
                }
            }
            uc.cache_user_info(&prefix_name, uu);

            for ch in channels.split(',') {
                let uc = uc!(u, ni);
                if is_our_nick(cm, uc, &prefix_name) {
                    logf!(uc.logger, "joined channel {ch:?}");
                    // A JOIN of a channel we're in starts it over
                    forget_channel(uc, cm, ch);
                    uc.channels.set(ch, UpstreamChannel::new(ch, cm));
                    update_channel_auto_detach(u, ni, ch);
                    uc!(u, ni).send_message(msg!("MODE", ch));
                } else {
                    let Some(uch) = uc.channels.get_mut(ch) else {
                        bail!("unknown channel {ch:?}");
                    };
                    uch.members.set(&prefix_name, MembershipSet::default());
                }
                produce_for_channel(u, ni, ch, msg);
            }
        }
        "PART" => {
            need_params(msg, 1)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channels = msg.params[0].clone();
            for ch in channels.split(',') {
                let uc = uc!(u, ni);
                if is_our_nick(cm, uc, &prefix_name) {
                    logf!(uc.logger, "parted channel {ch:?}");
                    forget_channel(uc, cm, ch);
                } else {
                    remove_member(uc, cm, ch, &prefix_name)?;
                }
                produce_for_channel(u, ni, ch, msg);
            }
        }
        "KICK" => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channel = msg.params[0].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let user = msg.params[1].clone();
            let uc = uc!(u, ni);
            if is_our_nick(cm, uc, &user) {
                logf!(
                    uc.logger,
                    "kicked from channel {channel:?} by {prefix_name}"
                );
                forget_channel(uc, cm, &channel);
            } else {
                remove_member(uc, cm, &channel, &user)?;
            }
            produce(u, ni, &channel, msg, 0, false);
        }
        RPL_TOPIC | RPL_NOTOPIC => {
            need_params(msg, 3)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let name = msg.params[1].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
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
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let name = msg.params[0].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&name) else {
                bail!("unknown channel {name:?}");
            };
            if msg.params.len() > 1 {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    ch.topic.clone_from(&msg.params[1]);
                }
                ch.topic_who.clone_from(&msg.prefix);
                ch.topic_time = Some(
                    msg.tags
                        .get("time")
                        .and_then(xirc::parse_server_time)
                        .unwrap_or_else(Timestamp::now),
                );
            } else {
                ch.topic.clear();
            }
            let ch_name = ch.name.clone();
            produce(u, ni, &ch_name, msg, 0, false);
        }
        "MODE" => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let name = msg.params[0].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let mode_str = msg.params[1].clone();
            let uc = uc!(u, ni);
            if uc.is_channel(&name) {
                // channel mode change
                #[expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                let ch_name = apply_channel_modes(uc, &name, &mode_str, &msg.params[2..], false)?
                    .ok_or_else(|| anyhow!("unknown channel {name:?}"))?;
                append_log(u, ni, &ch_name, msg);
                {
                    if !u
                        .networks
                        .get(ni)
                        .ok_or_else(|| anyhow!("missing network"))?
                        .is_detached(&name)
                    {
                        forward_message(u, net_id, msg);
                    }
                }
            } else {
                // user mode change
                if name != uc.nick {
                    bail!("received MODE message for unknown nick {name:?}");
                }
                uc.modes.apply(&mode_str).map_err(|e| anyhow!(e))?;
                forward_message(u, net_id, msg);
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
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channel = &msg.params[1];
            let mode_str = msg.param(2);
            let mode_args = msg.params.get(3..).unwrap_or_default();
            let uc = uc!(u, ni);
            let first_mode = uc
                .channels
                .get(channel)
                .is_some_and(|ch| ch.modes.is_none());
            if apply_channel_modes(uc, channel, mode_str, mode_args, true)?.is_none() {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            }
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                if first_mode && !u.networks[ni].is_detached(channel) {
                    forward_message(u, net_id, msg);
                }
            }
        }
        RPL_CREATIONTIME => {
            need_params(msg, 3)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            };
            let first = ch.creation_time.is_empty();
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                ch.creation_time.clone_from(&msg.params[2]);
            }
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                if first && !u.networks[ni].is_detached(&channel) {
                    forward_message(u, net_id, msg);
                }
            }
        }
        RPL_TOPICWHOTIME => {
            need_params(msg, 4)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let Some(ch) = uc.channels.get_mut(&channel) else {
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            };
            let first = ch.topic_who.is_none();
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                ch.topic_who = Some(Prefix::parse(&msg.params[2]));
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let sec: i64 = msg.params[3]
                .parse()
                .map_err(|e| anyhow!("failed to parse topic time: {e}"))?;
            ch.topic_time = Timestamp::from_second(sec).ok();
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                if first && !u.networks[ni].is_detached(&channel) {
                    forward_message(u, net_id, msg);
                }
            }
        }
        RPL_LISTSTART | RPL_LIST => {
            let Some(dc_id) = current_pending_downstream(u, ni, "LIST") else {
                bail!("unexpected RPL_LIST: no matching pending LIST");
            };
            if let Some(i) = u.dc_index(dc_id) {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    u.downstreams[i].send_message(msg.clone());
                }
            }
        }
        RPL_LISTEND => {
            let Some((dc_id, _)) = dequeue_command(u, ni, "LIST") else {
                bail!("unexpected RPL_LISTEND: no matching pending LIST");
            };
            if let Some(i) = u.dc_index(dc_id) {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    u.downstreams[i].send_message(msg.clone());
                }
            }
        }
        RPL_NAMREPLY => {
            need_params(msg, 4)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let status_str = msg.params[1].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let name = msg.params[2].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let members = msg.params[3].clone();
            let uc = uc!(u, ni);
            if !uc.channels.has(&name) {
                // NAMES on a channel we have not joined, forward to downstream
                forward_msg_by_id(u, net_id, downstream_id, msg);
                return Ok(true);
            }
            let status = ChannelStatus::parse(&status_str).map_err(|e| anyhow!(e))?;
            let ch = uc.channels.get_mut(&name).ok_or_else(|| {
                anyhow::anyhow!("required user or connection state is unavailable")
            })?;
            ch.status = status;
            for s in members.split(' ').filter(|s| !s.is_empty()) {
                let (ms, nick) = parse_membership_prefix(&uc.available_memberships, s);
                ch.members.set(nick, ms);
            }
        }
        RPL_ENDOFNAMES => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
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
            if !u
                .networks
                .get(ni)
                .ok_or_else(|| anyhow!("missing network"))?
                .is_detached(&name)
            {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                let net = &u.networks[ni];
                let uch = net
                    .conn
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("required user or connection state is unavailable")
                    })?
                    .channels
                    .get(&name)
                    .ok_or_else(|| {
                        anyhow::anyhow!("required user or connection state is unavailable")
                    })?;
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
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let nick = msg.params[0].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channel = msg.params[1].clone();
            let uc = uc!(u, ni);
            let we_are_invited = is_our_nick(cm, uc, &nick);
            let mut blocked = false;
            if we_are_invited {
                let joined = uc.channels.has(&channel);
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
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
                }
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    blocked = u.networks[ni].message_target(&cm(&prefix_name)).blocked;
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
            if we_are_invited && !blocked {
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

impl UpstreamChannel {
    fn new(name: &str, cm: CaseMapping) -> UpstreamChannel {
        UpstreamChannel {
            name: name.to_owned(),
            topic: String::new(),
            topic_who: None,
            topic_time: None,
            status: ChannelStatus::Public,
            modes: None,
            creation_time: String::new(),
            members: CaseMappingMap::new(cm),
            complete: false,
            detach_timer: None,
        }
    }
}

/// Forgets a channel we left, and the cached info of the users we don't
/// share a channel with anymore.
fn forget_channel(uc: &mut UpstreamConn, cm: CaseMapping, name: &str) {
    let Some(mut uch) = uc.channels.del(name) else {
        return;
    };
    uch.stop_auto_detach();
    for nick in uch.members.keys() {
        if !should_cache_user_info(cm, uc, nick) {
            uc.users.del(nick);
        }
    }
}

/// Removes a member who left a channel.
fn remove_member(uc: &mut UpstreamConn, cm: CaseMapping, channel: &str, nick: &str) -> Result<()> {
    let Some(uch) = uc.channels.get_mut(channel) else {
        bail!("unknown channel {channel:?}");
    };
    uch.members.del(nick);
    if !should_cache_user_info(cm, uc, nick) {
        uc.users.del(nick);
    }
    Ok(())
}

/// Applies a mode change to a channel we're in. reset clears its modes
/// first, for a full mode list (`RPL_CHANNELMODEIS`). Returns the channel's
/// name, or None if we're not in it.
fn apply_channel_modes(
    uc: &mut UpstreamConn,
    name: &str,
    mode_str: &str,
    args: &[String],
    reset: bool,
) -> Result<Option<String>> {
    let Some(ch) = uc.channels.get_mut(name) else {
        return Ok(None);
    };
    if reset {
        ch.modes = Some(ChannelModes::new());
    }
    ircutil::apply_channel_modes(
        ch.modes.as_mut(),
        &mut ch.members,
        &uc.available_memberships,
        &uc.available_channel_modes,
        mode_str,
        args,
    )
    .map_err(|e| anyhow!(e))?;
    Ok(Some(ch.name.clone()))
}

/// Parses a NAMES entry: membership prefixes, then the nick.
fn parse_membership_prefix<'a>(memberships: &[Membership], s: &'a str) -> (MembershipSet, &'a str) {
    let b = s.as_bytes();
    let mut ms = MembershipSet::default();
    let mut i = 0;
    for m in memberships {
        if i >= b.len() {
            break;
        }
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "The surrounding length check or bounded iteration establishes this local collection index."
            )]
            if b[i] == m.prefix {
                ms.0.push(*m);
                i += 1;
            }
        }
    }
    (ms, s.get(i..).unwrap_or(s))
}

/// Produces a JOIN or PART for one of the channels it lists.
fn produce_for_channel(u: &mut User, ni: usize, ch: &str, msg: &Message) {
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
        )]
        if msg.params[0] == ch {
            produce(u, ni, ch, msg, 0, false);
        } else {
            let mut ch_msg = msg.clone();
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                ch.clone_into(&mut ch_msg.params[0]);
            }
            produce(u, ni, ch, &ch_msg, 0, false);
        }
    }
}

pub fn update_channel_auto_detach(u: &mut User, ni: usize, name: &str) {
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
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
    let Some(uch) = uc.channels.get_mut(name) else {
        return;
    };
    if dur.is_zero() {
        uch.stop_auto_detach();
        return;
    }
    let deadline = tokio::time::Instant::now() + dur;
    match &mut uch.detach_timer {
        // The timer re-arms itself when it fires before the deadline
        Some(t) if t.fires_at <= deadline => t.deadline = deadline,
        _ => uc.start_detach_timer(name, deadline),
    }
}

/// Handles the auto-detach timer of a channel firing. Returns whether the
/// channel is due to be detached: if there was activity since the timer
/// started, it's re-armed instead. A timer that has been stopped meanwhile
/// (e.g. the channel was parted) is ignored.
pub fn auto_detach_timer_fired(u: &mut User, ni: usize, name: &str) -> bool {
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let Some(uc) = u.networks[ni].upstream_mut() else {
        return false;
    };
    let Some(uch) = uc.channels.get_mut(name) else {
        return false;
    };
    let Some(t) = &uch.detach_timer else {
        return false;
    };
    let deadline = t.deadline;
    if deadline <= tokio::time::Instant::now() {
        uch.detach_timer = None;
        return true;
    }
    uc.start_detach_timer(name, deadline);
    false
}
