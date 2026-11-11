//! Upstream connection initialization, output, timers and cached user queries.

use super::*;

impl UpstreamConn {
    pub fn new(new: NewConn, net_logger: &Logger, casemap: CaseMapping) -> UpstreamConn {
        UpstreamConn {
            id: new.id,
            conn: new.conn,
            logger: net_logger.clone(),
            net_id: new.net_id,
            events: new.events,
            srv: new.srv,
            rate_flag: new.rate_flag,
            outcome_tx: Some(new.outcome_tx),
            reg_error: None,
            attached: false,
            delayed_messages: None,
            server_prefix: Prefix::new("*"),
            available_channel_modes: ircutil::std_channel_modes(),
            available_channel_types: ircutil::STD_CHANNEL_TYPES.to_owned(),
            available_status_msg: String::new(),
            available_memberships: ircutil::std_memberships(),
            isupport: Isupport::new(),
            registered: false,
            registered_token: new.registered,
            nick: String::new(),
            username: String::new(),
            realname: String::new(),
            hostname: String::new(),
            modes: UserModes::default(),
            channels: CaseMappingMap::new(casemap),
            users: CaseMappingMap::new(casemap),
            caps: CapRegistry::new(),
            batches: HashMap::new(),
            away: false,
            account: String::new(),
            next_label_id: 0,
            next_batch_ref: std::cell::Cell::new(0),
            monitored: CaseMappingMap::new(casemap),
            sasl_client: None,
            sasl_started: false,
            pending_cmds: HashMap::new(),
            ended_early: None,
            pending_regain_nick: String::new(),
            regain_nick_timer: None,
            regain_nick_backoff: None,
            got_motd: false,
            has_desired_nick: true,
        }
    }

    pub fn send_message(&self, msg: Message) {
        let mut msg = msg;
        if !self.caps.is_enabled("message-tags") {
            msg.tags.clear();
        }
        let prepared = xirc::prepare_text_message(
            msg,
            self.multiline_limits().is_some(),
            || self.prefix_len(),
            || {
                let r = self.next_batch_ref.get() + 1;
                self.next_batch_ref.set(r);
                r.to_string()
            },
        );
        match prepared {
            xirc::TextMessage::Single(m) => self.raw_send(m),
            xirc::TextMessage::Batch(l) => {
                for m in l {
                    self.raw_send(m);
                }
            }
            xirc::TextMessage::Lines(l) => {
                // Only the first message carries the label
                for (i, mut m) in l.into_iter().enumerate() {
                    if i > 0 {
                        m.tags.remove("label");
                    }
                    self.raw_send(m);
                }
            }
        }
    }

    pub(super) fn raw_send(&self, msg: Message) {
        metrics::inc(&self.srv.metrics.upstream_out_messages_total);
        self.conn.send_message(msg);
    }

    /// The length of the prefix the server relays our messages with.
    pub(super) fn prefix_len(&self) -> usize {
        // Assume the longest hostname if we don't know ours
        let host_len = if self.hostname.is_empty() {
            63
        } else {
            self.hostname.len()
        };
        let user_len = self.username.len().max(10);
        self.nick.len() + 1 + user_len + 1 + host_len
    }

    /// The draft/multiline limits of the server, if supported.
    pub fn multiline_limits(&self) -> Option<xirc::MultilineLimits> {
        if !self.caps.is_enabled("batch") || !self.caps.is_enabled(xirc::MULTILINE_BATCH_TYPE) {
            return None;
        }
        let v = self
            .caps
            .available
            .get(xirc::MULTILINE_BATCH_TYPE)
            .map(String::as_str)
            .unwrap_or("");
        Some(xirc::MultilineLimits::parse(v))
    }

    /// Returns the number of lines a multiline message would be sent as.
    pub fn multiline_line_count(&self, msg: &Message) -> usize {
        let max = xirc::max_text_length(self.prefix_len(), &msg.command, msg.param(0));
        xirc::split_multiline(msg.param(1), max).len()
    }

    /// Sends a message, labeled so that replies get routed to a downstream
    /// connection. label is the downstream's label (may be empty).
    pub fn send_message_labeled(&mut self, downstream_id: u64, label: &str, msg: Message) {
        let mut msg = msg;
        if self.caps.is_enabled("labeled-response") {
            msg.tags.set(
                "label",
                format!("sd-{downstream_id}-{}-{label}", self.next_label_id),
            );
            self.next_label_id += 1;
        }
        self.send_message(msg);
    }

    pub fn is_channel(&self, entity: &str) -> bool {
        entity
            .chars()
            .next()
            .is_some_and(|c| self.available_channel_types.contains(c))
    }

