//! PRIVMSG, NOTICE, TAGMSG and REDACT from registered downstream
//! connections.

use super::*;

pub(super) fn handle_privmsg(u: &mut User, dc_id: u64, msg: &Message) -> Result<()> {
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
            btags.set("time", xirc::format_server_time(Utc::now()));
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
                etags.set("time", xirc::format_server_time(Utc::now()));
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
                // Each line of a multiline message is a separate command
                for line in text.split('\n').filter(|l| !l.trim().is_empty()) {
                    let res = crate::service::handle_service_privmsg(u, &mut ctx, line);
                    let prints = ctx.take_prints();
                    let deferred = ctx.take_deferred();
                    if let Some(i) = u.dc_index(dc_id) {
                        for line in prints {
                            send_service_privmsg(&mut u.downstreams[i], &line);
                        }
                        if let Err(e) = &res {
                            send_service_privmsg(&mut u.downstreams[i], &format!("error: {e}"));
                        }
                    }
                    if let (Ok(()), Some(deferred)) = (res, deferred) {
                        crate::user::spawn_deferred_service_command(u.tx.clone(), dc_id, deferred);
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

        // The lines of a multiline message aren't a single command
        if msg.command == "PRIVMSG"
            && !text.contains('\n')
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
            etags.set("time", xirc::format_server_time(Utc::now()));
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
