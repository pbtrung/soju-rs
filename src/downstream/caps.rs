//! Capabilities and ISUPPORT tokens advertised to downstream connections.

use super::*;

/// Always-supported downstream capabilities.
pub(super) const PERMANENT_DOWNSTREAM_CAPS: &[(&str, &str)] = &[
    ("batch", ""),
    ("cap-notify", ""),
    ("echo-message", ""),
    ("invite-notify", ""),
    ("no-implicit-names", ""),
    ("server-time", ""),
    ("setname", ""),
    ("draft/extended-isupport", ""),
    (
        "draft/metadata-2",
        "before-connect,max-keys=0,max-value-bytes=1",
    ),
    ("draft/no-implicit-names", ""),
    ("draft/pre-away", ""),
    ("draft/read-marker", ""),
    ("soju.im/account-required", ""),
    ("soju.im/bouncer-networks", ""),
    ("soju.im/bouncer-networks-notify", ""),
    ("soju.im/multiplex", ""),
    ("soju.im/no-implicit-names", ""),
    ("soju.im/read", ""),
    ("soju.im/webpush", ""),
];

/// Downstream capabilities which require support from the upstream server
/// to be enabled.
pub const PASSTHROUGH_DOWNSTREAM_CAPS: &[(&str, &str)] = &[
    ("account-notify", ""),
    ("account-tag", ""),
    ("away-notify", ""),
    ("chghost", ""),
    ("extended-join", ""),
    ("extended-monitor", ""),
    ("labeled-response", ""),
    ("message-tags", ""),
    ("multi-prefix", ""),
    ("draft/extended-monitor", ""),
    ("draft/message-redaction", ""),
];

/// ISUPPORT tokens always passed to downstream clients.
const PERMANENT_ISUPPORT: &[&str] = &["soju.im/SAFERATE", "draft/ACCOUNTREQUIRED"];

/// ISUPPORT tokens directly passed through from the upstream server.
const PASSTHROUGH_ISUPPORT: &[&str] = &[
    "ACCOUNTEXTBAN",
    "AWAYLEN",
    "BOT",
    "CASEMAPPING",
    "CHANLIMIT",
    "CHANMODES",
    "CHANNELLEN",
    "CHANTYPES",
    "CLIENTTAGDENY",
    "ELIST",
    "EXCEPTS",
    "EXTBAN",
    "HOSTLEN",
    "INVEX",
    "KICKLEN",
    "LINELEN",
    "MAXLIST",
    "MAXTARGETS",
    "MODES",
    "MONITOR",
    "NAMELEN",
    "NETWORK",
    "NICKLEN",
    "PREFIX",
    "SAFELIST",
    "STATUSMSG",
    "TARGMAX",
    "TOPICLEN",
    "USERLEN",
    "UTF8ONLY",
    "WHOX",
    "draft/ICON",
];

impl DownstreamConn {
    pub fn set_supported_cap(&mut self, name: &str, value: &str) {
        let changed = self.caps.available.get(name).is_none_or(|v| v != value);
        self.caps
            .available
            .insert(name.to_owned(), value.to_owned());
        if !self.caps.is_enabled("cap-notify") || !changed {
            return;
        }
        let cap = if !value.is_empty() && self.cap_version >= 302 {
            format!("{name}={value}")
        } else {
            name.to_owned()
        };
        let p = self.srv.prefix();
        let nick = self.nick.clone();
        self.send_message(msg!("CAP", nick, "NEW", cap).with_prefix(p));
    }

    pub fn unset_supported_cap(&mut self, name: &str) {
        let had = self.caps.is_available(name);
        self.caps.del(name);
        if !self.caps.is_enabled("cap-notify") || !had {
            return;
        }
        let p = self.srv.prefix();
        let nick = self.nick.clone();
        self.send_message(msg!("CAP", nick, "DEL", name).with_prefix(p));
    }

