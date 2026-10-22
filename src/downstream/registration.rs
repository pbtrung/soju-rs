//! Downstream connection registration: authentication and network
//! selection.

use super::*;

/// Reads and handles messages until the connection is registered. Returns
/// an error (or Ok(false) when the client quit) if registration failed.
pub async fn run_until_registered(
    dc: &mut DownstreamConn,
    reader: &mut ConnReader,
) -> Result<bool> {
    loop {
        let msg = match reader.read_message().await {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(false),
            Err(e) => bail!("failed to read IRC command: {e}"),
        };
        metrics::inc(&dc.srv.metrics.downstream_in_messages_total);

        let msg_str = ircutil::redact_for_log(&msg);
        if let Err(e) = handle_message_unregistered(dc, msg).await {
            bail!("failed to handle IRC command {msg_str:?}: {e}");
        }
        if dc.conn.is_closed() {
            return Ok(false);
        }

        let r = dc.registration.as_ref().unwrap();
        if r.nick.is_empty() || r.username.is_empty() || r.negotiating_caps {
            continue;
        }

        match register(dc).await {
            Ok(()) => return Ok(true),
            Err(e) => match e.downcast::<IrcError>() {
                Ok(irc_err) => {
                    let mut m = irc_err.0;
                    m.prefix = Some(dc.srv.prefix());
                    dc.send_message(m);
                    return Ok(false);
                }
                Err(e) => bail!("connection registration failed: {e}"),
            },
        }
    }
}

async fn handle_message_unregistered(dc: &mut DownstreamConn, msg: Message) -> Result<()> {
    begin_message(dc, &msg);
    let res = match msg.command.as_str() {
        "QUIT" => {
            dc.conn.shutdown();
            Ok(())
        }
        "ISUPPORT" => {
            if !dc.caps.is_enabled("draft/extended-isupport") {
                Err(unknown_command(&msg.command))
            } else {
                let ctx = IsupportCtx {
                    has_user: false,
                    network_id: None,
                    chathistory: false,
                    upstream: None,
                };
                let isupport = build_isupport(dc, &ctx);
                send_isupport(dc, &isupport);
                dc.isupport = isupport;
                Ok(())
            }
        }
        _ => handle_unregistered_command(dc, &msg).await,
    };
    finish_message(dc, res)
}

async fn handle_unregistered_command(dc: &mut DownstreamConn, msg: &Message) -> Result<()> {
    match msg.command.as_str() {
        "NICK" => {
            need_params(msg, 1)?;
            dc.registration.as_mut().unwrap().nick = msg.params[0].clone();
        }
        "USER" => {
            need_params(msg, 4)?;
            dc.registration.as_mut().unwrap().username = msg.params[0].clone();
        }
        "PASS" => {
            need_params(msg, 1)?;
            dc.registration.as_mut().unwrap().pass = msg.params[0].clone();
        }
        "CAP" => return dc.handle_cap(msg),
        "AUTHENTICATE" => {
            let Some(credentials) = dc.handle_authenticate(msg)? else {
                return Ok(());
            };
            match authenticate(dc, &credentials).await {
                Err(e) => {
                    logf!(
                        dc.logger,
                        "SASL {} authentication error for nick {:?}: {e}",
                        credentials.mechanism,
                        dc.registration.as_ref().unwrap().nick
                    );
                    let nick = dc.nick.clone();
                    dc.end_sasl(Some(msg!(
                        ERR_SASLFAIL,
                        nick,
                        crate::auth::auth_error_reason(&e)
                    )));
                }
                Ok((username, client, network, impersonating)) => {
                    assert!(
                        !username.is_empty(),
                        "username unset after SASL authentication"
                    );
                    dc.set_auth_username(&username, &client, &network);
                    dc.impersonating = impersonating;
                    // Technically we should send RPL_LOGGEDIN here. However
                    // we use RPL_LOGGEDIN to mirror the upstream connection
                    // status. See:
                    // https://github.com/ircv3/ircv3-specifications/pull/476
                    dc.end_sasl(None);
                }
            }
        }
        "BOUNCER" => {
            need_params(msg, 1)?;
            let subcommand = msg.params[0].clone();
            match subcommand.to_uppercase().as_str() {
                "BIND" => {
                    need_params(msg, 2)?;
                    if dc.registration.as_ref().unwrap().auth_username.is_empty() {
                        return Err(irc_error(msg!(
                            "FAIL",
                            "BOUNCER",
                            "ACCOUNT_REQUIRED",
                            "BIND",
                            "Authentication needed to bind to bouncer network"
                        )));
                    }
                    let id = parse_bouncer_net_id(&subcommand, &msg.params[1])?;
                    dc.registration.as_mut().unwrap().network_id = id;
                }
                _ => {
                    return Err(irc_error(msg!(
                        "FAIL",
                        "BOUNCER",
                        "UNKNOWN_COMMAND",
                        subcommand,
                        "Unknown subcommand"
                    )));
                }
            }
        }
        "AWAY" => {
            dc.away = msg.params.first().cloned();
            let reply = ircutil::generate_away_reply(dc.away.is_some());
            dc.send_message(reply);
        }
        "METADATA" => {
            need_params(msg, 2)?;
            let subcommand = msg.params[1].clone();
            if let Some(res) = handle_metadata_sub(dc, msg, None) {
                return res;
            }
            return Err(irc_error(msg!(
                "FAIL",
                "METADATA",
                "INVALID_PARAMS",
                subcommand,
                "Unknown command"
            )));
        }
        _ => {
            debugf!(dc.logger, "unhandled message: {msg}");
            return Err(unknown_command(&msg.command));
        }
    }
    Ok(())
}

