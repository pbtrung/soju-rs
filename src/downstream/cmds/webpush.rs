//! WEBPUSH (soju.im/webpush) from registered downstream connections.

use super::*;

/// Maximum number of Web Push subscriptions per user and network.
const MAX_SUBSCRIPTIONS: usize = 25;

/// Maximum number of test notifications being sent at once per user.
const MAX_PENDING_CHECKS: usize = 4;

/// Finishes WEBPUSH REGISTER, once the test notification was sent.
pub fn web_push_tested(
    u: &mut User,
    dc_id: u64,
    label: String,
    network_id: i64,
    mut sub: db::WebPushSubscription,
    res: Result<(), String>,
    check: u64,
) {
    let key = (network_id, sub.endpoint.clone());
    // A later REGISTER of the same endpoint is being checked: it stores the
    // subscription
    let superseded = u.web_push_checks.get(&key).is_some_and(|&c| c != check);
    // UNREGISTER was received meanwhile
    let cancelled = !superseded && u.web_push_checks.remove(&key).is_none();
    let Some(i) = u.dc_index(dc_id) else {
        return;
    };
    if !label.is_empty() {
        dc!(u, i).label = Some(LabelContext {
            label,
            ..Default::default()
        });
    }
    let user_id = u.record.id;
    let res = (|| {
        if let Err(e) = res {
            logf!(
                dc!(u, i).logger,
                "failed to send Web push notification to endpoint {:?}: {e}",
                sub.endpoint
            );
            return Err(fail(&[
                "WEBPUSH",
                "INVALID_PARAMS",
                "REGISTER",
                "Invalid endpoint",
            ]));
        }
        if cancelled {
            return Err(fail(&[
                "WEBPUSH",
                "INTERNAL_ERROR",
                "REGISTER",
                "Registration cancelled",
            ]));
        }
        if superseded {
            dc!(u, i).send_message(msg!("WEBPUSH", "REGISTER", sub.endpoint.clone()));
            return Ok(());
        }
        // The subscriptions may have changed while the notification was
        // being sent
        let subs = match u.srv.db.list_web_push_subscriptions(user_id, network_id) {
            Ok(s) => s,
            Err(e) => {
                logf!(
                    dc!(u, i).logger,
                    "failed to fetch Web push subscription: {e}"
                );
                return Err(internal_error("WEBPUSH", "REGISTER"));
            }
        };
        match subs.iter().find(|s| s.endpoint == sub.endpoint) {
            Some(old) => sub.id = old.id,
            None if subs.len() >= MAX_SUBSCRIPTIONS => return Err(too_many_subscriptions()),
            None => sub.id = 0,
        }
        if let Err(e) = u
            .srv
            .db
            .store_web_push_subscription(user_id, network_id, &mut sub)
        {
            logf!(
                dc!(u, i).logger,
                "failed to store Web push subscription: {e}"
            );
            return Err(fail(&[
                "WEBPUSH",
                "INTERNAL_ERROR",
                "REGISTER",
                "Internal error",
            ]));
        }
        u.reset_web_push_networks();
        dc!(u, i).send_message(msg!("WEBPUSH", "REGISTER", sub.endpoint.clone()));
        Ok(())
    })();
    if let Err(e) = finish_message(&mut u.downstreams[i], res) {
        logf!(dc!(u, i).logger, "failed to handle WEBPUSH REGISTER: {e}");
    }
}

