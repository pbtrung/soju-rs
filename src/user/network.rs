//! A user's networks: their state and lifecycle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use jiff::Timestamp;
use tokio_util::sync::CancellationToken;

use super::{DeliveredStore, User, user_ident};
use crate::database::{self as db, get_nick, get_realname};
use crate::downstream;
use crate::irc::{Message, Tags};
use crate::ircutil;
use crate::logger::Logger;
use crate::upstream::{self, UpstreamConn};
use crate::xirc::{self, CaseMapping, CaseMappingMap};
use crate::{logf, msg};

pub struct Network {
    pub record: db::Network,
    /// Identifies the runner task of this network instance. Events from
    /// stale runners are ignored.
    pub runner_id: u64,
    pub stop: CancellationToken,
    pub logger: Logger,
    /// The upstream connection, including while it registers. Use
    /// [`Network::upstream`] to get it only once it's fully connected.
    pub conn: Option<UpstreamConn>,
    pub channels: CaseMappingMap<db::Channel>,
    pub delivered: DeliveredStore,
    pub last_error: Option<String>,
    pub casemap: CaseMapping,
    pub push_targets: Arc<Mutex<CaseMappingMap<Timestamp>>>,
    /// Shared with the runner, so it can pick up record changes (e.g. a new
    /// `CertFP` certificate) on reconnect.
    pub record_snapshot: Arc<Mutex<db::Network>>,
    /// The pinned, muted or blocked targets, by stored (case-mapped) name.
    /// The user's event loop is their only writer: incoming messages check
    /// them without a database query.
    pub message_targets: HashMap<String, db::MessageTarget>,
    /// Mentions to store, by conversation, see `upstream::record_mention`
    pub pending_mentions: Arc<Mutex<HashMap<String, Timestamp>>>,
}

impl Network {
    #[must_use]
    pub fn new(user_logger: &Logger, record: db::Network, channels: Vec<db::Channel>) -> Network {
        let logger = user_logger.with_prefix(format!("network {:?}: ", record.get_name()));

        // Initialize maps with the most strict case-mapping to avoid
        // collisions: we don't know which case-mapping will be used by the
        // upstream server yet
        let cm = xirc::CASEMAPPING_ASCII;
        let mut m = CaseMappingMap::new(cm);
        for ch in channels {
            let name = ch.name.clone();
            m.set(&name, ch);
        }

        Network {
            record_snapshot: Arc::new(Mutex::new(record.clone())),
            record,
            runner_id: 0,
            stop: CancellationToken::new(),
            logger,
            conn: None,
            channels: m,
            delivered: DeliveredStore::new(cm),
            last_error: None,
            casemap: xirc::CASEMAPPING_RFC1459,
            push_targets: Arc::new(Mutex::new(CaseMappingMap::new(cm))),
            message_targets: HashMap::new(),
            pending_mentions: Arc::default(),
        }
    }

    pub fn id(&self) -> i64 {
        self.record.id
    }

    pub fn get_name(&self) -> &str {
        self.record.get_name()
    }

    /// The upstream connection, if registered and fully connected.
    pub fn upstream(&self) -> Option<&UpstreamConn> {
        self.conn.as_ref().filter(|uc| uc.attached)
    }

    pub fn upstream_mut(&mut self) -> Option<&mut UpstreamConn> {
        self.conn.as_mut().filter(|uc| uc.attached)
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.is_cancelled()
    }

    pub fn casemap(&self, s: &str) -> String {
        (self.casemap)(s)
    }

    pub fn equal_casemap(&self, a: &str, b: &str) -> bool {
        (self.casemap)(a) == (self.casemap)(b)
    }

    /// Whether a saved channel is detached.
    pub fn is_detached(&self, channel: &str) -> bool {
        self.channels.get(channel).is_some_and(|c| c.detached)
    }

    pub fn sync_snapshot(&self) {
        *crate::state_lock::lock(&self.record_snapshot) = self.record.clone();
    }

    /// Our current nickname on this network.
    pub fn nick<'a>(&'a self, user: &'a db::User) -> &'a str {
        match self.upstream() {
            Some(uc) => &uc.nick,
            None => get_nick(user, Some(&self.record)),
        }
    }