/// Checks SASL credentials. Returns the username, client name, network name
/// and whether the user is impersonated.
async fn authenticate(
    dc: &mut DownstreamConn,
    credentials: &DownstreamSasl,
) -> Result<(String, String, String, bool)> {
    let srv = dc.srv.clone();
    let cfg = srv.config();
    let remote_addr = dc.info.remote_addr.clone();
    match credentials.mechanism.as_str() {
        "PLAIN" => {
            let plain = credentials.plain.as_ref().unwrap();
            let (mut username, mut client, mut network) = unmarshal_username(&plain.username);
            let Some(auth) = &cfg.auth.plain else {
                bail!("SASL PLAIN not supported");
            };
            auth.auth_plain(&srv.db, &username, &plain.password, &remote_addr)
                .await
                .map_err(|e| wrap_auth_err(e, &username))?;
            let mut impersonating = false;
            if !plain.identity.is_empty() && plain.identity != plain.username {
                let name = username.clone();
                let u = db::run(&srv.db, move |db| db.get_user(&name))
                    .await?
                    .ok_or_else(|| anyhow!("sql: no rows in result set (username {username:?})"))?;
                if !u.admin {
                    bail!("SASL impersonation only allowed for admin users");
                }
                (username, client, network) = unmarshal_username(&plain.identity);
                let name = username.clone();
                if db::run(&srv.db, move |db| db.get_user(&name))
                    .await?
                    .is_none()
                {
                    bail!("sql: no rows in result set (username {username:?})");
                }
                impersonating = true;
            }
            Ok((username, client, network, impersonating))
        }
        "OAUTHBEARER" => {
            let Some(auth) = &cfg.auth.oauth_bearer else {
                bail!("SASL OAUTHBEARER not supported");
            };
            let opts = credentials.oauth_bearer.clone().unwrap_or_default();
            let username = auth.auth_oauth_bearer(&opts.token, &remote_addr).await?;
            if !opts.username.is_empty() && opts.username != username {
                bail!(
                    "username mismatch (client provided {:?}, but server returned {username:?})",
                    opts.username
                );
            }
            Ok((username, String::new(), String::new(), false))
        }
        "EXTERNAL" => {
            if !cfg.basic.client_cert_auth {
                bail!("SASL EXTERNAL not supported");
            }
            let Some(cert) = &dc.info.peer_cert else {
                bail!("TLS client certificate not sent");
            };
            let h = Sha512::digest(cert);
            let Some((user_id, mut device)) =
                db::run(&srv.db, move |db| db.get_device_certificate(&h)).await?
            else {
                bail!("Unknown TLS client certificate");
            };
            let username = db::run(&srv.db, move |db| db.get_username_by_id(user_id))
                .await?
                .ok_or_else(|| anyhow!("sql: no rows in result set"))?;
            device.mark_used(dc.info.remote_ip.map(|ip| std::net::SocketAddr::new(ip, 0)));
            db::run(&srv.db, move |db| {
                db.store_device_certificate(user_id, &mut device)
            })
            .await?;
            Ok((username, String::new(), String::new(), false))
        }
        _ => bail!("unsupported SASL mechanism"),
    }
}