    pub fn update_casemapping(&mut self, cm: CaseMapping) {
        self.casemap = cm;
        self.nick_cm = cm(&self.nick);
        self.monitored.set_casemapping(cm);
    }

    /// Sends a CAP LS or LIST reply. With CAP 302, it's split over several
    /// lines if needed, all but the last marked with "*", so that each line
    /// fits in an IRC message.
    fn send_cap_list(&mut self, subcmd: &str, caps: &[String], prefix: Prefix) {
        // Room for ":<prefix> CAP <nick> <subcmd> * :" in 512 bytes
        let max = xirc::MAX_MESSAGE_LENGTH
            - 2
            - (prefix.formatted_len() + 2)
            - (self.nick.len() + subcmd.len() + 10);
        let mut lines: Vec<String> = vec![String::new()];
        for cap in caps {
            let line = lines.last_mut().unwrap();
            if self.cap_version >= 302 && !line.is_empty() && line.len() + 1 + cap.len() > max {
                lines.push(cap.clone());
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(cap);
            }
        }
        let n = lines.len();
        for (i, line) in lines.into_iter().enumerate() {
            let mut params = vec![self.nick.clone(), subcmd.to_owned()];
            if i + 1 < n {
                params.push("*".to_owned());
            }
            params.push(line);
            self.send_message(Message::new("CAP", params).with_prefix(prefix.clone()));
        }
    }

    pub fn handle_cap(&mut self, msg: &Message) -> Result<()> {
        need_params(msg, 1)?;
        let cmd = msg.params[0].to_uppercase();
        let args = &msg.params[1..];
        let p = self.srv.prefix();
        match cmd.as_str() {
            "LS" => {
                if let Some(v) = args.first() {
                    self.cap_version = v
                        .parse()
                        .map_err(|_| anyhow!("strconv.Atoi: parsing {v:?}: invalid syntax"))?;
                }
                if !self.registered && self.cap_version >= 302 {
                    // Let downstream show everything it supports, and trim
                    // down the available capabilities when upstreams are
                    // known.
                    for (k, v) in PASSTHROUGH_DOWNSTREAM_CAPS {
                        self.caps.available.insert(k.to_string(), v.to_string());
                    }
                    self.caps.available.insert(
                        xirc::MULTILINE_BATCH_TYPE.to_owned(),
                        xirc::MultilineLimits::default().cap_value(),
                    );
                }
                let caps: Vec<String> = self
                    .caps
                    .available
                    .iter()
                    .map(|(k, v)| {
                        if self.cap_version >= 302 && !v.is_empty() {
                            format!("{k}={v}")
                        } else {
                            k.clone()
                        }
                    })
                    .collect();
                self.send_cap_list("LS", &caps, p);
                if self.cap_version >= 302 {
                    // CAP version 302 implicitly enables cap-notify
                    self.caps.set_enabled("cap-notify", true);
                }
                if let Some(r) = &mut self.registration
                    && !self.registered
                {
                    r.negotiating_caps = true;
                }
            }
            "LIST" => {
                let caps: Vec<String> = self.caps.enabled.iter().cloned().collect();
                self.send_cap_list("LIST", &caps, p);
            }
            "REQ" => {
                let Some(arg) = args.first() else {
                    return Err(irc_error(msg!(
                        ERR_INVALIDCAPCMD,
                        self.nick.clone(),
                        cmd,
                        "Missing argument in CAP REQ command"
                    )));
                };
                let mut ack = true;
                let mut changes = Vec::new();
                for name in arg.split_whitespace() {
                    let name = name.to_lowercase();
                    let (name, enable) = match name.strip_prefix('-') {
                        Some(n) => (n.to_owned(), false),
                        None => (name, true),
                    };
                    if enable == self.caps.is_enabled(&name) {
                        continue;
                    }
                    if !self.caps.is_available(&name) {
                        ack = false;
                        break;
                    }
                    if name == "cap-notify" && self.cap_version >= 302 && !enable {
                        // cap-notify cannot be disabled with CAP version 302
                        ack = false;
                        break;
                    }
                    if name == "soju.im/account-required" {
                        // account-required is an informational cap
                        ack = false;
                        break;
                    }
                    changes.push((name, enable));
                }
                // Atomically ack the whole capability set
                if ack {
                    for (name, enable) in changes {
                        self.caps.set_enabled(&name, enable);
                    }
                }
                let reply = if ack { "ACK" } else { "NAK" };
                let nick = self.nick.clone();
                self.send_message(msg!("CAP", nick, reply, arg.clone()).with_prefix(p));
                if let Some(r) = &mut self.registration
                    && !self.registered
                {
                    r.negotiating_caps = true;
                }
            }
            "END" => {
                if let Some(r) = &mut self.registration
                    && !self.registered
                {
                    r.negotiating_caps = false;
                }
            }
            _ => {
                return Err(irc_error(msg!(
                    ERR_INVALIDCAPCMD,
                    self.nick.clone(),
                    cmd,
                    "Unknown CAP command"
                )));
            }
        }
        Ok(())
    }
}