    /// A target's flags, all unset if it has none.
    pub fn message_target(&self, target_cm: &str) -> db::MessageTarget {
        self.message_targets
            .get(target_cm)
            .cloned()
            .unwrap_or_else(|| db::MessageTarget {
                target: target_cm.to_owned(),
                ..Default::default()
            })
    }

    /// The pinned, muted or blocked targets, in name order.
    pub fn flagged_targets(&self) -> Vec<db::MessageTarget> {
        let mut l: Vec<_> = self.message_targets.values().cloned().collect();
        l.sort_by(|a, b| a.target.cmp(&b.target));
        l
    }

    /// Records a target's flags once stored.
    pub fn set_message_target(&mut self, mt: db::MessageTarget) {
        if mt.pinned || mt.muted || mt.blocked {
            self.message_targets.insert(mt.target.clone(), mt);
        } else {
            self.message_targets.remove(&mt.target);
        }
    }

    pub fn is_highlight(&self, user: &db::User, msg: &Message) -> bool {
        if msg.command != "PRIVMSG" && msg.command != "NOTICE" {
            return false;
        }
        let text = msg.param(1);
        let nick = self.nick(user);
        if nick.is_empty() {
            return false;
        }
        !self.equal_casemap(msg.prefix_name(), nick) && ircutil::is_highlight(text, nick)
    }

    pub fn detached_message_needs_relay(
        &self,
        user: &db::User,
        ch: &db::Channel,
        msg: &Message,
    ) -> bool {
        relays_detached_message(ch, self.is_highlight(user, msg))
    }
}

/// Whether a message of a detached channel is relayed to downstreams,
/// given whether it's a highlight.
#[must_use]
pub fn relays_detached_message(ch: &db::Channel, highlight: bool) -> bool {
    match ch.relay_detached {
        db::MessageFilter::Message => true,
        db::MessageFilter::Highlight | db::MessageFilter::Default => highlight,
        db::MessageFilter::None => false,
    }
}

/// Whether a message reattaches a detached channel, given whether it's a
/// highlight.
#[must_use]
pub fn reattaches_detached_channel(ch: &db::Channel, highlight: bool) -> bool {
    match ch.reattach_on {
        db::MessageFilter::Message => true,
        db::MessageFilter::Highlight => highlight,
        db::MessageFilter::Default | db::MessageFilter::None => false,
    }
}

impl Network {
    /// Whether a nick is ours. If not connected, best-effort: use the
    /// configured nickname.
    pub fn is_our_nick(&self, user: &db::User, nick: &str) -> bool {
        match self.upstream() {
            Some(uc) => self.equal_casemap(&uc.nick, nick),
            None => self.equal_casemap(nick, get_nick(user, Some(&self.record))),
        }
    }
}