    pub fn supports_sasl(&self, mech: &str) -> bool {
        let Some(v) = self.caps.available.get("sasl") else {
            return false;
        };
        if v.is_empty() {
            return true;
        }
        v.split(',').any(|m| m.eq_ignore_ascii_case(mech))
    }

    pub fn stop_timers(&self) {
        if let Some(t) = &self.regain_nick_timer {
            t.abort();
        }
        for uch in self.channels.values() {
            if let Some(t) = &uch.detach_timer {
                t.handle.abort();
            }
        }
    }

    pub fn stop_regain_nick_timer(&mut self) {
        if let Some(t) = self.regain_nick_timer.take() {
            // Maybe we're racing with the timer task, so maybe we'll receive
            // a TryRegainNick event later on, but try_regain_nick handles
            // that case
            t.abort();
        }
        self.regain_nick_backoff = None;
    }

    pub(super) fn schedule_regain_nick(&mut self, delay: Duration, nick: String) {
        let tx = self.events.clone();
        let (net_id, conn_id) = (self.net_id, self.id);
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Event::TryRegainNick {
                net_id,
                conn_id,
                nick,
            });
        });
        self.regain_nick_timer = Some(task.abort_handle());
    }

    /// Starts the auto-detach timer of a channel, replacing the current one.
    pub(super) fn start_detach_timer(&mut self, name: &str, deadline: tokio::time::Instant) {
        let tx = self.events.clone();
        let (net_id, conn_id) = (self.net_id, self.id);
        let Some(uch) = self.channels.get_mut(name) else {
            return;
        };
        let name = uch.name.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let _ = tx.send(Event::ChannelDetach {
                net_id,
                conn_id,
                name,
            });
        })
        .abort_handle();
        uch.stop_auto_detach();
        uch.detach_timer = Some(DetachTimer {
            handle,
            fires_at: deadline,
            deadline,
        });
    }

    pub fn cancel_pending_commands_by_downstream_id(&mut self, downstream_id: u64) {
        for l in self.pending_cmds.values_mut() {
            // We can't cancel the currently running command stored in l[0]
            let mut i = l.len();
            while i > 1 {
                i -= 1;
                if l[i].downstream_id == downstream_id {
                    l.remove(i);
                }
            }
        }
    }

    pub(super) fn handle_supported_caps(&mut self, caps: &str) {
        for s in caps.split_whitespace() {
            let (k, v) = match s.split_once('=') {
                Some((k, v)) => (k, v),
                None => (s, ""),
            };
            self.caps.available.insert(k.to_lowercase(), v.to_owned());
        }
    }

    pub(super) fn update_caps(&mut self) {
        let request = caps_to_request(&self.caps);
        if request.is_empty() {
            return;
        }
        self.send_message(msg!("CAP", "REQ", request.join(" ")));
    }

    pub(super) fn cache_user_info(&mut self, nick: &str, mut info: UpstreamUser) {
        if nick.is_empty() {
            return;
        }
        if let Some(uu) = self.users.get_mut(nick) {
            uu.update_from(&info);
            if !info.nickname.is_empty() && nick != info.nickname {
                let uu = self.users.del(nick).unwrap();
                let new_nick = uu.nickname.clone();
                self.users.set(&new_nick, uu);
            }
        } else {
            let key = if !info.nickname.is_empty() {
                info.nickname.clone()
            } else {
                info.nickname = nick.to_owned();
                nick.to_owned()
            };
            self.users.set(&key, info);
        }
    }

    pub fn get_cached_who(&self, mask: &str, fields: &str) -> Option<Vec<UpstreamUser>> {
        // Non-extended WHO fields
        let fields = if fields.is_empty() {
            "cuhsnfdr"
        } else {
            fields
        };

        // Some extensions are required to keep our cached state in sync. We
        // could require setname for 'r' and chghost for 'h'/'s', but servers
        // usually implement a QUIT/JOIN fallback, so let's not bother.
        if fields.contains('a') && !self.caps.is_enabled("account-notify") {
            return None;
        }
        if fields.contains('f') && !self.caps.is_enabled("away-notify") {
            return None;
        }

        if let Some(uu) = self.users.get(mask) {
            if uu.has_whox_fields(fields) {
                return Some(vec![uu.clone()]);
            }
        } else if let Some(uch) = self.channels.get(mask) {
            let mut l = Vec::with_capacity(uch.members.len());
            for nick in uch.members.keys() {
                let uu = self.users.get(nick)?;
                if !uu.has_whox_fields(fields) {
                    return None;
                }
                l.push(uu.clone());
            }
            return Some(l);
        }
        None
    }
}
