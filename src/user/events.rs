//! The user's event loop and its event handlers.

use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use super::{Event, Network, User, UserRunMsg, dcs_for};
use crate::database as db;
use crate::downstream::{self, DownstreamConn, send_service_notice};
use crate::irc::Tags;
use crate::{logf, msg};

impl User {
    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Event>) {
        let user_id = self.record.id;
        let networks = match db::run(&self.srv.db, move |db| db.list_networks(user_id)).await {
            Ok(mut n) => {
                n.sort_by_key(|n| n.id);
                n
            }
            Err(e) => {
                logf!(
                    self.logger,
                    "failed to list networks for user {:?}: {e}",
                    self.record.username
                );
                self.close_msg_store();
                return;
            }
        };

        for record in networks {
            let net_id = record.id;
            let channels = match db::run(&self.srv.db, move |db| db.list_channels(net_id)).await {
                Ok(c) => c,
                Err(e) => {
                    logf!(
                        self.logger,
                        "failed to list channels for user {:?}, network {:?}: {e}",
                        self.record.username,
                        record.get_name()
                    );
                    continue;
                }
            };
            let mut network = Network::new(&self.logger, record, channels);
            if self.has_persistent_msg_store() {
                let receipts =
                    db::run(&self.srv.db, move |db| db.list_delivery_receipts(net_id)).await;
                match receipts {
                    Ok(receipts) => {
                        for rcpt in receipts {
                            network.delivered.store_id(
                                &rcpt.target,
                                &rcpt.client,
                                &rcpt.internal_msg_id,
                            );
                        }
                    }
                    Err(e) => {
                        logf!(
                            self.logger,
                            "failed to load delivery receipts for user {:?}, network {:?}: {e}",
                            self.record.username,
                            network.get_name()
                        );
                        self.close_msg_store();
                        return;
                    }
                }
            }
            self.networks.push(network);
            let i = self.networks.len() - 1;
            self.start_network(i);
        }

        while let Some(ev) = rx.recv().await {
            // Event handlers are synchronous and make blocking SQLite calls
            // whose results they need
            crate::server::blocking(|| self.handle_event(ev));
            if self.stopped {
                break;
            }
        }
        self.close_msg_store();
    }

    fn close_msg_store(&mut self) {
        if let Err(e) = self.msg_store.close() {
            logf!(
                self.logger,
                "failed to close message store for user {:?}: {e}",
                self.record.username
            );
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::UpstreamConnected {
                net_id,
                runner_id,
                conn,
            } => {
                let Some(ni) = self.net_index(net_id) else {
                    conn.conn.close();
                    return;
                };
                if self.networks[ni].runner_id != runner_id || self.networks[ni].is_stopped() {
                    conn.conn.close();
                    return;
                }
                self.upstream_connected(ni, *conn);
            }
            Event::UpstreamMessage {
                net_id,
                conn_id,
                msg,
                permit: _permit,
            } => {
                self.upstream_message(net_id, conn_id, msg);
            }
            Event::UpstreamClosed {
                net_id,
                conn_id,
                read_error,
            } => self.upstream_closed(net_id, conn_id, read_error),
            Event::UpstreamConnectionError {
                net_id,
                runner_id,
                err,
            } => {
                let Some(ni) = self.net_index(net_id) else {
                    return;
                };
                let net = &mut self.networks[ni];
                if net.runner_id != runner_id {
                    return;
                }
                let stopped = net.is_stopped();
                if !stopped && net.last_error.as_deref() != Some(err.as_str()) {
                    let text =
                        format!("failed connecting/registering to {}: {err}", net.get_name());
                    for dc in dcs_for(&mut self.downstreams, net_id) {
                        send_service_notice(dc, &text);
                    }
                }
                self.networks[ni].last_error = Some(err.clone());
                let attrs = Tags::from_iter([("error", err)]);
                self.notify_bouncer_network_state(net_id, &attrs);
            }
            Event::UpstreamRegistrationDelayDone { net_id, conn_id } => {
                let Some(ni) = self.net_index(net_id) else {
                    return;
                };
                if self.networks[ni].conn.as_ref().map(|c| c.id) != Some(conn_id) {
                    return;
                }
                self.attach_upstream(ni);
            }
            Event::ChannelDetach {
                net_id,
                conn_id,
                name,
            } => {
                let Some(ni) = self.net_index(net_id) else {
                    return;
                };
                // The timer belongs to a previous connection
                if self.networks[ni].conn.as_ref().map(|c| c.id) != Some(conn_id) {
                    return;
                }
                if !crate::upstream::auto_detach_timer_fired(self, ni, &name) {
                    return;
                }
                let Some(c) = self.networks[ni].channels.get(&name) else {
                    return;
                };
                if c.detached {
                    return;
                }
                self.detach_channel(ni, &name);
                self.store_channel(ni, &name);
            }
            Event::DownstreamConnected(dc) => self.downstream_connected(*dc),
            Event::DownstreamDisconnected { dc_id } => self.remove_downstream(dc_id),
            Event::DownstreamMessage {
                dc_id,
                msg,
                permit: _permit,
            } => {
                let Some((dc_id, msg)) = downstream::route_message(self, dc_id, msg) else {
                    return;
                };
                let Some(i) = self.dc_index(dc_id) else {
                    return;
                };
                if self.downstreams[i].conn.is_closed() {
                    logf!(
                        self.downstreams[i].logger,
                        "ignoring message on closed connection: {msg}"
                    );
                    return;
                }
                if let Err(e) = downstream::handle_message(self, dc_id, &msg)
                    && let Some(i) = self.dc_index(dc_id)
                {
                    logf!(
                        self.downstreams[i].logger,
                        "failed to handle message {:?}: {e}",
                        crate::ircutil::redact_for_log(&msg)
                    );
                    self.downstreams[i].conn.close();
                }
            }
            Event::Broadcast(msg) => {
                for dc in &mut self.downstreams {
                    dc.send_message(msg.clone());
                }
            }
            Event::UserUpdate {
                password,
                admin,
                enabled,
                max_networks,
                done,
            } => {
                let password_updated = password.is_some();
                let res = self.update_user(|record| {
                    if let Some(p) = password {
                        record.password = p;
                    }
                    if let Some(a) = admin {
                        record.admin = a;
                    }
                    if let Some(e) = enabled {
                        record.enabled = e;
                    }
                    if let Some(m) = max_networks {
                        record.max_networks = m;
                    }
                    Ok(())
                });
                let _ = done.send(res.map_err(|e| e.to_string()));

                // If the password was updated, kill all downstream
                // connections to force them to re-authenticate with the new
                // credentials.
                if password_updated {
                    for dc in &self.downstreams {
                        dc.conn.close();
                    }
                }
            }
            Event::TryRegainNick {
                net_id,
                conn_id,
                nick,
            } => {
                let Some(ni) = self.net_index(net_id) else {
                    return;
                };
                if self.networks[ni].conn.as_ref().map(|c| c.id) != Some(conn_id) {
                    return;
                }
                self.try_regain_nick(ni, &nick);
            }
            Event::UserRun { params, tx } => {
                // user run can only be run by an admin, so run with admin
                // rights. This enables admins to e.g. add a network past the
                // user limit.
                let mut ctx =
                    crate::service::ServiceContext::for_user(self.srv.clone(), true, true);
                let res = crate::service::handle_service_command(self, &mut ctx, &params);
                for line in ctx.take_prints() {
                    let _ = tx.send(UserRunMsg::Message(line));
                }
                match (res, ctx.take_deferred()) {
                    (Ok(()), Some(deferred)) => {
                        tokio::spawn(async move {
                            let print_tx = tx.clone();
                            let print = Box::new(move |line| {
                                let _ = print_tx.send(UserRunMsg::Message(line));
                            });
                            let res = deferred(print).await;
                            let _ = tx.send(UserRunMsg::Done(res.map_err(|e| e.to_string())));
                        });
                    }
                    (res, _) => {
                        let _ = tx.send(UserRunMsg::Done(res.map_err(|e| e.to_string())));
                    }
                }
            }
            Event::ServiceReply { dc_id, text } => {
                if let Some(i) = self.dc_index(dc_id) {
                    downstream::send_service_privmsg(&mut self.downstreams[i], &text);
                }
            }
            Event::HasNetwork { name, reply } => {
                let _ = reply.send(self.get_network(&name).is_some());
            }
            Event::WebPushTested {
                dc_id,
                label,
                network_id,
                sub,
                res,
                check,
            } => {
                crate::downstream::web_push_tested(self, dc_id, label, network_id, sub, res, check)
            }
            Event::Run(f) => f(self),
            Event::Stop => {
                for dc in &self.downstreams {
                    dc.conn.close();
                }
                for ni in 0..self.networks.len() {
                    self.networks[ni].stop.cancel();
                    if let Some(uc) = &self.networks[ni].conn {
                        uc.conn.close();
                        uc.stop_timers();
                    }
                    for client in self.networks[ni].delivered.clients() {
                        self.store_client_delivery_receipts(ni, &client);
                    }
                }
                self.stopped = true;
            }
        }
    }

    /// Removes a downstream connection, once closed. The virtual
    /// connections multiplexed on it go with it.
    pub fn remove_downstream(&mut self, dc_id: u64) {
        let Some(i) = self.dc_index(dc_id) else {
            return;
        };
        let dc = self.downstreams.remove(i);
        self.num_downstream_conns.fetch_sub(1, Ordering::SeqCst);

        if let Some(net_id) = dc.network_id
            && let Some(ni) = self.net_index(net_id)
        {
            self.store_client_delivery_receipts(ni, &dc.client_name);
        }

        for ni in 0..self.networks.len() {
            if let Some(uc) = self.networks[ni].upstream_mut() {
                uc.cancel_pending_commands_by_downstream_id(dc_id);
            }
            self.update_away(ni);
            self.update_monitor(ni);
        }

        if !dc.impersonating {
            self.bump_downstream_interaction_time();
        }

        let children: Vec<u64> = self
            .downstreams
            .iter()
            .filter(|d| d.parent_id == Some(dc_id))
            .map(|d| d.id)
            .collect();
        for id in children {
            self.remove_downstream(id);
        }
    }

    pub(crate) fn downstream_connected(&mut self, mut dc: DownstreamConn) {
        if !self.record.enabled
            && self.srv.config().basic.enable_users_on_auth
            && let Err(e) = self.update_user(|record| {
                record.enabled = true;
                Ok(())
            })
        {
            logf!(
                dc.logger,
                "failed to enable user after successful authentication: {e}"
            );
        }

        if !self.record.enabled {
            dc.send_message(msg!("ERROR", "This bouncer account is disabled"));
            dc.conn.shutdown();
            return;
        }

        let dc_id = dc.id;
        self.downstreams.push(dc);
        if let Err(e) = downstream::welcome(self, dc_id) {
            let Some(i) = self.dc_index(dc_id) else {
                return;
            };
            let mut dc = self.downstreams.remove(i);
            match e.downcast_ref::<downstream::IrcError>() {
                Some(irc_err) => {
                    let mut m = irc_err.0.clone();
                    m.prefix = Some(self.srv.prefix());
                    dc.send_message(m);
                }
                None => dc.send_message(msg!("ERROR", "Internal server error")),
            }
            logf!(dc.logger, "failed to handle new registered connection: {e}");
            dc.conn.shutdown();
            return;
        }

        self.num_downstream_conns.fetch_add(1, Ordering::SeqCst);

        let i = self.dc_index(dc_id).unwrap();
        if let Some(net_id) = self.downstreams[i].network_id
            && let Some(ni) = self.net_index(net_id)
            && let Some(err) = self.networks[ni].last_error.clone()
        {
            let text = format!("disconnected from {}: {err}", self.networks[ni].get_name());
            send_service_notice(&mut self.downstreams[i], &text);
        }

        for ni in 0..self.networks.len() {
            self.update_away(ni);
        }

        if !self.downstreams[i].impersonating {
            self.bump_downstream_interaction_time();
        }
    }
}
