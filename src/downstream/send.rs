//! Creating downstream connections and sending them messages, within
//! labeled responses and batches.

use super::*;

impl DownstreamConn {
    pub fn new(
        srv: Arc<Server>,
        conn: Conn,
        info: ConnInfo,
        id: u64,
        logger: Logger,
    ) -> DownstreamConn {
        let cfg = srv.config();
        let cm = xirc::CASEMAPPING_ASCII;
        let mut caps = CapRegistry::new();
        for (k, v) in PERMANENT_DOWNSTREAM_CAPS {
            caps.available.insert(k.to_string(), v.to_string());
        }
        let mut mechs = Vec::new();
        if cfg.auth.plain.is_some() {
            mechs.push("PLAIN");
        }
        if cfg.auth.oauth_bearer.is_some() {
            mechs.push("OAUTHBEARER");
        }
        if cfg.basic.client_cert_auth && info.peer_cert.is_some() {
            mechs.push("EXTERNAL");
        }
        caps.available.insert("sasl".to_owned(), mechs.join(","));
        // TODO: we should only enable chathistory after authentication and
        // then check that the user's message store supports it
        match cfg.basic.msg_store.driver {
            crate::config::MsgStoreDriver::Db => {
                caps.available
                    .insert("draft/chathistory".to_owned(), String::new());
                caps.available
                    .insert("soju.im/search".to_owned(), String::new());
            }
            crate::config::MsgStoreDriver::Memory => {}
        }
        if cfg.basic.client_cert_auth {
            caps.available
                .insert("soju.im/client-cert".to_owned(), String::new());
        }

        DownstreamConn {
            id,
            conn,
            hostname: crate::conn::host_of(&info.remote_addr),
            info,
            srv,
            logger,
            registered: false,
            network_id: None,
            client_name: String::new(),
            impersonating: false,
            nick: "*".to_owned(),
            nick_cm: "*".to_owned(),
            realname: String::new(),
            username: "~u".to_owned(),
            account: String::new(),
            away: None,
            cap_version: 0,
            caps,
            isupport: Isupport::new(),
            sasl: None,
            registration: Some(Registration::default()),
            last_batch_ref: 0,
            multiline_batches: HashMap::new(),
            casemap: cm,
            monitored: CaseMappingMap::new(cm),
            metadata_subs: HashSet::new(),
            label: None,
        }
    }

    pub fn prefix(&self) -> Prefix {
        Prefix::full(
            self.nick.clone(),
            self.username.clone(),
            self.hostname.clone(),
        )
    }

    pub fn casemap(&self, s: &str) -> String {
        (self.casemap)(s)
    }

    fn raw_send(&self, msg: Message) {
        metrics::inc(&self.srv.metrics.downstream_out_messages_total);
        self.conn.send_message(msg);
    }

    /// Sends a message, adapting it to the client's capabilities and
    /// wrapping it in a labeled-response if a label is pending.
    pub fn send_message(&mut self, msg: Message) {
        let mut msg = msg;
        if !self.caps.is_enabled("message-tags") {
            if msg.command == "TAGMSG" {
                return;
            }
            let caps = &self.caps;
            msg.tags.retain(|name, _| match name {
                "time" => caps.is_enabled("server-time"),
                "account" => caps.is_enabled("account-tag"),
                "batch" => caps.is_enabled("batch"),
                _ => false,
            });
        }
        if !self.caps.is_enabled("batch") && !msg.tags.get_or_empty("batch").is_empty() {
            msg.tags.remove("batch");
        }
        if msg.command == "JOIN" && !self.caps.is_enabled("extended-join") {
            msg.params.truncate(1);
        }
        let required_cap = match msg.command.as_str() {
            "SETNAME" => Some("setname"),
            "CHGHOST" => Some("chghost"),
            "AWAY" => Some("away-notify"),
            "ACCOUNT" => Some("account-notify"),
            "MARKREAD" => Some("draft/read-marker"),
            "READ" => Some("soju.im/read"),
            "METADATA" => Some("draft/metadata-2"),
            "REDACT" => Some("draft/message-redaction"),
            _ => None,
        };
        if let Some(cap) = required_cap
            && !self.caps.is_enabled(cap)
        {
            return;
        }
        if msg.prefix.as_ref().is_some_and(|p| p.name == "*") {
            // We use "*" as a sentinel value to simplify upstream message
            // handling
            msg.prefix = None;
        }
        if msg.prefix.as_ref().is_none_or(|p| p.name.is_empty())
            && ircutil::is_numeric(&msg.command)
        {
            // Numerics must always carry a source
            msg.prefix = Some(self.srv.prefix());
        }

        let multiline =
            self.caps.is_enabled("batch") && self.caps.is_enabled(xirc::MULTILINE_BATCH_TYPE);
        let prefix_len = msg.prefix.as_ref().map(|p| p.formatted_len());
        let srv = &self.srv;
        let last_batch_ref = &mut self.last_batch_ref;
        let prepared = xirc::prepare_text_message(
            msg,
            multiline,
            || prefix_len.unwrap_or_else(|| srv.prefix().formatted_len()),
            || {
                *last_batch_ref += 1;
                last_batch_ref.to_string()
            },
        );
        match prepared {
            xirc::TextMessage::Single(m) => self.send_reply(vec![m]),
            // A multiline batch is a single reply
            xirc::TextMessage::Batch(l) => self.send_reply(l),
            xirc::TextMessage::Lines(l) => {
                for m in l {
                    self.send_reply(vec![m]);
                }
            }
        }
    }

