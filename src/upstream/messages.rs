//! Messages relayed to downstreams, batches, errors and the message store.

use super::{
    ERR_ERRONEUSNICKNAME, ERR_NEEDMOREPARAMS, ERR_NICKCOLLISION, ERR_NICKNAMEINUSE,
    ERR_NOPERMFORHOST, ERR_NOSUCHNICK, ERR_NOSUCHSERVER, ERR_PASSWDMISMATCH, ERR_UNAVAILRESOURCE,
    ERR_UNKNOWNCOMMAND, ERR_UNKNOWNERROR, ERR_YOUREBANNEDCREEP, Message, RPL_CREATED,
    RPL_GLOBALUSERS, RPL_LOCALUSERS, RPL_LUSERCHANNELS, RPL_LUSERCLIENT, RPL_LUSERME, RPL_LUSEROP,
    RPL_LUSERUNKNOWN, RPL_MOTD, RPL_MOTDSTART, RPL_NOWAWAY, RPL_STATSBLINE, RPL_STATSDLINE,
    RPL_STATSPING, RPL_STATSVLINE, RPL_TRYAGAIN, RPL_UNAWAY, RPL_YOURHOST, RegistrationError,
    Result, STORABLE_MESSAGE_TAGS, Timestamp, UpstreamBatch, User, bail, current_pending_command,
    db, dcs_for, dequeue_command, downstream, end_command_early, forward_msg_by_id, is_our_nick,
    logf, msg, need_params, parse_label, update_channel_auto_detach, xirc,
};