/// Context for building ISUPPORT tokens.
pub struct IsupportCtx<'a> {
    pub has_user: bool,
    pub network_id: Option<i64>,
    pub chathistory: bool,
    pub upstream: Option<&'a UpstreamConn>,
}

/// The ISUPPORT tokens of a registered downstream connection.
pub(super) fn build_user_isupport(u: &User, i: usize) -> Isupport {
    let dc = &u.downstreams[i];
    let ctx = IsupportCtx {
        has_user: true,
        network_id: dc.network_id,
        chathistory: u.has_chathistory_store(),
        upstream: dc_network(u, i).and_then(|ni| u.networks[ni].upstream()),
    };
    build_isupport(dc, &ctx)
}

pub fn build_isupport(dc: &DownstreamConn, ctx: &IsupportCtx) -> Isupport {
    let cfg = dc.srv.config();
    let mut isupport = Isupport::new();
    for k in PERMANENT_ISUPPORT {
        isupport.insert(k.to_string(), None);
    }
    if let Some(id) = ctx.network_id {
        isupport.insert("BOUNCER_NETID".to_owned(), Some(id.to_string()));
    }
    if ctx.network_id.is_none() && !cfg.basic.title.is_empty() {
        isupport.insert("NETWORK".to_owned(), Some(cfg.basic.title.clone()));
    }
    if ctx.network_id.is_none() && !cfg.icon_url.is_empty() {
        isupport.insert("draft/ICON".to_owned(), Some(cfg.icon_url.clone()));
    } else if ctx.network_id.is_none() && !cfg.icon_path.is_empty() {
        isupport.insert(
            "draft/ICON".to_owned(),
            Some(format!("{}/icon", cfg.basic.http_ingress)),
        );
    }
    if ctx.has_user {
        if ctx.network_id.is_none() {
            isupport.insert("BOT".to_owned(), Some("B".to_owned()));
            isupport.insert("CASEMAPPING".to_owned(), Some("ascii".to_owned()));
            isupport.insert("WHOX".to_owned(), None);
            // channels are not supported
            isupport.insert("CHANTYPES".to_owned(), Some(String::new()));
            // default bufio.Reader size
            isupport.insert("LINELEN".to_owned(), Some("4096".to_owned()));
        }
        if ctx.chathistory && ctx.network_id.is_some() {
            isupport.insert(
                "CHATHISTORY".to_owned(),
                Some(cfg.basic.chat_history_limit.to_string()),
            );
            isupport.insert("MSGREFTYPES".to_owned(), Some("timestamp".to_owned()));
        }
        if dc.caps.is_enabled("soju.im/webpush")
            && let Some(wp) = dc.srv.web_push()
        {
            isupport.insert("VAPID".to_owned(), Some(wp.vapid_public.clone()));
        }
    }

    if let Some(uc) = ctx.upstream {
        // If upstream doesn't support message-tags, indicate that we'll drop
        // all of them
        if !uc.isupport.contains_key("CLIENTTAGDENY") && !uc.caps.is_enabled("message-tags") {
            isupport.insert("CLIENTTAGDENY".to_owned(), Some("*".to_owned()));
        }
        for k in PASSTHROUGH_ISUPPORT {
            if let Some(v) = uc.isupport.get(*k) {
                isupport.insert(k.to_string(), v.clone());
            }
        }
    }
    isupport
}