/// Adds the username to an authentication error. Like upstream (which
/// formats the error with %v), this drops the user-facing message, so the
/// client gets a generic "Authentication failed".
fn wrap_auth_err(e: anyhow::Error, username: &str) -> anyhow::Error {
    anyhow!("{e} (username {username:?})")
}

async fn register(dc: &mut DownstreamConn) -> Result<()> {
    assert!(!dc.registered, "tried to register twice");

    if dc.sasl.is_some() {
        let nick = dc.nick.clone();
        dc.end_sasl(Some(msg!(
            ERR_SASLABORTED,
            nick,
            "SASL authentication aborted"
        )));
    }

    let srv = dc.srv.clone();
    let r = dc.registration.as_mut().unwrap();
    let password = std::mem::take(&mut r.pass);
    let reg_username = r.username.clone();
    if r.auth_username.is_empty() {
        let nick = dc.nick.clone();
        if password.is_empty() {
            if dc.caps.is_enabled("sasl") {
                return Err(irc_error(msg!(
                    "FAIL",
                    "*",
                    "ACCOUNT_REQUIRED",
                    "Authentication required"
                )));
            } else {
                return Err(irc_error(msg!(
                    ERR_PASSWDMISMATCH,
                    nick,
                    "Authentication required"
                )));
            }
        }
        let cfg = srv.config();
        let Some(plain_auth) = &cfg.auth.plain else {
            return Err(irc_error(msg!(
                ERR_PASSWDMISMATCH,
                nick,
                "PASS authentication disabled"
            )));
        };
        let (username, client, network) = unmarshal_username(&reg_username);
        if let Err(e) = plain_auth
            .auth_plain(&srv.db, &username, &password, &dc.info.remote_addr)
            .await
        {
            logf!(
                dc.logger,
                "PASS authentication error for user {reg_username:?}: {e}"
            );
            return Err(irc_error(msg!(
                ERR_PASSWDMISMATCH,
                nick,
                crate::auth::auth_error_reason(&e)
            )));
        }
        dc.set_auth_username(&username, &client, &network);
    }

    let (_, fallback_client, fallback_network) = unmarshal_username(&reg_username);
    if dc.client_name.is_empty() {
        dc.client_name = fallback_client;
    } else if !fallback_client.is_empty() && dc.client_name != fallback_client {
        return Err(irc_error(msg!(
            ERR_ERRONEUSNICKNAME,
            dc.nick.clone(),
            "Client name mismatch in usernames"
        )));
    }

    let r = dc.registration.as_mut().unwrap();
    if r.network_name.is_empty() {
        r.network_name = fallback_network;
    } else if !fallback_network.is_empty() && r.network_name != fallback_network {
        return Err(irc_error(msg!(
            ERR_ERRONEUSNICKNAME,
            dc.nick.clone(),
            "Network name mismatch in usernames"
        )));
    }

    dc.registered = true;
    dc.username = r.auth_username.clone();
    logf!(
        dc.logger,
        "registration complete for user {:?}",
        dc.username
    );
    Ok(())
}