/// Handles messages, batches and errors. Returns false if the message isn't one
/// of them.
pub(super) fn handle_message(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    mut downstream_id: u64,
    downstream_label: &str,
    label: &str,
) -> Result<bool> {
    let network = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow::anyhow!("upstream network is unavailable"))?;
    let net_id = network.id();

    match msg.command.as_str() {
        "PING" => {
            super::connection(u, ni)?.send_message(Message::new("PONG", msg.params.clone()));
        }
        "NOTICE" | "PRIVMSG" | "TAGMSG" | "REDACT" => {
            return handle_chat_message(u, ni, msg, downstream_id);
        }
        "BATCH" => return handle_batch(u, ni, msg, downstream_id, downstream_label, label),
        ERR_NOSUCHNICK | ERR_NOSUCHSERVER => {
            return handle_err_nosuchnick(u, ni, msg, downstream_id);
        }
        ERR_UNKNOWNERROR | ERR_UNKNOWNCOMMAND | ERR_NEEDMOREPARAMS | RPL_TRYAGAIN => {
            need_params(msg, 3)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let command = msg.params[1].clone();
            if let Some((dc_id, _)) = end_command_early(u, ni, &command)
                && downstream_id == 0
                && u.dc_index(dc_id).is_some()
            {
                downstream_id = dc_id;
            }
            if command == "AUTHENTICATE" {
                let uc = super::connection(u, ni)?;
                uc.sasl_client = None;
                uc.sasl_started = false;
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        "FAIL" => {
            need_params(msg, 2)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let command = msg.params[0].clone();
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let code = msg.params[1].clone();
            if !super::connection(u, ni)?.registered && command == "*" && code == "ACCOUNT_REQUIRED"
            {
                return Err(RegistrationError(msg.clone()).into());
            }
            if let Some((dc_id, _)) = dequeue_command(u, ni, &command)
                && downstream_id == 0
                && u.dc_index(dc_id).is_some()
            {
                downstream_id = dc_id;
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        // Ignore
        "ACK" | RPL_NOWAWAY | RPL_UNAWAY | RPL_YOURHOST | RPL_CREATED => {}
        RPL_LUSERCLIENT | RPL_LUSEROP | RPL_LUSERUNKNOWN | RPL_LUSERCHANNELS | RPL_LUSERME
        | RPL_STATSVLINE | RPL_STATSPING | RPL_STATSBLINE | RPL_STATSDLINE | RPL_LOCALUSERS
        | RPL_GLOBALUSERS | RPL_MOTDSTART | RPL_MOTD => {
            // Ignore these messages if they're part of the initial
            // registration message burst. Forward them if the user
            // explicitly asked for them.
            if !super::connection(u, ni)?.got_motd {
                return Ok(true);
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        "ERROR" => {
            need_params(msg, 1)?;
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                bail!("fatal server error: {}", msg.params[0]);
            }
        }
        ERR_NICKNAMEINUSE | ERR_PASSWDMISMATCH | ERR_ERRONEUSNICKNAME | ERR_NICKCOLLISION
        | ERR_UNAVAILRESOURCE | ERR_NOPERMFORHOST | ERR_YOUREBANNEDCREEP => {
            return handle_err_nicknameinuse(u, ni, msg, downstream_id);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn handle_chat_message(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
) -> Result<bool> {
    let network = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow::anyhow!("upstream network is unavailable"))?;
    let net_id = network.id();
    let cm = network.casemap;
    let prefix_name = msg.prefix_name().to_owned();
    let is_text = msg.command == "NOTICE" || msg.command == "PRIVMSG";
    let Some(ChatRoute {
        buffer_name,
        direct_message,
        self_msg,
    }) = route_chat_message(u, ni, msg)?
    else {
        return Ok(true);
    };
    let mut highlight = false;
    let mut detached = false;
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let ch = (u.networks[ni].channels.get(&buffer_name))
        .map(|c| (c.detached, c.detach_on, c.relay_detached));
    if let Some((was_detached, detach_on, relay_detached)) = ch
        && is_text
        && !self_msg
    {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            highlight = u.networks[ni].is_highlight(&u.record, msg);
        }
        if was_detached {
            handle_detached_message(u, ni, &buffer_name, msg, highlight);
        }
        if detach_on == db::MessageFilter::Message
            || detach_on == db::MessageFilter::Default
            || (detach_on == db::MessageFilter::Highlight && highlight)
        {
            update_channel_auto_detach(u, ni, &buffer_name);
        }
        // The message may have reattached the channel
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let (is_detached, relay_detached) = (u.networks[ni].channels.get(&buffer_name))
            .map_or((was_detached, relay_detached), |c| {
                (c.detached, c.relay_detached)
            });
        if is_detached && relay_detached == db::MessageFilter::None {
            detached = true;
        }
    }

    let mut send_web_push = !self_msg
        && !detached
        && is_text
        && (highlight || direct_message)
        && u.has_web_push_subscriptions(net_id);

    let source_cm = cm(&prefix_name);
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let mt = u.networks[ni].message_target(&source_cm);
    if mt.muted || mt.blocked {
        send_web_push = false;
    }
    let blocked = mt.blocked;

    let timestamp = msg
        .tags
        .get("time")
        .and_then(xirc::parse_server_time)
        .unwrap_or_else(Timestamp::now);
    // highlight is only computed for saved channels
    // A private PRIVMSG, which clients show in its own conversation,
    // unlike NOTICEs (e.g. from services) and CTCP requests or replies
    let private_mention = direct_message
        && msg.command == "PRIVMSG"
        && (!msg.param(1).starts_with('\u{1}') || msg.param(1).starts_with("\u{1}ACTION"));
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let mention = is_text
        && !self_msg
        && !blocked
        && (private_mention
            || highlight
            || (ch.is_none() && !direct_message && u.networks[ni].is_highlight(&u.record, msg)));
    if mention && u.has_chathistory_store() {
        // Clients learn which conversations mention them from
        // CHATHISTORY TARGETS, without loading their history
        u.record_mention(ni, cm(&buffer_name), timestamp);
    }

    if send_web_push {
        u.queue_web_push(ni, cm(&buffer_name), msg.clone(), Some(timestamp));
    }

    produce(u, ni, &buffer_name, msg, downstream_id, blocked);
    Ok(true)
}

struct ChatRoute {
    buffer_name: String,
    direct_message: bool,
    self_msg: bool,
}

fn route_chat_message(u: &mut User, ni: usize, msg: &Message) -> Result<Option<ChatRoute>> {
    let network = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow::anyhow!("upstream network is unavailable"))?;
    let cm = network.casemap;
    let prefix_name = msg.prefix_name().to_owned();
    let is_text = msg.command == "NOTICE" || msg.command == "PRIVMSG";
    need_params(msg, if is_text { 2 } else { 1 })?;
    let uc = super::connection(u, ni)?;
    // remove STATUSMSG sigils from target
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let target = msg.params[0]
        .trim_start_matches(|c| uc.available_status_msg.contains(c))
        .to_owned();

    if cm(&prefix_name) == cm(downstream::SERVICE_NICK) {
        logf!(
            uc.logger,
            "skipping {} from soju's service: {msg}",
            msg.command
        );
        return Ok(None);
    }
    if cm(&target) == cm(downstream::SERVICE_NICK) {
        logf!(
            uc.logger,
            "skipping {} to soju's service: {msg}",
            msg.command
        );
        return Ok(None);
    }

    if !uc.registered
        || cm(&prefix_name) == cm(&uc.server_prefix.name)
        || target == "*"
        || target.starts_with('$')
    {
        // This is a server message
        produce(u, ni, "", msg, 0, false);
        return Ok(None);
    }

    let mut direct_message = is_our_nick(cm, uc, &target);
    let mut buffer_name = if direct_message {
        prefix_name.clone()
    } else {
        target.clone()
    };
    if let Some(t) = msg.tags.get("+draft/channel-context")
        && let Some(ch) = uc.channels.get(t)
        && ch.members.has(&prefix_name)
    {
        buffer_name.clone_from(&ch.name);
        direct_message = false;
    }

    let self_msg = is_our_nick(cm, uc, &prefix_name);
    Ok(Some(ChatRoute {
        buffer_name,
        direct_message,
        self_msg,
    }))
}

fn handle_batch(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
    downstream_label: &str,
    label: &str,
) -> Result<bool> {
    let network = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow::anyhow!("upstream network is unavailable"))?;
    let net_id = network.id();
    need_params(msg, 1)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let tag = msg.params[0].clone();
    if let Some(tag) = tag.strip_prefix('+') {
        let uc = super::connection(u, ni)?;
        if uc.batches.contains_key(tag) {
            bail!("unexpected BATCH reference tag: batch was already defined: {tag:?}");
        }
        need_params(msg, 2)?;
        // label is the batch's own, or else the one of its parent
        uc.batches.insert(
            tag.to_owned(),
            UpstreamBatch {
                label: label.to_owned(),
                multiline: None,
            },
        );
        if downstream_id != 0 && !downstream_label.is_empty() {
            let prefix = u.srv.prefix();
            for dc in dcs_for(&mut u.downstreams, net_id) {
                if dc.id != downstream_id {
                    continue;
                }
                let batch = Message::new("BATCH", msg.params.clone()).with_prefix(prefix.clone());
                match &mut dc.label {
                    // A batch nested in the labeled response: it
                    // goes in the outer batch, which has the label
                    Some(l) if !l.batch.is_empty() => dc.send_message(batch),
                    l => {
                        if let Some(l) = l {
                            l.label.clear();
                        }
                        dc.send_message(batch.with_tag("label", downstream_label));
                    }
                }
            }
        }
    } else if let Some(tag) = tag.strip_prefix('-') {
        let uc = super::connection(u, ni)?;
        let Some(b) = uc.batches.remove(tag) else {
            bail!("unknown BATCH reference tag: {tag:?}");
        };
        // BATCH - does not have @label/@batch attached to it, so
        // the downstream ID is empty. Extract it back from the batch.
        if let Ok((id, lbl)) = parse_label(&b.label)
            && id != 0
            && !lbl.is_empty()
        {
            let prefix = u.srv.prefix();
            for dc in dcs_for(&mut u.downstreams, net_id) {
                if dc.id == id {
                    dc.send_message(
                        Message::new("BATCH", msg.params.clone()).with_prefix(prefix.clone()),
                    );
                }
            }
        }
    } else {
        bail!("unexpected BATCH reference tag: missing +/- prefix: {tag:?}");
    }
    Ok(true)
}

fn handle_err_nosuchnick(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
) -> Result<bool> {
    let network = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow::anyhow!("upstream network is unavailable"))?;
    let net_id = network.id();
    let cm = network.casemap;
    // one argument WHOIS variant errors with NOSUCHNICK
    // two argument WHOIS variant errors with NOSUCHSERVER
    need_params(msg, 3)?;
    #[expect(
        clippy::indexing_slicing,
        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
    )]
    let nick = &msg.params[1];
    let is_whois_target = current_pending_command(u, ni, "WHOIS")
        .and_then(|cmd| cmd.msg.params.last())
        .is_some_and(|p| cm(p) == cm(nick));
    if is_whois_target {
        if let Some((dc_id, _)) = end_command_early(u, ni, "WHOIS")
            && let Some(i) = u.dc_index(dc_id)
        {
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                u.downstreams[i].send_message(msg.clone());
            }
        }
    } else {
        forward_msg_by_id(u, net_id, downstream_id, msg);
    }
    Ok(true)
}

