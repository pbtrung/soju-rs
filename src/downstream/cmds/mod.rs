//! Command handlers for registered downstream connections (port of
//! upstream's downstreamConn.handleMessageRegistered).

use anyhow::{Result, anyhow};
use jiff::Timestamp;
use sha2::{Digest, Sha512};

use crate::database as db;
use crate::downstream::{
    ERR_ALREADYREGISTERED, ERR_BADCHANMASK, ERR_BADMASK, ERR_ERRONEUSNICKNAME, ERR_MONLISTFULL,
    ERR_NICKNAMEINUSE, ERR_NONICKNAMEGIVEN, ERR_NOSUCHCHANNEL, ERR_NOSUCHSERVER, ERR_SASLFAIL,
    ERR_UNKNOWNCOMMAND, ERR_UNKNOWNERROR, ERR_USERSDONTMATCH, ILLEGAL_CHAN_CHARS,
    ILLEGAL_NICK_CHARS, LabelContext, MULTIPLEX_CAP, RPL_CHANNELMODEIS, RPL_CREATIONTIME,
    RPL_ENDOFINFO, RPL_ENDOFMONLIST, RPL_ENDOFNAMES, RPL_ENDOFWHO, RPL_ENDOFWHOIS, RPL_INFO,
    RPL_KEYVALUE, RPL_MONLIST, RPL_MONOFFLINE, RPL_MONONLINE, RPL_UMODEIS, RPL_WHOISACCOUNT,
    RPL_WHOISBOT, RPL_WHOISOPERATOR, RPL_WHOISSERVER, RPL_WHOISUSER, SERVICE_NICK, SERVICE_NICK_CM,
    SERVICE_REALNAME, attach_network, chathistory_error, dc_network, detach_network,
    enqueue_upstream_command, find_attached, finish_message, get_message_target_metadata,
    handle_metadata_sub, irc_error, need_more_params, need_params, parse_bouncer_net_id,
    parse_nickserv_credentials, send_names, send_service_privmsg, send_topic,
    send_upstream_labeled, service_prefix, unknown_command, update_network_attrs, update_nick,
    update_realname, upstream_for_command,
};
use crate::irc::{Message, Prefix, Tags};
use crate::ircutil;
use crate::msgstore::{LoadMessageOptions, SearchMessageOptions};
use crate::upstream;
use crate::user::{Event, User, dcs_for};
use crate::xirc::{self, CaseMappingMap, WhoxInfo};
use crate::{debugf, logf, msg};

macro_rules! dc {
    ($u:expr, $i:expr) => {
        *{
            // The downstream index was resolved before synchronous handler dispatch.
            let downstream = $u
                .downstreams
                .get_mut($i)
                .expect("validated downstream index");
            downstream
        }
    };
}

mod account;
mod basic;
mod bouncer;
mod history;
mod metadata;
mod privmsg;
mod query;
mod webpush;

use account::{handle_authenticate_registered, handle_clientcert};
use basic::{handle_join, handle_mode, handle_nick, handle_part, handle_setname};
use bouncer::handle_bouncer;
use history::{check_target, handle_chathistory, handle_read, handle_search};
use metadata::handle_metadata;
use privmsg::handle_privmsg;
use query::{handle_monitor, handle_who, handle_whois};
pub use webpush::*;
pub(crate) fn fail(params: &[&str]) -> anyhow::Error {
    irc_error(Message::new(
        "FAIL",
        params
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    ))
}

/// A FAIL reply for an unexpected server-side error, e.g. a database one.
fn internal_error(cmd: &str, context: &str) -> anyhow::Error {
    fail(&[cmd, "INTERNAL_ERROR", context, "Internal error"])
}

#[expect(
    clippy::too_many_lines,
    reason = "Ordered protocol/state dispatch remains together with local validation contracts; further cohesive extraction is tracked by ARCH-001."
)]
///
/// # Errors
///
/// Returns command validation errors, unavailable connection-state errors, or backend failures encountered while handling the command.
pub fn handle_message_registered(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u
        .dc_index(dc_id)
        .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
    match msg.command.as_str() {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        "CAP" => return u.downstreams[i].handle_cap(msg),
        "PING" => {
            need_params(msg, 1)?;
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
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
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let name = msg.params[0].clone();
            let ni = upstream_for_command(u, i, &msg.command)?;
            if msg.params.len() > 1 {
                // setting topic
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                    )]
                    send_upstream_labeled(u, i, ni, msg!("TOPIC", name, msg.params[1].clone()));
                }
            } else {
                // getting topic
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                let uc = u.networks[ni].upstream().ok_or_else(|| {
                    anyhow::anyhow!("required user or connection state is unavailable")
                })?;
                match uc.channels.get(&name) {
                    // we're not on that channel, pass command to upstream
                    None => send_upstream_labeled(u, i, ni, msg.clone()),
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
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
            #[expect(
                clippy::indexing_slicing,
                reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
            )]
            let channels: Vec<String> = msg.params[0].split(',').map(str::to_owned).collect();
            if channels.len() > 1 {
                // We need to potentially mix local and remote messages so we
                // drop the label
                dc!(u, i).drop_label();
            }
            for name in channels {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                let uc = u.networks[ni].upstream().ok_or_else(|| {
                    anyhow::anyhow!("required user or connection state is unavailable")
                })?;
                match uc.channels.get(&name) {
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                    )]
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
            #[expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
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
            let ni = ni.ok_or_else(|| {
                anyhow::anyhow!("required user or connection state is unavailable")
            })?;
            #[expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            let uc = u.networks[ni].upstream().ok_or_else(|| {
                anyhow::anyhow!("required user or connection state is unavailable")
            })?;
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The handler validates IRC parameter counts before accessing these fixed command fields."
                )]
                logf!(
                    uc.logger,
                    "starting {} with account name {}",
                    msg.command,
                    msg.params[0]
                );
            }
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
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                if u.networks[ni].upstream().is_none() {
                    let nick = dc!(u, i).nick.clone();
                    return Err(irc_error(msg!(
                        ERR_UNKNOWNCOMMAND,
                        nick,
                        msg.command.clone(),
                        "Disconnected from upstream network"
                    )));
                }
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
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                if u.networks[ni].upstream().is_none() {
                    return Err(irc_error(msg!(
                        ERR_UNKNOWNCOMMAND,
                        "*",
                        msg.command.clone(),
                        "Disconnected from upstream network"
                    )));
                }
            }
            send_upstream_labeled(u, i, ni, msg.clone());
        }
    }
    Ok(())
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
    // The ID comes from the client: don't let it add targets without bounds
    #[expect(
        clippy::indexing_slicing,
        reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
    )]
    let delivered = &mut u.networks[ni].delivered;
    if !delivered.has_target(&entity) && delivered.len() >= MAX_DELIVERY_TARGETS {
        return;
    }
    let client = dc!(u, i).client_name.clone();
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        u.networks[ni].delivered.store_id(&entity, &client, msg_id);
    }
}

/// The maximum number of targets with delivery receipts per network.
const MAX_DELIVERY_TARGETS: usize = 10_000;
