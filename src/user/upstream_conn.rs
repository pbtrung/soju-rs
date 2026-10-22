//! Upstream connections of a user's networks.

use super::{User, dc_ids_for, dcs_for};
use crate::downstream::{self, send_service_notice};
use crate::irc::{Message, Tags};
use crate::upstream::{self, ConnOutcome, UpstreamConn};
use crate::xirc;
use crate::{logf, msg};

impl User {
    pub fn handle_upstream_disconnected(&mut self, ni: usize) {
        let net = &mut self.networks[ni];
        let Some(uc) = net.conn.as_mut() else { return };
        let was_attached = uc.attached;
        uc.attached = false;
        uc.stop_regain_nick_timer();
        for uch in uc.channels.values_mut() {
            uch.update_auto_detach(None);
        }
        let pending = std::mem::take(&mut uc.pending_cmds);
        let net_id = net.id();
        upstream::abort_pending_commands(&mut self.downstreams, pending);

        if !was_attached {
            return;
        }

        for dc_id in dc_ids_for(&self.downstreams, net_id) {
            downstream::update_supported_caps(self, dc_id);
        }

        self.notify_bouncer_network_state(net_id, &Tags::from_iter([("state", "disconnected")]));

        let net = &self.networks[ni];
        if net.last_error.is_none() {
            let text = format!("disconnected from {}", net.get_name());
            for dc in dcs_for(&mut self.downstreams, net_id) {
                if !dc.caps.is_enabled("soju.im/bouncer-networks") {
                    send_service_notice(dc, &text);
                }
            }
        }
    }

    pub(super) fn upstream_connected(&mut self, ni: usize, new: upstream::NewConn) {
        let net = &mut self.networks[ni];
        if let Some(old) = net.conn.take() {
            old.conn.close();
            old.stop_timers();
        }
        let uc = UpstreamConn::new(new, &net.logger, net.casemap);
        net.conn = Some(uc);
        upstream::register(self, ni);
    }

    pub(super) fn upstream_message(&mut self, net_id: i64, conn_id: u64, msg: Message) {
        let Some(ni) = self.net_index(net_id) else {
            return;
        };
        let Some(uc) = self.networks[ni].conn.as_mut() else {
            return;
        };
        if uc.id != conn_id {
            return;
        }
        if uc.conn.is_closed() {
            logf!(uc.logger, "ignoring message on closed connection: {msg}");
            return;
        }
        if let Some(buf) = &mut uc.delayed_messages {
            // Keep the connection alive while connect commands are sent
            if msg.command == "PING" {
                uc.send_message(Message::new("PONG", msg.params));
                return;
            }
            buf.push(msg);
            return;
        }
        self.process_upstream_message(ni, msg);
    }

    fn process_upstream_message(&mut self, ni: usize, mut msg: Message) {
        let registered_before = self.networks[ni]
            .conn
            .as_ref()
            .is_some_and(|uc| uc.registered);
        let res = upstream::handle_message(self, ni, &mut msg);
        let Some(uc) = self.networks[ni].conn.as_mut() else {
            return;
        };
        if let Err(e) = res {
            if !registered_before && !uc.registered {
                // Errors during registration abort the connection
                let err = match e.downcast_ref::<upstream::RegistrationError>() {
                    Some(reg) => upstream::OutcomeError {
                        text: format!("failed to register: {}", reg.reason()),
                        event: format!("failed to register: {reg}"),
                        temporary: reg.temporary(),
                    },
                    None => {
                        let mut m = msg.clone();
                        m.tags.clear();
                        let text = format!(
                            "failed to register: failed to handle message {:?}: {e}",
                            m.to_string()
                        );
                        upstream::OutcomeError {
                            event: text.clone(),
                            text,
                            temporary: true,
                        }
                    }
                };
                uc.reg_error = Some(err);
                uc.conn.close();
                return;
            }
            logf!(
                uc.logger,
                "failed to handle message {:?}: {e}",
                msg.to_string()
            );
            return;
        }
        if uc.registered && !registered_before {
            upstream::finish_registration(self, ni);
        }
    }