fn handle_err_nicknameinuse(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
) -> Result<bool> {
    let network = u
        .networks
        .get(ni)
        .ok_or_else(|| anyhow::anyhow!("upstream network is unavailable"))?;
    let net_id = network.id();
    let cm = network.casemap;
    if msg.command == ERR_NICKNAMEINUSE {
        let uc = super::connection(u, ni)?;
        // At this point, we haven't received ISUPPORT so we don't
        // know the maximum nickname length or whether the server
        // supports MONITOR. Many servers have NICKLEN=30 so let's
        // just use that.
        if !uc.registered && uc.nick.len() + 1 < 30 {
            uc.nick.push('_');
            uc.has_desired_nick = false;
            logf!(
                uc.logger,
                "desired nick is not available, falling back to {:?}",
                uc.nick
            );
            let nick = uc.nick.clone();
            uc.send_message(msg!("NICK", nick));
            return Ok(true);
        }
    }
    // Servers refuse a nick with any of these, e.g. 437 for a nick
    // held by services
    let nick_error = matches!(
        msg.command.as_str(),
        ERR_NICKNAMEINUSE | ERR_ERRONEUSNICKNAME | ERR_NICKCOLLISION | ERR_UNAVAILRESOURCE
    );
    let uc = super::connection(u, ni)?;
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
        )]
        if nick_error
            && uc.registered
            && msg.params.len() >= 2
            && !uc.pending_regain_nick.is_empty()
            && cm(&uc.pending_regain_nick) == cm(&msg.params[1])
        {
            // This message comes from our own logic to try to regain our
            // desired nick, don't relay to downstream connections
            uc.pending_regain_nick.clear();
            return Ok(true);
        }
    }
    if !super::connection(u, ni)?.registered {
        return Err(RegistrationError(msg.clone()).into());
    }
    forward_msg_by_id(u, net_id, downstream_id, msg);
    Ok(true)
}