    /// Sends a reply made of one or more messages. The reply is wrapped in
    /// a labeled-response batch if needed: a multi-message reply (e.g. a
    /// draft/multiline batch) counts as a single reply, its first message
    /// carries the label or batch tag.
    fn send_reply(&mut self, mut reply: Vec<Message>) {
        let Some(label) = self.label.as_mut().filter(|l| !l.label.is_empty()) else {
            for m in reply {
                self.raw_send(m);
            }
            return;
        };
        if let Some(mut pending) = label.pending_msg.take() {
            // create a batch
            self.last_batch_ref += 1;
            let batch = self.last_batch_ref.to_string();
            let label_str = label.label.clone();
            label.batch = batch.clone();
            if pending[0].tags.get_or_empty("batch").is_empty() {
                pending[0].tags.set("batch", batch.clone());
            }
            self.raw_send(
                msg!("BATCH", format!("+{batch}"), "labeled-response").with_tag("label", label_str),
            );
            // send the buffered reply
            for m in pending {
                self.raw_send(m);
            }
        }
        let label = self.label.as_mut().unwrap();
        if !label.batch.is_empty() {
            // send the current reply in the batch
            if reply[0].tags.get_or_empty("batch").is_empty() {
                reply[0].tags.set("batch", label.batch.clone());
            }
            for m in reply {
                self.raw_send(m);
            }
        } else {
            // first reply we're sending: buffer it
            label.pending_msg = Some(reply);
        }
    }

    /// Ends a labeled-response: sends the buffered message, closes the
    /// batch or sends an ACK.
    pub fn flush_batch(&mut self) {
        let Some(label) = self.label.as_mut().filter(|l| !l.label.is_empty()) else {
            return;
        };
        let label_str = label.label.clone();
        if let Some(mut pending) = label.pending_msg.take() {
            pending[0].tags.set("label", label_str);
            for m in pending {
                self.raw_send(m);
            }
        } else if !label.batch.is_empty() {
            let batch = label.batch.clone();
            self.raw_send(msg!("BATCH", format!("-{batch}")));
        } else {
            self.raw_send(msg!("ACK").with_tag("label", label_str));
        }
    }

    /// Takes the label of the current command, so that the upstream
    /// server's labeled response can be routed back.
    pub fn take_label(&mut self) -> String {
        match &mut self.label {
            Some(l) => std::mem::take(&mut l.label),
            None => String::new(),
        }
    }

    /// Drops the label of the current command, for commands mixing local
    /// and remote replies.
    pub fn drop_label(&mut self) {
        if let Some(l) = &mut self.label {
            l.label.clear();
        }
    }

    /// Starts a batch if the client supports it, returning its reference.
    pub fn begin_batch(&mut self, typ: &str, params: &[String], tags: Tags) -> String {
        self.last_batch_ref += 1;
        let r = self.last_batch_ref.to_string();
        if self.caps.is_enabled("batch") {
            let mut p = vec![format!("+{r}"), typ.to_owned()];
            p.extend_from_slice(params);
            self.send_message(Message::new("BATCH", p).with_tags(tags));
        }
        r
    }

    pub fn end_batch(&mut self, r: &str) {
        if self.caps.is_enabled("batch") {
            self.send_message(msg!("BATCH", format!("-{r}")));
        }
    }

    /// Sends messages in a batch, tagging each with the batch reference.
    pub fn send_batch(&mut self, typ: &str, params: &[String], msgs: Vec<Message>) {
        let r = self.begin_batch(typ, params, Tags::new());
        for mut m in msgs {
            m.tags.set("batch", r.clone());
            self.send_message(m);
        }
        self.end_batch(&r);
    }

    /// Sends a message with an internal ID, so that clients without
    /// chathistory acknowledge it via PING/PONG.
    pub fn send_message_with_id(&mut self, msg: Message, id: &str) {
        let supports_backlog = message_supports_backlog(&msg);
        self.send_message(msg);
        if id.is_empty() || !supports_backlog || self.caps.is_enabled("draft/chathistory") {
            return;
        }
        self.send_ping(id);
    }

    /// Advances history to a message ID without sending the message, e.g.
    /// for self-messages when echo-message isn't enabled.
    pub fn advance_message_with_id(&mut self, msg: &Message, id: &str) {
        if id.is_empty()
            || !message_supports_backlog(msg)
            || self.caps.is_enabled("draft/chathistory")
        {
            return;
        }
        self.send_ping(id);
    }

    fn send_ping(&mut self, msg_id: &str) {
        self.send_message(msg!("PING", format!("soju-msgid-{msg_id}")));
    }

    pub fn end_sasl(&mut self, msg: Option<Message>) {
        if self.sasl.is_none() {
            return;
        }
        self.sasl = None;
        let m = msg.unwrap_or_else(|| {
            msg!(
                RPL_SASLSUCCESS,
                self.nick.clone(),
                "SASL authentication successful"
            )
        });
        self.send_message(m);
    }
}
