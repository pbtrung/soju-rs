//! The user's event loop and its event handlers.

use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use super::{Event, Network, User, UserRunMsg, dcs_for};
use crate::database as db;
use crate::downstream::{self, DownstreamConn, send_service_notice};
use crate::irc::Tags;
use crate::{logf, msg};

/// Retry delays of a user's loading, doubling from the first.
const STARTUP_RETRY_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const STARTUP_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

impl User {
    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Event>) {
        // A transient database error (e.g. a Turso timeout) must not leave
        // the user unavailable, nor drop one of their networks, until
        // restart: retry until everything is loaded. Events wait meanwhile,
        // except a stop.
        let mut delay = STARTUP_RETRY_MIN;
        let mut early = std::collections::VecDeque::new();
        let stopped = self.srv.stopped();
        let networks = loop {
            let load = Self::load_networks(
                self.srv.clone(),
                self.logger.clone(),
                self.record.id,
                self.has_persistent_msg_store(),
            );
            let loaded = if self.srv.db.is_remote() {
                let (user, prepared) = tokio::task::spawn_blocking(move || {
                    let prepared = self.msg_store.prepare();
                    (self, prepared)
                })
                .await
                .expect("message store initialization task failed");
                self = user;
                match prepared {
                    Ok(()) => load.await,
                    Err(e) => Err(e.context("failed to prepare Turso message buffers")),
                }
            } else {
                load.await
            };
            match loaded {
                Ok(networks) => break networks,
                Err(e) => logf!(self.logger, "failed to load, retrying in {delay:?}: {e:#}"),
            }
            let retry = tokio::time::sleep(delay);
            tokio::pin!(retry);
            loop {
                tokio::select! {
                    _ = &mut retry => break,
                    _ = stopped.cancelled() => {
                        self.close_msg_store();
                        return;
                    }
                    ev = rx.recv() => match ev {
                        None | Some(Event::Stop) => {
                            self.close_msg_store();
                            return;
                        }
                        Some(ev) => early.push_back(ev),
                    },
                }
            }
            delay = (delay * 2).min(STARTUP_RETRY_MAX);
        };
        for network in networks {
            self.networks.push(network);
            let i = self.networks.len() - 1;
            self.start_network(i);
        }

        let mut flush_tick = tokio::time::interval(std::time::Duration::from_millis(100));
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // These retain the input permits, so storage backpressure reaches
        // socket readers while PING/PONG and read-only commands stay live.
        // Events received while loading come first.
        let mut deferred = early;
        loop {
            let capacity = self.msg_store.has_capacity();
            let ev = if capacity && !deferred.is_empty() {
                deferred.pop_front().unwrap()
            } else {
                tokio::select! {
                    ev = rx.recv() => match ev { Some(ev) => ev, None => break },
                    _ = flush_tick.tick(), if self.srv.db.is_remote() => {
                        self.msg_store.set_batch_limit(self.srv.config().basic.chat_history_limit);
                        if let Err(e) = self.msg_store.poll() {
                            logf!(self.logger, "Turso message batch failed; retaining it for retry: {e}");
                        }
                        continue;
                    }
                }
            };
            let bypass = match &ev {
                Event::UpstreamMessage { msg, .. } => {
                    matches!(msg.command.as_str(), "PING" | "PONG")
                }
                Event::DownstreamMessage { msg, .. } => {
                    !matches!(
                        msg.command.as_str(),
                        "PRIVMSG" | "NOTICE" | "TAGMSG" | "BATCH"
                    ) || msg.param(0).eq_ignore_ascii_case("BouncerServ")
                }
                Event::UpstreamConnected { .. }
                | Event::UpstreamClosed { .. }
                | Event::UpstreamConnectionError { .. } => false,
                _ => true,
            };
            // Keep each connection's input in order: e.g. a client's PART or
            // QUIT must not overtake its deferred PRIVMSG.
            let keepalive = matches!(&ev,
                Event::UpstreamMessage { msg, .. } | Event::DownstreamMessage { msg, .. }
                    if matches!(msg.command.as_str(), "PING" | "PONG"));
            let queued_source = !keepalive
                && event_source(&ev)
                    .is_some_and(|s| deferred.iter().any(|d| event_source(d) == Some(s)));
            if !capacity && (!bypass || queued_source) {
                deferred.push_back(ev);
                continue;
            }
            if self.srv.db.is_remote() {
                // Events held for storage backpressure precede STOP in the
                // input stream. Finish them before draining the store rather
                // than silently dropping already-read lines on shutdown.
                let before_stop = if matches!(ev, Event::Stop) {
                    std::mem::take(&mut deferred)
                } else {
                    std::collections::VecDeque::new()
                };
                // Cloud I/O must not stop the current-thread runtime's network
                // and timer tasks. The blocking pool retains the Tokio context
                // used by handlers to spawn upstream and deferred tasks.
                self = tokio::task::spawn_blocking(move || {
                    for queued in before_stop {
                        self.handle_event(queued);
                    }
                    self.handle_event(ev);
                    self
                })
                .await
                .expect("user storage task failed");
            } else {
                crate::server::blocking(|| self.handle_event(ev));
            }
            if self.stopped {
                break;
            }
        }
        if self.srv.db.is_remote() {
            tokio::task::spawn_blocking(move || self.close_msg_store())
                .await
                .expect("message store shutdown task failed");
        } else {
            self.close_msg_store();
        }
    }

    /// Loads the user's networks with their channels, flagged targets and
    /// delivery receipts.
    /// Takes the user's fields: the user can't be borrowed across awaits.
    async fn load_networks(
        srv: std::sync::Arc<crate::server::Server>,
        logger: crate::logger::Logger,
        user_id: i64,
        persistent: bool,
    ) -> anyhow::Result<Vec<Network>> {
        let mut records = db::run(&srv.db, move |db| db.list_networks(user_id))
            .await
            .map_err(|e| e.context("failed to list networks"))?;
        records.sort_by_key(|n| n.id);
        let mut networks = Vec::with_capacity(records.len());
        for record in records {
            let net_id = record.id;
            let context = format!("network {:?}", record.get_name());
            let channels = db::run(&srv.db, move |db| db.list_channels(net_id))
                .await
                .map_err(|e| e.context(format!("{context}: failed to list channels")))?;
            let targets = db::run(&srv.db, move |db| db.list_message_targets(net_id))
                .await
                .map_err(|e| e.context(format!("{context}: failed to list message targets")))?;
            let mut network = Network::new(&logger, record, channels);
            for mt in targets {
                network.set_message_target(mt);
            }
            if persistent {
                let receipts = db::run(&srv.db, move |db| db.list_delivery_receipts(net_id))
                    .await
                    .map_err(|e| {
                        e.context(format!("{context}: failed to load delivery receipts"))
                    })?;
                for rcpt in receipts {
                    network
                        .delivered
                        .store_id(&rcpt.target, &rcpt.client, &rcpt.internal_msg_id);
                }
            }
            networks.push(network);
        }
        Ok(networks)
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

/// Identifies the connection an event was read from: (upstream, ID).
fn event_source(ev: &Event) -> Option<(bool, u64)> {
    match ev {
        Event::UpstreamMessage { conn_id, .. } | Event::UpstreamClosed { conn_id, .. } => {
            Some((true, *conn_id))
        }
        Event::DownstreamMessage { dc_id, .. } | Event::DownstreamDisconnected { dc_id, .. } => {
            Some((false, *dc_id))
        }
        _ => None,
    }
}