/// Checks that a TLS server is reachable, before auto-creating a network.
async fn sanity_check_server(addr: &str) -> Result<()> {
    let host = crate::conn::host_of(addr);
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| anyhow!("i/o timeout"))??;
    let cfg = crate::tls::client_config("", None)?;
    let name = rustls::pki_types::ServerName::try_from(host)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio_rustls::TlsConnector::from(cfg).connect(name, tcp),
    )
    .await
    .map_err(|_| anyhow!("i/o timeout"))??;
    Ok(())
}

/// Checks that the server of a new network requested by a connection is
/// reachable, before load_network auto-creates the network. This runs
/// before the connection is handed to its user, as it may take a while:
/// the user's event loop must not wait on it.
pub(super) async fn check_new_network(
    user: &crate::user::UserHandle,
    dc: &mut DownstreamConn,
) -> Result<()> {
    let r = dc.registration.as_mut().unwrap();
    if r.network_id != 0 || r.network_name.is_empty() || r.network_name == "*" {
        return Ok(());
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let name = r.network_name.clone();
    if !user.send(crate::user::Event::HasNetwork { name, reply: tx }) {
        return Ok(());
    }
    if rx.await.unwrap_or(true) {
        return Ok(());
    }
    let mut addr = r.network_name.clone();
    if !addr.contains(':') {
        addr.push_str(":6697");
    }
    logf!(dc.logger, "trying to connect to new network {addr:?}");
    if let Err(e) = sanity_check_server(&addr).await {
        logf!(dc.logger, "failed to connect to {addr:?}: {e}");
        return Err(irc_error(msg!(
            ERR_PASSWDMISMATCH,
            dc.nick.clone(),
            format!("Failed to connect to {:?}", r.network_name)
        )));
    }
    r.server_checked = true;
    Ok(())
}

pub(super) fn load_network(u: &mut User, dc_id: u64) -> Result<()> {
    let i = u.dc_index(dc_id).unwrap();
    let dc = &u.downstreams[i];
    let r = dc.registration.clone().unwrap();
    let nick = dc.nick.clone();
    if r.network_id != 0 {
        if u.net_index(r.network_id).is_none() {
            return Err(irc_error(msg!(
                "FAIL",
                "BOUNCER",
                "INVALID_NETID",
                r.network_id.to_string(),
                "Unknown network ID"
            )));
        }
        u.downstreams[i].network_id = Some(r.network_id);
        return Ok(());
    }

    if r.network_name == "*" {
        return Err(irc_error(msg!(
            ERR_PASSWDMISMATCH,
            nick,
            "Multi-upstream mode is no longer supported"
        )));
    }
    if r.network_name.is_empty() {
        return Ok(());
    }

    let ni = match u.get_network(&r.network_name) {
        Some(ni) => ni,
        None => {
            // The network was deleted since check_new_network
            if !r.server_checked {
                return Err(irc_error(msg!(
                    ERR_PASSWDMISMATCH,
                    nick,
                    format!("Failed to connect to {:?}", r.network_name)
                )));
            }

            // Some clients only allow specifying the nickname (and use the
            // nickname as a username too). Strip the network name from the
            // nickname when auto-saving networks.
            let (net_nick, _, _) = unmarshal_username(&r.nick);
            if net_nick.is_empty() || net_nick.contains(ILLEGAL_NICK_CHARS) {
                return Err(irc_error(msg!(
                    ERR_ERRONEUSNICKNAME,
                    nick,
                    r.nick.clone(),
                    "Nickname contains illegal characters"
                )));
            }
            if xirc::casemap_ascii(&net_nick) == SERVICE_NICK_CM {
                return Err(irc_error(msg!(
                    ERR_NICKNAMEINUSE,
                    nick,
                    r.nick.clone(),
                    "Nickname reserved for bouncer service"
                )));
            }

            let mut record = db::Network::new(&r.network_name);
            record.nick = net_nick;
            logf!(
                u.downstreams[i].logger,
                "auto-saving network {:?}",
                r.network_name
            );
            u.create_network(record, true)?
        }
    };
    let net_id = u.networks[ni].id();
    let i = u.dc_index(dc_id).unwrap();
    u.downstreams[i].network_id = Some(net_id);
    Ok(())
}
