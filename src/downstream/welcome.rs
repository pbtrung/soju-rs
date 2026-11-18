//! The welcome burst sent to newly registered downstream connections,
//! and backlog replay.

use super::{
    BACKLOG_LIMIT, DownstreamConn, ERR_NOMOTD, Message, RPL_MYINFO, RPL_UMODEIS, RPL_WELCOME,
    RPL_YOURHOST, Result, Tags, User, current_nick, forward_channel, load_network, logf, msg,
    send_service_notice, send_target_metadata, update_account, update_host, update_isupport,
    update_realname, update_supported_caps, xirc,
};

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
/// Sends the welcome burst to a newly registered connection.
///
/// # Errors
///
/// Returns command validation errors, unavailable connection-state errors, or backend failures encountered while handling the command.
pub fn welcome(u: &mut User, dc_id: u64) -> Result<()> {
    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        if !dc.registered {
            anyhow::bail!("cannot welcome an unregistered connection");
        }
        dc.logger = u.srv.logger.with_prefix(format!(
            "user {:?}: downstream {:?}: ",
            u.record.username, dc.info.remote_addr
        ));
    }

    load_network(u, dc_id)?;

    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        u.downstreams[i].registration = None;
    }

    update_supported_caps(u, dc_id);

    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let nick = current_nick(u, net_idx);
    let cfg = u.srv.config();
    {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        dc.nick.clone_from(&nick);
        dc.nick_cm = dc.casemap(&nick);
        dc.send_message(msg!(
            RPL_WELCOME,
            nick.clone(),
            format!("Welcome to soju, {nick}")
        ));
        dc.send_message(msg!(
            RPL_YOURHOST,
            nick.clone(),
            format!("Your host is {}", cfg.basic.hostname)
        ));
        dc.send_message(msg!(
            RPL_MYINFO,
            nick.clone(),
            cfg.basic.hostname.clone(),
            "soju",
            "aiwroO",
            "OovaimnqpsrtklbeI"
        ));
    }
    update_isupport(u, dc_id);
    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        if let Some(ni) = net_idx
            && let Some(uc) = u.networks[ni].upstream()
        {
            let modes = format!("+{}", uc.modes.0);
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                u.downstreams[i].send_message(msg!(RPL_UMODEIS, nick.clone(), modes));
            }
        }
    }
    if net_idx.is_none() && u.record.admin {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            u.downstreams[i].send_message(msg!(RPL_UMODEIS, nick.clone(), "+o"));
        }
    }

    update_host(u, dc_id);
    update_realname(u, dc_id);
    update_account(u, dc_id);
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let cm = net_idx.map_or(xirc::CASEMAPPING_ASCII, |ni| u.networks[ni].casemap);
    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        u.downstreams[i].update_casemapping(cm);
    }

    {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &mut u.downstreams[i];
        if !cfg.motd.is_empty() && net_idx.is_none() {
            for m in xirc::generate_motd(&cfg.motd) {
                dc.send_message(m);
            }
        } else {
            let hint = if net_idx.is_some() {
                "Use /motd to read the message of the day"
            } else {
                "No MOTD"
            };
            dc.send_message(msg!(ERR_NOMOTD, nick.clone(), hint));
        }
    }

    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        if u.downstreams[i]
            .caps
            .is_enabled("soju.im/bouncer-networks-notify")
        {
            send_bouncer_networks(u, i);
        }
    }

    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        if u.downstreams[i].caps.is_enabled("draft/metadata-2") {
            // No user-specific metadata
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                u.downstreams[i].send_batch("metadata", &[], Vec::new());
            }
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    if let Some(ni) = net_idx
        && u.downstreams[i].caps.is_enabled("draft/metadata-2")
    {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            for mt in u.networks[ni].flagged_targets() {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
                    send_target_metadata(&mut u.downstreams[i], &mt);
                }
            }
        }
    }

    if let Some(ni) = net_idx {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let net = &u.networks[ni];
        if let Some(uc) = net.upstream() {
            #[expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            let dc = &mut u.downstreams[i];
            for ch in uc.channels.values() {
                if !ch.complete {
                    continue;
                }
                if net.channels.get(&ch.name).is_some_and(|r| r.detached) {
                    continue;
                }
                let mut params = vec![ch.name.clone()];
                if dc.caps.is_enabled("extended-join") {
                    let account = if uc.account.is_empty() {
                        "*".to_owned()
                    } else {
                        uc.account.clone()
                    };
                    params.push(account);
                    params.push(uc.realname.clone());
                }
                let prefix = dc.prefix();
                dc.send_message(Message::new("JOIN", params).with_prefix(prefix));
                forward_channel(&u.srv.db, dc, ch, net.id(), net.casemap);
            }
        }
    }

    if let Some(ni) = net_idx
        && !u
            .downstreams
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("missing downstream"))?
            .caps
            .is_enabled("draft/chathistory")
    {
        // Only send history if we're the first connected client with that
        // name for the network
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let dc = &u.downstreams[i];
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let net_id = u.networks[ni].id();
        let first_client = !u.downstreams.iter().any(|c| {
            c.id != dc.id && c.client_name == dc.client_name && c.network_id == Some(net_id)
        });
        if first_client {
            let client = dc.client_name.clone();
            let receipts = u
                .networks
                .get_mut(ni)
                .ok_or_else(|| anyhow::anyhow!("missing network"))?
                .delivered
                .receipts(&client)?;
            for receipt in receipts {
                let receipt = receipt?;
                let target = receipt.target;
                let last_delivered = receipt.internal_msg_id;
                if last_delivered.is_empty() {
                    continue;
                }
                let i = u.dc_index(dc_id).ok_or_else(|| {
                    anyhow::anyhow!("required user or connection state is unavailable")
                })?;
                send_target_backlog(u, i, ni, &target, &last_delivered);

                // Fast-forward history to last message
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                let net = &mut u.networks[ni];
                let target_cm = net.casemap(&target);
                let store = &mut u.msg_store;
                match store.last_msg_id(&net.record, &target_cm) {
                    Ok(last_id) => net.delivered.store_id(&target, &client, &last_id)?,
                    Err(e) => {
                        let i = u.dc_index(dc_id).ok_or_else(|| {
                            anyhow::anyhow!("required user or connection state is unavailable")
                        })?;
                        {
                            #![expect(
                                clippy::indexing_slicing,
                                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                            )]
                            logf!(
                                u.downstreams[i].logger,
                                "failed to get last message ID: {e}"
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Sends the backlog of a target since a message ID, for clients without
/// chathistory.
pub fn send_target_backlog(u: &mut User, dc_idx: usize, ni: usize, target: &str, msg_id: &str) {
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        if u.downstreams[dc_idx].caps.is_enabled("draft/chathistory") {
            return;
        }
    }
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net = &u.networks[ni];
    let target_cm = net.casemap(target);
    let opts = crate::msgstore::LoadMessageOptions {
        network: &net.record,
        entity: &target_cm,
        limit: BACKLOG_LIMIT,
        events: false,
    };
    let history = match u.msg_store.load_latest_id(msg_id, &opts) {
        Ok(h) => h,
        Err(e) => {
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                logf!(
                    u.downstreams[dc_idx].logger,
                    "failed to send backlog for {target:?}: {e}"
                );
            }
            return;
        }
    };

    // Disjoint fields of the user: the network is read while the downstream
    // is written
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let net = &u.networks[ni];
    let user = &u.record;
    let detached_ch = net.channels.get(target).filter(|c| c.detached);
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let dc = &mut u.downstreams[dc_idx];
    let r = dc.begin_batch("chathistory", &[target.to_owned()], Tags::new());
    for mut m in history {
        if let Some(ch) = detached_ch {
            if net.detached_message_needs_relay(user, ch, &m) {
                relay_detached_message(dc, net.is_highlight(user, &m), &m);
            }
        } else {
            m.tags.set("batch", r.clone());
            dc.send_message(m);
        }
    }
    dc.end_batch(&r);
}

/// Sends the list of the user's networks, in a soju.im/bouncer-networks
/// batch.
pub fn send_bouncer_networks(u: &mut User, i: usize) {
    let msgs: Vec<Message> = (0..u.networks.len())
        .map(|ni| {
            {
#![expect(clippy::indexing_slicing, reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler.")]
msg!(
                "BOUNCER",
                "NETWORK",
                u.networks[ni].id().to_string(),
                u.network_attrs(ni).to_string()
            )
}
        })
        .collect();
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        u.downstreams[i].send_batch("soju.im/bouncer-networks", &[], msgs);
    }
}

pub fn relay_detached_message(dc: &mut DownstreamConn, highlight: bool, msg: &Message) {
    if msg.command != "PRIVMSG" && msg.command != "NOTICE" {
        return;
    }
    let sender = msg.prefix_name();
    let (target, text) = (msg.param(0), msg.param(1));
    let s = if highlight {
        format!("highlight in {target}: <{sender}> {text}")
    } else {
        format!("message in {target}: <{sender}> {text}")
    };
    send_service_notice(dc, &s);
}