    pub(super) fn upstream_closed(
        &mut self,
        net_id: i64,
        conn_id: u64,
        read_error: Option<String>,
    ) {
        let Some(ni) = self.net_index(net_id) else {
            return;
        };
        if self.networks[ni].conn.as_ref().map(|c| c.id) != Some(conn_id) {
            return;
        }
        let was_attached = self.networks[ni].upstream().is_some();
        if was_attached {
            self.handle_upstream_disconnected(ni);
        }
        let mut uc = self.networks[ni].conn.take().unwrap();
        uc.conn.close();
        uc.stop_timers();

        let outcome = if let Some(err) = uc.reg_error.take() {
            ConnOutcome { err: Some(err) }
        } else if !uc.registered {
            let reason = read_error.unwrap_or_else(|| "EOF".to_owned());
            let text = format!("failed to register: failed to read message: {reason}");
            ConnOutcome {
                err: Some(upstream::OutcomeError {
                    event: text.clone(),
                    text,
                    temporary: true,
                }),
            }
        } else if let Some(e) = read_error {
            let text = format!("failed to handle messages: failed to read IRC command: {e}");
            ConnOutcome {
                err: Some(upstream::OutcomeError {
                    event: text.clone(),
                    text,
                    temporary: true,
                }),
            }
        } else {
            ConnOutcome { err: None }
        };
        if let Some(tx) = uc.outcome_tx.take() {
            let _ = tx.send(outcome);
        }
    }

    /// Called once registration completed and the post-registration delay
    /// (if any) elapsed: joins channels and makes the connection available
    /// to downstreams.
    pub fn attach_upstream(&mut self, ni: usize) {
        let net = &mut self.networks[ni];
        let Some(uc) = net.conn.as_mut() else { return };
        if uc.attached {
            return;
        }

        if !net.channels.is_empty() {
            let (channels, keys): (Vec<String>, Vec<String>) = net
                .channels
                .values()
                .map(|ch| (ch.name.clone(), ch.key.clone()))
                .unzip();
            for m in xirc::generate_join(&channels, &keys) {
                uc.send_message(m);
            }
        }

        uc.attached = true;
        let delayed = uc.delayed_messages.take();
        let net_id = net.id();

        self.update_away(ni);
        self.update_monitor(ni);

        let name = self.networks[ni].get_name().to_owned();
        for dc_id in dc_ids_for(&self.downstreams, net_id) {
            downstream::update_supported_caps(self, dc_id);
            let Some(i) = self.dc_index(dc_id) else {
                continue;
            };
            if !self.downstreams[i]
                .caps
                .is_enabled("soju.im/bouncer-networks")
            {
                send_service_notice(&mut self.downstreams[i], &format!("connected to {name}"));
            }
            downstream::update_nick(self, dc_id);
            downstream::update_host(self, dc_id);
            downstream::update_realname(self, dc_id);
            downstream::update_account(self, dc_id);
            let cm = self.networks[ni].casemap;
            if let Some(i) = self.dc_index(dc_id) {
                self.downstreams[i].update_casemapping(cm);
            }
        }
        self.notify_bouncer_network_state(
            net_id,
            &Tags::from_iter([("state", "connected"), ("error", "")]),
        );
        self.networks[ni].last_error = None;

        for msg in delayed.unwrap_or_default() {
            if self.networks[ni].conn.is_none() {
                break;
            }
            self.process_upstream_message(ni, msg);
        }
    }

    pub fn update_away(&mut self, ni: usize) {
        let net = &self.networks[ni];
        if !net.record.auto_away {
            return;
        }
        let net_id = net.id();
        let away = self
            .downstreams
            .iter()
            .filter(|d| d.network_id == Some(net_id))
            .all(|d| d.away.is_some());
        let net = &mut self.networks[ni];
        let Some(uc) = net.upstream_mut() else { return };
        if away == uc.away {
            return;
        }
        if away {
            let reason = if uc.caps.is_available("draft/pre-away") {
                "*"
            } else {
                "Auto away"
            };
            uc.send_message(msg!("AWAY", reason));
        } else {
            uc.send_message(msg!("AWAY"));
        }
        uc.away = away;
    }

    pub fn update_monitor(&mut self, ni: usize) {
        upstream::update_monitor(self, ni);
    }

    pub(super) fn try_regain_nick(&mut self, ni: usize, nick: &str) {
        upstream::try_regain_nick(self, ni, nick);
    }
}