pub(super) fn handle_webpush(u: &mut User, i: usize, msg: &Message) -> Result<()> {
    if !dc!(u, i).caps.is_enabled("soju.im/webpush") {
        return Err(unknown_command(&msg.command));
    }
    need_params(msg, 1)?;
    let subcommand = msg.params[0].clone();
    let user_id = u.record.id;
    let network_id = dc!(u, i).network_id.unwrap_or(0);
    let Some(sender) = u.srv.web_push() else {
        return Err(internal_error("WEBPUSH", &subcommand));
    };

    match subcommand.as_str() {
        "REGISTER" => {
            need_params(msg, 3)?;
            let endpoint = msg.params[1].clone();
            let raw_keys = Tags::parse(&msg.params[2]);
            let (Some(auth), Some(p256dh)) = (raw_keys.get("auth"), raw_keys.get("p256dh")) else {
                return Err(fail(&[
                    "WEBPUSH",
                    "INVALID_PARAMS",
                    &subcommand,
                    "Missing auth or p256dh key",
                ]));
            };
            let mut new_sub = db::WebPushSubscription {
                endpoint: endpoint.clone(),
                keys: db::WebPushKeys {
                    auth: auth.to_owned(),
                    p256dh: p256dh.to_owned(),
                    vapid: sender.vapid_public.clone(),
                },
                ..Default::default()
            };

            let subs = match u.srv.db.list_web_push_subscriptions(user_id, network_id) {
                Ok(s) => s,
                Err(e) => {
                    logf!(
                        dc!(u, i).logger,
                        "failed to fetch Web push subscription: {e}"
                    );
                    return Err(internal_error("WEBPUSH", &subcommand));
                }
            };
            let old = subs.iter().find(|s| s.endpoint == endpoint);
            // Refreshing an existing subscription is always allowed
            if old.is_none() {
                let pending_new = u
                    .web_push_checks
                    .keys()
                    .filter(|(n, e)| *n == network_id && !subs.iter().any(|s| s.endpoint == *e))
                    .count();
                if subs.len() + pending_new >= MAX_SUBSCRIPTIONS {
                    return Err(too_many_subscriptions());
                }
            }

            let mut update_sub = true;
            if let Some(old) = old {
                // Update the old subscription instead of creating a new one
                new_sub.id = old.id;
                // Rate-limit subscription checks
                let stale = old
                    .updated_at
                    .is_none_or(|t| Utc::now() - t > chrono::Duration::hours(24));
                update_sub = stale || old.keys != new_sub.keys;
            }

            // Send a test Web Push message, to make sure the endpoint is
            // valid
            if update_sub {
                if let Err(e) = sanity_check_web_push_endpoint(&new_sub.endpoint) {
                    logf!(
                        dc!(u, i).logger,
                        "failed to sanity check Web push endpoint {:?}: {e}",
                        new_sub.endpoint
                    );
                    return Err(fail(&[
                        "WEBPUSH",
                        "INVALID_PARAMS",
                        &subcommand,
                        "Invalid endpoint",
                    ]));
                }
                let key = (network_id, endpoint.clone());
                if !u.web_push_checks.contains_key(&key)
                    && u.web_push_checks.len() >= MAX_PENDING_CHECKS
                {
                    return Err(fail(&[
                        "WEBPUSH",
                        "INTERNAL_ERROR",
                        &subcommand,
                        "Too many pending subscriptions, try again later",
                    ]));
                }
                let check = u.next_web_push_check;
                u.next_web_push_check += 1;
                u.web_push_checks.insert(key, check);
                // The test notification is sent in the background: the reply
                // comes with Event::WebPushTested
                let dc_id = dc!(u, i).id;
                let label = dc!(u, i).take_label();
                let tx = u.tx.clone();
                tokio::spawn(async move {
                    let test = msg!(
                        "NOTE",
                        "WEBPUSH",
                        "REGISTERED",
                        "Push notifications enabled"
                    );
                    let res = crate::user::send_web_push(&sender, &new_sub, &test)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Event::WebPushTested {
                        dc_id,
                        label,
                        network_id,
                        sub: new_sub,
                        res,
                        check,
                    });
                });
                return Ok(());
            }
            dc!(u, i).send_message(msg!("WEBPUSH", "REGISTER", endpoint));
        }
        "UNREGISTER" => {
            need_params(msg, 2)?;
            let endpoint = msg.params[1].clone();
            u.web_push_checks.remove(&(network_id, endpoint.clone()));
            let subs = match u.srv.db.list_web_push_subscriptions(user_id, network_id) {
                Ok(s) => s,
                Err(e) => {
                    logf!(
                        dc!(u, i).logger,
                        "failed to fetch Web push subscription: {e}"
                    );
                    return Err(internal_error("WEBPUSH", &subcommand));
                }
            };
            u.reset_web_push_networks();
            if let Some(old) = subs.iter().find(|s| s.endpoint == endpoint)
                && let Err(e) = u.srv.db.delete_web_push_subscription(old.id)
            {
                logf!(
                    dc!(u, i).logger,
                    "failed to delete Web push subscription: {e}"
                );
                return Err(internal_error("WEBPUSH", &subcommand));
            }
            dc!(u, i).send_message(msg!("WEBPUSH", "UNREGISTER", endpoint));
        }
        _ => {
            return Err(fail(&[
                "WEBPUSH",
                "INVALID_PARAMS",
                &subcommand,
                "Unknown command",
            ]));
        }
    }
    Ok(())
}

fn too_many_subscriptions() -> anyhow::Error {
    fail(&[
        "WEBPUSH",
        "INTERNAL_ERROR",
        "REGISTER",
        "Too many subscriptions",
    ])
}

fn sanity_check_web_push_endpoint(endpoint: &str) -> Result<()> {
    let u = db::parse_url(endpoint)?;
    if u.scheme != "https" {
        anyhow::bail!("scheme must be HTTPS");
    }
    Ok(())
}
