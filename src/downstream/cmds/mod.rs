//! Command handlers for registered downstream connections (port of
//! upstream's downstreamConn.handleMessageRegistered).

use anyhow::{Result, anyhow};
use chrono::Utc;
use sha2::{Digest, Sha512};

use crate::database as db;
use crate::downstream::*;
use crate::irc::{Message, Prefix, Tags};
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

mod account;
mod basic;
mod bouncer;
mod history;
mod metadata;
mod privmsg;
mod query;
mod webpush;

use account::*;
use basic::*;
use bouncer::*;
use history::*;
use metadata::*;
use privmsg::*;
use query::*;
pub use webpush::*;
pub(crate) fn fail(params: &[&str]) -> anyhow::Error {
    irc_error(Message::new(
        "FAIL",
        params.iter().map(|s| s.to_string()).collect(),
    ))
}

pub fn handle_message_registered(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    match msg.command.as_str() {
        "CAP" => return u.downstreams[i].handle_cap(msg),
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
    let delivered = &mut u.networks[ni].delivered;
    if !delivered.has_target(&entity) && delivered.len() >= MAX_DELIVERY_TARGETS {
        return;
    }
    let client = dc!(u, i).client_name.clone();
    u.networks[ni].delivered.store_id(&entity, &client, msg_id);
}

/// The maximum number of targets with delivery receipts per network.
const MAX_DELIVERY_TARGETS: usize = 10_000;