fn handle_detached_message(u: &mut User, ni: usize, name: &str, msg: &Message, highlight: bool) {
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net = &u.networks[ni];
    let Some(ch) = net.channels.get(name) else {
        return;
    };
    let net_id = net.id();
    let relay = crate::user::relays_detached_message(ch, highlight);
    let reattach = crate::user::reattaches_detached_channel(ch, highlight);
    if relay {
        for dc in dcs_for(&mut u.downstreams, net_id) {
            downstream::relay_detached_message(dc, highlight, msg);
        }
    }
    if reattach {
        u.attach_channel(ni, name);
        u.store_channel(ni, name);
    }
}

/// Appends a message to the message store. Returns the internal message
/// ID, or an empty string if the message isn't stored.
pub fn append_log(u: &mut User, ni: usize, entity: &str, msg: &Message) -> String {
    let store = &mut u.msg_store;
    if msg.command == "TAGMSG"
        && !msg
            .tags
            .iter()
            .any(|(k, _)| STORABLE_MESSAGE_TAGS.contains(&k))
    {
        return String::new();
    }
    // Don't store messages with a server mask target
    if entity.starts_with('$') {
        return String::new();
    }
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net = &mut u.networks[ni];
    let entity_cm = net.casemap(entity);
    if entity_cm == "nickserv" {
        // The messages sent/received from NickServ may contain
        // security-related information (like passwords). Don't store these.
        return String::new();
    }

    let tracked = match net.delivered.has_target(entity) {
        Ok(tracked) => tracked,
        Err(error) => {
            logf!(net.logger, "failed to look up receipt target: {error}");
            true
        }
    };
    if !tracked {
        // This is the first message we receive from this target. Save the
        // last message ID in delivery receipts, so that we can send the new
        // message in the backlog if an offline client reconnects.
        let last_id = match store.last_msg_id(&net.record, &entity_cm) {
            Ok(id) => id,
            Err(e) => {
                logf!(
                    net.logger,
                    "failed to log message: failed to get last message ID: {e}"
                );
                return String::new();
            }
        };
        if let Err(error) = net.delivered.initialize_target(entity, &last_id) {
            logf!(
                net.logger,
                "failed to initialize delivery receipts: {error}"
            );
        }
    }

    match store.append(&net.record, &entity_cm, msg) {
        Ok(id) => id,
        Err(e) => {
            logf!(net.logger, "failed to append message to store: {e}");
            String::new()
        }
    }
}

/// Appends a message to the logs and forwards it to connected downstream
/// connections.
///
/// `origin_id` is the ID of the downstream that sent the message. If it's not
/// 0 and the origin doesn't support echo-message, the message is forwarded
/// to all connections except the origin.
pub fn produce(
    u: &mut User,
    ni: usize,
    target: &str,
    msg: &Message,
    origin_id: u64,
    blocked: bool,
) {
    let msg_id = if target.is_empty() {
        String::new()
    } else {
        append_log(u, ni, target, msg)
    };

    // Don't forward messages if it's a detached channel
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net = &u.networks[ni];
    let detached = net.is_detached(target);
    let net_id = net.id();
    let our_nick = net.conn.as_ref().map_or("", |uc| uc.nick.as_str());
    let from_us = msg
        .prefix
        .as_ref()
        .is_some_and(|p| net.equal_casemap(&p.name, our_nick));
    for dc in dcs_for(&mut u.downstreams, net_id) {
        if blocked && !dc.metadata_subs.contains("soju.im/blocked") {
            continue;
        }
        let echo = dc.id == origin_id && from_us;
        if !detached && (!echo || dc.caps.is_enabled("echo-message")) {
            dc.send_message_with_id(msg.clone(), &msg_id);
        } else {
            dc.advance_message_with_id(msg, &msg_id);
        }
    }
}
