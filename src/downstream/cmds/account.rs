//! AUTHENTICATE and CLIENTCERT from registered downstream connections.

use super::*;

pub(super) fn handle_authenticate_registered(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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
                dc!(u, i).end_sasl(Some(msg!(
                    ERR_SASLFAIL,
                    nick,
                    "SASL PLAIN identity not supported"
                )));
                return Ok(());
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

pub(super) fn handle_clientcert(u: &mut User, i: usize, msg: &Message) -> Result<()> {
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