impl User {
    /// Starts the runner task of a network, if the user and network are
    /// enabled.
    pub(super) fn start_network(&mut self, i: usize) {
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            if !self.record.enabled || !self.networks[i].record.enabled {
                return;
            }
        }
        let runner_id = self.next_runner_id;
        self.next_runner_id += 1;
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let net = &mut self.networks[i];
        net.runner_id = runner_id;
        tokio::spawn(upstream::run_network(upstream::RunnerParams {
            srv: self.srv.clone(),
            tx: self.tx.clone(),
            user_id: self.record.id,
            user_ident: user_ident(&self.record),
            record: net.record_snapshot.clone(),
            runner_id,
            stop: net.stop.clone(),
            logger: net.logger.clone(),
        }));
    }

    pub fn notify_bouncer_network_state(&mut self, net_id: i64, attrs: &Tags) {
        // Don't send state updates for removed networks
        if self.net_index(net_id).is_none() {
            return;
        }
        let id = net_id.to_string();
        for dc in &mut self.downstreams {
            if dc.caps.is_enabled("soju.im/bouncer-networks-notify") {
                dc.send_message(msg!("BOUNCER", "NETWORK", id.clone(), attrs.to_string()));
            }
        }
    }

    /// Attaches the per-user durable receipt budget before the first delivery.
    pub(super) fn prepare_receipts(&self, network: &mut Network) {
        if self.has_persistent_msg_store() {
            network.delivered.attach(
                self.srv.db.clone(),
                network.id(),
                self.receipt_budget.clone(),
            );
        }
    }

    /// Adds a network, keeping networks sorted by ID, and starts it.
    fn add_network(&mut self, mut network: Network) -> usize {
        self.prepare_receipts(&mut network);
        let id = network.id();
        self.networks.push(network);
        self.networks.sort_by_key(Network::id);
        let i = self.networks.partition_point(|network| network.id() < id);
        self.start_network(i);
        i
    }

    /// Stops and removes a network, closing the downstream connections
    /// bound to it.
    fn remove_network(&mut self, i: usize) -> Network {
        let net = self.networks.remove(i);
        self.notifications.cancel_network(net.id());
        net.stop.cancel();
        if let Some(uc) = &net.conn {
            uc.conn.close();
        }
        for dc in &self.downstreams {
            if dc.network_id == Some(net.id()) {
                dc.conn.close();
            }
        }
        net
    }

    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn can_enable_new_network(&self) -> Result<()> {
        let mut max = self.record.max_networks;
        if max < 0 {
            max = self.srv.config().basic.max_user_networks;
        }
        if max < 0 {
            return Ok(());
        }
        let n = i64::try_from(self.networks.iter().filter(|n| n.record.enabled).count())?;
        if n >= max {
            bail!("maximum number of enabled networks reached");
        }
        Ok(())
    }

    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn check_network(&self, record: &db::Network) -> Result<()> {
        let url = record.url()?;
        if url.has_user {
            bail!(
                "{}:// URL must not have username and password information",
                url.scheme
            );
        }
        if !url.query.is_empty() {
            bail!("{}:// URL must not have query values", url.scheme);
        }
        if !url.fragment.is_empty() {
            bail!("{}:// URL must not have a fragment", url.scheme);
        }
        match url.scheme.as_str() {
            "ircs" | "irc+insecure" => {
                if url.host.is_empty() {
                    bail!("{}:// URL must have a host", url.scheme);
                }
                if !url.path.is_empty() {
                    bail!("{}:// URL must not have a path", url.scheme);
                }
            }
            "irc+unix" | "unix" => {
                if !self.record.admin {
                    bail!("non-admin users cannot add networks with a unix address");
                }
                if !url.host.is_empty() {
                    bail!("{}:// URL must not have a host", url.scheme);
                }
                if url.path.is_empty() {
                    bail!("{}:// URL must have a path", url.scheme);
                }
            }
            s => bail!("unknown URL scheme {s:?}"),
        }

        if record.get_name().is_empty() {
            bail!("network name cannot be empty");
        }
        if record.get_name().starts_with('-') {
            // Can be mixed up with flags when sending commands to the service
            bail!("network name cannot start with a dash character");
        }
        for net in &self.networks {
            if net.get_name() == record.get_name() && net.id() != record.id {
                bail!(
                    "a network with the name {:?} already exists",
                    record.get_name()
                );
            }
        }
        Ok(())
    }

    ///
    /// # Panics
    ///
    /// Panics only if the documented fixed-data invariant is violated; operational errors are handled separately.
    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn create_network(
        &mut self,
        mut record: db::Network,
        enforce_limit: bool,
    ) -> Result<usize> {
        assert_eq!(record.id, 0, "tried creating an already-existing network");
        if enforce_limit && record.enabled {
            self.can_enable_new_network()?;
        }
        self.check_network(&record)?;
        self.srv.db.store_network(self.record.id, &mut record)?;
        let network = Network::new(&self.logger, record, Vec::new());
        let id = network.id();
        let i = self.add_network(network);
        let attrs = self.network_attrs(i);
        self.notify_bouncer_network_state(id, &attrs);
        self.net_index(id)
            .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))
    }

    ///
    /// # Panics
    ///
    /// Panics only if the documented fixed-data invariant is violated; operational errors are handled separately.
    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn update_network(
        &mut self,
        mut record: db::Network,
        enforce_limit: bool,
    ) -> Result<usize> {
        assert_ne!(record.id, 0, "tried updating a new network");
        if enforce_limit && record.enabled {
            self.can_enable_new_network()?;
        }

        // If the nickname/realname is reset to the default, just wipe the
        // per-network setting
        if record.nick == self.record.nick {
            record.nick.clear();
        }
        if record.realname == self.record.realname {
            record.realname.clear();
        }

        self.check_network(&record)?;
        let i = self
            .net_index(record.id)
            .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
        self.srv.db.store_network(self.record.id, &mut record)?;

        // Most network changes require us to re-connect to the upstream
        // server
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let channels: Vec<db::Channel> = self.networks[i].channels.values().cloned().collect();
        let mut updated = Network::new(&self.logger, record, channels);

        // If we're currently connected, disconnect and perform the
        // necessary bookkeeping
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            self.networks[i].stop.cancel();
        }
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            if self.networks[i].upstream().is_some() {
                self.handle_upstream_disconnected(i);
            }
        }
        {
            #![expect(
                clippy::indexing_slicing,
                reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
            )]
            if let Some(uc) = self.networks[i].conn.take() {
                uc.conn.close();
                uc.stop_timers();
            }
        }

        // Remove the old network without closing downstream connections:
        // they keep using the network ID
        let old = self.networks.remove(i);

        // Transfer network attributes that aren't in the record
        let cm = old.casemap;
        updated.delivered = old.delivered;
        updated.push_targets = old.push_targets;
        updated.message_targets = old.message_targets;
        updated.pending_mentions = old.pending_mentions;
        if updated.record.enabled {
            updated.last_error = old.last_error;
        }
        updated.casemap = cm;
        updated.channels.set_casemapping(cm);

        let id = updated.id();
        // This will re-connect to the upstream server
        let i = self.add_network(updated);
        for dc in self
            .downstreams
            .iter_mut()
            .filter(|d| d.network_id == Some(id))
        {
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
                )]
                dc.update_casemapping(self.networks[i].casemap);
            }
        }

        // TODO: only broadcast attributes that have changed
        let attrs = self.network_attrs(i);
        self.notify_bouncer_network_state(id, &attrs);
        self.net_index(id)
            .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))
    }

    ///
    /// # Errors
    ///
    /// Returns an error if requested state is unavailable, input is invalid, or an underlying operation fails.
    pub fn delete_network(&mut self, id: i64) -> Result<()> {
        let i = self
            .net_index(id)
            .ok_or_else(|| anyhow::anyhow!("required user or connection state is unavailable"))?;
        self.srv.db.delete_network(id)?;
        self.msg_store.delete_network(id);
        let net = self.remove_network(i);
        if let Some(uc) = &net.conn {
            uc.stop_timers();
        }
        let id_str = id.to_string();
        for dc in &mut self.downstreams {
            if dc.caps.is_enabled("soju.im/bouncer-networks-notify") {
                dc.send_message(msg!("BOUNCER", "NETWORK", id_str.clone(), "*"));
            }
        }
        Ok(())
    }

    pub fn auto_save_sasl_plain(&mut self, ni: usize, username: &str, password: &str) {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let net = &mut self.networks[ni];
        // User may have e.g. EXTERNAL mechanism configured. We do not want to
        // automatically erase the key pair or any other credentials.
        if !net.record.sasl.mechanism.is_empty() && net.record.sasl.mechanism != "PLAIN" {
            return;
        }
        logf!(
            net.logger,
            "auto-saving SASL PLAIN credentials with username {username:?}"
        );
        "PLAIN".clone_into(&mut net.record.sasl.mechanism);
        username.clone_into(&mut net.record.sasl.plain_username);
        password.clone_into(&mut net.record.sasl.plain_password);
        net.sync_snapshot();
        if let Err(e) = self.srv.db.store_network(self.record.id, &mut net.record) {
            logf!(net.logger, "failed to save SASL PLAIN credentials: {e}");
        }
    }

    #[must_use]
    pub fn network_attrs(&self, ni: usize) -> Tags {
        #[expect(
            clippy::indexing_slicing,
            reason = "Actor indices are resolved by lookup or bounded iteration and stay stable during this synchronous handler."
        )]
        let net = &self.networks[ni];
        let state = if net.upstream().is_some() {
            "connected"
        } else {
            "disconnected"
        };
        let mut attrs = Tags::new();
        attrs.set("name", net.get_name());
        attrs.set("state", state);
        attrs.set("nickname", get_nick(&self.record, Some(&net.record)));
        if !net.record.username.is_empty() {
            attrs.set("username", net.record.username.clone());
        }
        let realname = get_realname(&self.record, Some(&net.record));
        if !realname.is_empty() {
            attrs.set("realname", realname);
        }
        if let Some(err) = &net.last_error {
            attrs.set("error", err.clone());
        }
        downstream::fill_network_addr_attrs(&mut attrs, &net.record);
        attrs
    }
}
