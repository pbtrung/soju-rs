//! Connection registration: capabilities, SASL and ISUPPORT.

use super::*;

/// Sends the registration messages.
pub fn register(u: &mut User, ni: usize) {
    let net = &mut u.networks[ni];
    let nick = get_nick(&u.record, Some(&net.record)).to_owned();
    let username = get_username(&u.record, Some(&net.record)).to_owned();
    let realname = get_realname(&u.record, Some(&net.record)).to_owned();
    let pass = net.record.pass.clone();
    let uc = net.conn.as_mut().unwrap();
    uc.nick = nick.clone();
    uc.username = username.clone();
    uc.realname = realname.clone();

    uc.send_message(msg!("CAP", "LS", "302"));
    if !pass.is_empty() {
        uc.send_message(msg!("PASS", pass));
    }
    uc.send_message(msg!("NICK", nick));
    uc.send_message(msg!("USER", username, "0", "*", realname));
}

/// Runs after RPL_WELCOME: sends connect commands, then joins channels,
/// possibly after a short delay.
pub fn finish_registration(u: &mut User, ni: usize) {
    let net = &mut u.networks[ni];
    let commands = net.record.connect_commands.clone();
    let mechanism = net.record.sasl.mechanism.clone();
    let uc = net.conn.as_mut().unwrap();

    let mut connect_msgs = Vec::new();
    for command in &commands {
        match Message::parse(command) {
            Ok(mut m) => {
                if !uc.caps.is_enabled("message-tags") {
                    m.tags.clear();
                }
                connect_msgs.push(m);
            }
            Err(e) => logf!(
                uc.logger,
                "failed to parse connect command {command:?}: {e}"
            ),
        }
    }

    // Some channels require the user to be logged in to be joined, which may
    // need to happen asynchronously either via connect commands or CertFP
    // without SASL EXTERNAL on networks that don't support this. Work
    // around these old networks by sending connect commands
    // CONNECT_COMMAND_DELAY apart, then leaving 3 more seconds for the
    // authentication to be processed. Meanwhile, server messages are
    // buffered (PINGs are answered).
    let mut delay = false;
    if !commands.is_empty() {
        debugf!(
            uc.logger,
            "sending {} connect command(s)",
            connect_msgs.len()
        );
        // Connect commands usually send a message to NickServ or similar,
        // which asynchronously informs the IRC server that the user is
        // logged in.
        delay = true;
    }
    if mechanism == "EXTERNAL" && !uc.supports_sasl("EXTERNAL") {
        debugf!(
            uc.logger,
            "client certificate configured but the upstream doesn't advertise SASL EXTERNAL, delaying joining channels"
        );
        delay = true;
    }

    if delay {
        uc.delayed_messages = Some(Vec::new());
        let tx = uc.events.clone();
        let conn = uc.conn.clone();
        let srv = uc.srv.clone();
        let (net_id, conn_id) = (uc.net_id, uc.id);
        tokio::spawn(async move {
            // Servers may need time to process each connect command
            for m in connect_msgs {
                if conn.is_closed() {
                    return;
                }
                metrics::inc(&srv.metrics.upstream_out_messages_total);
                conn.send_message(m);
                tokio::time::sleep(CONNECT_COMMAND_DELAY).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = tx.send(Event::UpstreamRegistrationDelayDone { net_id, conn_id });
        });
    } else {
        u.attach_upstream(ni);
    }
}

/// Handles registration messages: capabilities, SASL, welcome and ISUPPORT.
/// Returns false if the message isn't one of them.
pub(super) fn handle_message(
    u: &mut User,
    ni: usize,
    msg: &mut Message,
    downstream_id: u64,
) -> Result<bool> {
    let net_id = u.networks[ni].id();

    match msg.command.as_str() {
        "CAP" => {
            need_params(msg, 2)?;
            let sub_cmd = msg.params[1].to_uppercase();
            let sub_params = msg.params[2..].to_vec();
            match sub_cmd.as_str() {
                "LS" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    let caps = sub_params.last().unwrap().clone();
                    let uc = uc!(u, ni);
                    uc.handle_supported_caps(&caps);
                    if cap_ls_continued(&sub_params) {
                        return Ok(true); // wait to receive all capabilities
                    }
                    uc.update_caps();
                    if request_sasl(u, ni) {
                        return Ok(true); // we'll send CAP END after authentication is completed
                    }
                    uc!(u, ni).send_message(msg!("CAP", "END"));
                }
                "ACK" | "NAK" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    for name in sub_params[0].split_whitespace() {
                        let mut enable = sub_cmd == "ACK";
                        let mut name = name;
                        if let Some(n) = name.strip_prefix('-') {
                            name = n;
                            enable = false;
                        }
                        handle_cap_ack(u, ni, &name.to_lowercase(), enable)?;
                    }
                    if uc!(u, ni).registered {
                        for id in dc_ids_for(&u.downstreams, net_id) {
                            downstream::update_supported_caps(u, id);
                        }
                    }
                }
                "NEW" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    let uc = uc!(u, ni);
                    uc.handle_supported_caps(&sub_params[0]);
                    uc.update_caps();
                }
                "DEL" => {
                    if sub_params.is_empty() {
                        return Err(downstream::need_more_params(&msg.command).into());
                    }
                    let uc = uc!(u, ni);
                    for c in sub_params[0].split_whitespace() {
                        uc.caps.del(&c.to_lowercase());
                    }
                    if uc.registered {
                        for id in dc_ids_for(&u.downstreams, net_id) {
                            downstream::update_supported_caps(u, id);
                        }
                    }
                }
                _ => debugf!(uc!(u, ni).logger, "unhandled message: {msg}"),
            }
        }
        "AUTHENTICATE" => {
            let uc = uc!(u, ni);
            let Some(client) = &uc.sasl_client else {
                bail!("received unexpected AUTHENTICATE message");
            };
            // TODO: if a challenge is 400 bytes long, buffer it
            if msg.params.is_empty() {
                uc.send_message(msg!("AUTHENTICATE", "*"));
                return Err(downstream::need_more_params(&msg.command).into());
            }
            let challenge_str = &msg.params[0];
            if challenge_str != "+" && BASE64.decode(challenge_str).is_err() {
                uc.send_message(msg!("AUTHENTICATE", "*"));
                bail!("illegal base64 data");
            }
            if uc.sasl_started {
                uc.send_message(msg!("AUTHENTICATE", "*"));
                bail!("sasl: unexpected server challenge");
            }
            let resp = client.start();
            uc.sasl_started = true;
            for m in xirc::generate_sasl(&resp) {
                uc.send_message(m);
            }
        }
        RPL_LOGGEDIN => {
            need_params(msg, 3)?;
            let uc = uc!(u, ni);
            uc.account = msg.params[2].clone();
            let prefix = Prefix::parse(&msg.params[1]);
            uc.username = prefix.user;
            uc.hostname = prefix.host;
            logf!(uc.logger, "logged in with account {:?}", uc.account);
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_account(u, id);
                downstream::update_host(u, id);
            }
        }
        RPL_LOGGEDOUT => {
            need_params(msg, 2)?;
            let uc = uc!(u, ni);
            uc.account.clear();
            let prefix = Prefix::parse(&msg.params[1]);
            uc.username = prefix.user;
            uc.hostname = prefix.host;
            logf!(uc.logger, "logged out");
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_account(u, id);
                downstream::update_host(u, id);
            }
        }
        RPL_VISIBLEHOST => {
            need_params(msg, 2)?;
            let uc = uc!(u, ni);
            match msg.params[1].split_once('@') {
                Some((user, host)) => {
                    uc.username = user.to_owned();
                    uc.hostname = host.to_owned();
                }
                None => uc.hostname = msg.params[1].clone(),
            }
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_host(u, id);
            }
        }
        ERR_NICKLOCKED | RPL_SASLSUCCESS | ERR_SASLFAIL | ERR_SASLTOOLONG | ERR_SASLABORTED
        | ERR_SASLALREADY => {
            need_params(msg, 2)?;
            let info = &msg.params[1];
            let uc = uc!(u, ni);
            match msg.command.as_str() {
                ERR_NICKLOCKED => logf!(
                    uc.logger,
                    "invalid nick used with SASL authentication: {info}"
                ),
                ERR_SASLFAIL => logf!(uc.logger, "SASL authentication failed: {info}"),
                ERR_SASLTOOLONG => logf!(uc.logger, "SASL message too long: {info}"),
                _ => {}
            }
            uc.sasl_client = None;
            uc.sasl_started = false;

            if let Some((dc_id, _)) = dequeue_command(u, ni, "AUTHENTICATE")
                && let Some(i) = u.dc_index(dc_id)
                && u.downstreams[i].sasl.is_some()
            {
                if msg.command == RPL_SASLSUCCESS
                    && let Some(plain) =
                        u.downstreams[i].sasl.as_ref().and_then(|s| s.plain.clone())
                {
                    u.auto_save_sasl_plain(ni, &plain.username, &plain.password);
                }
                u.downstreams[i].end_sasl(Some(msg.clone()));
            }

            let uc = uc!(u, ni);
            if !uc.registered {
                uc.send_message(msg!("CAP", "END"));
            }
        }
        "REGISTER" | "VERIFY" => {
            if let Some((dc_id, cmd)) = dequeue_command(u, ni, &msg.command) {
                if msg.command == "REGISTER" {
                    need_params(msg, 2)?;
                    need_params(&cmd, 3)?;
                    let account = msg.params[1].clone();
                    let password = cmd.params[2].clone();
                    u.auto_save_sasl_plain(ni, &account, &password);
                }
                if let Some(i) = u.dc_index(dc_id) {
                    u.downstreams[i].send_message(msg.clone());
                }
            }
        }
        RPL_WELCOME => {
            need_params(msg, 1)?;
            let uc = uc!(u, ni);
            uc.nick = msg.params[0].clone();
            uc.registered = true;
            uc.server_prefix = msg.prefix.clone().unwrap_or_default();
            logf!(uc.logger, "connection registered with nick {:?}", uc.nick);
        }
        RPL_MYINFO => {
            // Registration info, not forwarded: downstreams get our own
            need_params(msg, 5)?;
        }
        RPL_ISUPPORT => {
            need_params(msg, 2)?;
            let tokens = msg.params[1..msg.params.len() - 1].to_vec();
            for token in tokens {
                let (mut parameter, negate) = match token.strip_prefix('-') {
                    Some(p) => (p.to_owned(), true),
                    None => (token.clone(), false),
                };
                let mut value = String::new();
                let mut has_value = false;
                if !negate {
                    if let Some((p, v)) = token.split_once('=') {
                        parameter = p.to_owned();
                        value = v.to_owned();
                        has_value = true;
                    } else {
                        parameter = token.clone();
                    }
                }
                parameter = match parameter.split_once('/') {
                    Some((prefix, key)) => {
                        format!("{}/{}", prefix.to_lowercase(), key.to_uppercase())
                    }
                    None => parameter.to_uppercase(),
                };
                let value = xirc::decode_isupport_value(&value);

                let uc = uc!(u, ni);
                if has_value {
                    uc.isupport.insert(parameter.clone(), Some(value.clone()));
                } else if !negate {
                    uc.isupport.insert(parameter.clone(), None);
                } else {
                    uc.isupport.remove(&parameter);
                }

                match parameter.as_str() {
                    "CASEMAPPING" => {
                        let casemap =
                            xirc::parse_casemapping(&value).unwrap_or(xirc::CASEMAPPING_RFC1459);
                        u.update_casemapping(ni, casemap);
                    }
                    "CHANMODES" => {
                        if !negate {
                            handle_chan_modes(uc, &value)?;
                        } else {
                            uc.available_channel_modes = ircutil::std_channel_modes();
                        }
                    }
                    "CHANTYPES" => {
                        uc.available_channel_types = if !negate {
                            value.clone()
                        } else {
                            ircutil::STD_CHANNEL_TYPES.to_owned()
                        };
                    }
                    "STATUSMSG" => {
                        uc.available_status_msg = if !negate {
                            value.clone()
                        } else {
                            String::new()
                        };
                    }
                    "PREFIX" => {
                        if !negate {
                            handle_memberships(uc, &value)?;
                        } else {
                            uc.available_memberships = ircutil::std_memberships();
                        }
                    }
                    "soju.im/SAFERATE" => uc.rate_flag.store(negate, Ordering::Relaxed),
                    _ => {}
                }
            }

            update_monitor(u, ni);
            for id in dc_ids_for(&u.downstreams, net_id) {
                downstream::update_isupport(u, id);
            }
        }
        ERR_NOMOTD | RPL_ENDOFMOTD => {
            let uc = uc!(u, ni);
            if !uc.got_motd {
                // Ignore the initial MOTD upon connection, but forward
                // subsequent MOTD messages downstream
                uc.got_motd = true;

                // If upstream did not send any CASEMAPPING token, assume it
                // implements the old RFCs with rfc1459.
                if uc.isupport.get("CASEMAPPING").is_none_or(|v| v.is_none()) {
                    u.update_casemapping(ni, xirc::CASEMAPPING_RFC1459);
                }

                // If the server doesn't support MONITOR, periodically try to
                // regain our desired nick
                if !uc!(u, ni).isupport.contains_key("MONITOR") {
                    start_regain_nick_timer(u, ni);
                }
                return Ok(true);
            }
            forward_msg_by_id(u, net_id, downstream_id, msg);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Whether a CAP LS reply is continued on another line, given its
/// parameters after the subcommand: "CAP <nick> LS * :<caps>".
pub(super) fn cap_ls_continued(sub_params: &[String]) -> bool {
    sub_params.len() >= 2 && sub_params[sub_params.len() - 2] == "*"
}

fn handle_chan_modes(uc: &mut UpstreamConn, s: &str) -> Result<()> {
    let parts: Vec<&str> = s.splitn(5, ',').collect();
    if parts.len() < 4 {
        bail!("malformed ISUPPORT CHANMODES value: {s}");
    }
    let mut modes = IndexMap::new();
    for (i, mt) in [
        ChannelModeType::A,
        ChannelModeType::B,
        ChannelModeType::C,
        ChannelModeType::D,
    ]
    .into_iter()
    .enumerate()
    {
        for m in parts[i].bytes() {
            modes.insert(m, mt);
        }
    }
    uc.available_channel_modes = modes;
    Ok(())
}

fn handle_memberships(uc: &mut UpstreamConn, s: &str) -> Result<()> {
    if s.is_empty() {
        uc.available_memberships = Vec::new();
        return Ok(());
    }
    let b = s.as_bytes();
    // Memberships are single bytes: a multi-byte prefix can't be matched
    if b[0] != b'(' || !s.is_ascii() {
        bail!("malformed ISUPPORT PREFIX value: {s}");
    }
    let Some(sep) = s.find(')') else {
        bail!("malformed ISUPPORT PREFIX value: {s}");
    };
    if b.len() != sep * 2 {
        bail!("malformed ISUPPORT PREFIX value: {s}");
    }
    uc.available_memberships = (0..b.len() / 2 - 1)
        .map(|i| Membership {
            mode: b[i + 1],
            prefix: b[sep + i + 1],
        })
        .collect();
    Ok(())
}

fn request_sasl(u: &User, ni: usize) -> bool {
    let net = &u.networks[ni];
    let mech = &net.record.sasl.mechanism;
    if mech.is_empty() {
        return false;
    }
    net.conn.as_ref().unwrap().supports_sasl(mech)
}

fn handle_cap_ack(u: &mut User, ni: usize, name: &str, ok: bool) -> Result<()> {
    uc!(u, ni).caps.set_enabled(name, ok);
    match name {
        "sasl" => {
            if !request_sasl(u, ni) {
                return Ok(());
            }
            let auth = u.networks[ni].record.sasl.clone();
            let uc = uc!(u, ni);
            if !ok {
                logf!(
                    uc.logger,
                    "server refused to acknowledge the SASL capability"
                );
                return Ok(());
            }
            match auth.mechanism.as_str() {
                "PLAIN" => {
                    logf!(
                        uc.logger,
                        "starting SASL PLAIN authentication with username {:?}",
                        auth.plain_username
                    );
                    uc.sasl_client = Some(SaslClient::Plain {
                        username: auth.plain_username.clone(),
                        password: auth.plain_password.clone(),
                    });
                }
                "EXTERNAL" => {
                    logf!(uc.logger, "starting SASL EXTERNAL authentication");
                    uc.sasl_client = Some(SaslClient::External);
                }
                mech => bail!("unsupported SASL mechanism {mech:?}"),
            }
            uc.send_message(msg!("AUTHENTICATE", auth.mechanism));
        }
        "echo-message" => {}
        _ => {
            if !PERMANENT_UPSTREAM_CAPS.contains(&name) {
                logf!(
                    uc!(u, ni).logger,
                    "received CAP ACK/NAK for a cap we don't support: {name}"
                );
            }
        }
    }
    Ok(())
}