pub fn send_isupport(dc: &mut DownstreamConn, isupport: &Isupport) {
    let msgs = xirc::generate_isupport(isupport);
    if dc.caps.is_enabled("draft/extended-isupport") {
        dc.send_batch("draft/isupport", &[], msgs);
    } else {
        for m in msgs {
            dc.send_message(m);
        }
    }
}

pub fn update_supported_caps(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let net_idx = u.downstreams[i].network_id.and_then(|id| u.net_index(id));
    let uc = net_idx.and_then(|ni| u.networks[ni].upstream());
    let has_network = u.downstreams[i].network_id.is_some();
    let chathistory = u.has_chathistory_store();

    let supported: Vec<(&str, &str, bool)> = PASSTHROUGH_DOWNSTREAM_CAPS
        .iter()
        .map(|(cap, v)| (*cap, *v, uc.is_none_or(|uc| uc.caps.is_enabled(cap))))
        .collect();
    let sasl_plain = uc.is_some_and(|uc| uc.supports_sasl("PLAIN"));
    let multiline = uc.and_then(|uc| uc.multiline_limits());
    let account_registration = uc.and_then(|uc| {
        if uc.caps.is_enabled("draft/account-registration") {
            Some(
                uc.caps
                    .available
                    .get("draft/account-registration")
                    .cloned()
                    .unwrap_or_default(),
            )
        } else {
            None
        }
    });

    let dc = &mut u.downstreams[i];
    for (cap, v, ok) in supported {
        if ok {
            dc.set_supported_cap(cap, v);
        } else {
            dc.unset_supported_cap(cap);
        }
    }

    if sasl_plain {
        dc.set_supported_cap("sasl", "PLAIN,ANONYMOUS");
    } else if has_network {
        dc.unset_supported_cap("sasl");
    }

    match account_registration {
        Some(v) => {
            // Strip "before-connect", because we require downstreams to be
            // fully connected before attempting account registration.
            let mut values: Vec<&str> = v.split(',').collect();
            if let Some(pos) = values.iter().position(|v| *v == "before-connect") {
                values.remove(pos);
            }
            dc.set_supported_cap("draft/account-registration", &values.join(","));
        }
        None => dc.unset_supported_cap("draft/account-registration"),
    }

    match multiline {
        Some(l) => dc.set_supported_cap(xirc::MULTILINE_BATCH_TYPE, &l.cap_value()),
        None => dc.unset_supported_cap(xirc::MULTILINE_BATCH_TYPE),
    }

    if chathistory && has_network {
        dc.set_supported_cap("draft/event-playback", "");
    } else {
        dc.unset_supported_cap("draft/event-playback");
    }
}

pub fn update_isupport(u: &mut User, dc_id: u64) {
    let Some(i) = u.dc_index(dc_id) else { return };
    let dc = &u.downstreams[i];
    if !dc.registered && !dc.caps.is_enabled("draft/extended-isupport") {
        return;
    }
    let isupport = build_user_isupport(u, i);

    let mut changed = Isupport::new();
    for (k, v) in &isupport {
        if dc.isupport.get(k) == Some(v) {
            continue;
        }
        changed.insert(k.clone(), v.clone());
    }
    for k in dc.isupport.keys() {
        if !isupport.contains_key(k) {
            changed.insert(format!("-{k}"), None);
        }
    }

    let dc = &mut u.downstreams[i];
    send_isupport(dc, &changed);
    dc.isupport = isupport;
}
