//! Per-user state and event loop (port of upstream's user.go).
//!
//! Each user has one task owning all of the user's state: networks, their
//! upstream connections and downstream connections. Connection readers post
//! events to that task, so the dispatch logic never races.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::MsgStoreDriver;
use crate::database::{self as db, get_nick, get_realname};
use crate::downstream::{self, DownstreamConn, send_service_notice};
use crate::irc::{Message, Tags};
use crate::ircutil;
use crate::logger::Logger;
use crate::msgstore::{self, Store};
use crate::server::Server;
use crate::upstream::{self, ConnOutcome, UpstreamConn};
use crate::xirc::{self, CaseMapping, CaseMappingMap};
use crate::{logf, msg};

pub const RETRY_CONNECT_MIN_DELAY: Duration = Duration::from_secs(60);
pub const RETRY_CONNECT_MAX_DELAY: Duration = Duration::from_secs(10 * 60);
pub const RETRY_CONNECT_JITTER: Duration = Duration::from_secs(60);

/// Messages a user's event loop receives.
pub enum Event {
    /// A runner connected to an upstream server (not registered yet).
    UpstreamConnected {
        net_id: i64,
        runner_id: u64,
        conn: Box<upstream::NewConn>,
    },
    UpstreamMessage {
        net_id: i64,
        conn_id: u64,
        msg: Message,
    },
    /// The upstream connection's reader stopped.
    UpstreamClosed {
        net_id: i64,
        conn_id: u64,
        read_error: Option<String>,
    },
    UpstreamConnectionError {
        net_id: i64,
        runner_id: u64,
        err: String,
    },
    UpstreamRegistrationDelayDone {
        net_id: i64,
        conn_id: u64,
    },
    DownstreamConnected(Box<DownstreamConn>),
    DownstreamMessage {
        dc_id: u64,
        msg: Message,
    },
    DownstreamDisconnected {
        dc_id: u64,
    },
    ChannelDetach {
        net_id: i64,
        name: String,
    },
    TryRegainNick {
        net_id: i64,
        conn_id: u64,
        nick: String,
    },
    Broadcast(Message),
    Stop,
    UserUpdate {
        password: Option<String>,
        admin: Option<bool>,
        enabled: Option<bool>,
        max_networks: Option<i64>,
        done: oneshot::Sender<Result<(), String>>,
    },
    UserRun {
        params: Vec<String>,
        tx: mpsc::UnboundedSender<UserRunMsg>,
    },
}

pub enum UserRunMsg {
    Message(String),
    Done(Result<(), String>),
}

/// A handle to a running user, shared with the server.
#[derive(Clone)]
pub struct UserHandle {
    pub tx: mpsc::UnboundedSender<Event>,
    /// Cancelled when the user's event loop exits.
    pub done: CancellationToken,
    pub num_downstream_conns: Arc<AtomicI64>,
    /// A copy of the user record, kept in sync by the event loop.
    pub record: Arc<Mutex<db::User>>,
}

impl UserHandle {
    pub fn send(&self, ev: Event) -> bool {
        self.tx.send(ev).is_ok()
    }

    pub fn record(&self) -> db::User {
        self.record.lock().unwrap().clone()
    }

    /// Stops the user and waits for its event loop to exit.
    pub async fn stop(&self) {
        let _ = self.tx.send(Event::Stop);
        self.done.cancelled().await;
    }
}

/// Delivery receipts: target -> client name -> internal message ID.
pub struct DeliveredStore {
    pub m: CaseMappingMap<HashMap<String, String>>,
}

impl DeliveredStore {
    pub fn new(cm: CaseMapping) -> DeliveredStore {
        DeliveredStore {
            m: CaseMappingMap::new(cm),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.m.is_empty()
    }

    pub fn has_target(&self, target: &str) -> bool {
        self.m.has(target)
    }

    pub fn load_id(&self, target: &str, client: &str) -> String {
        self.m
            .get(target)
            .and_then(|c| c.get(client))
            .cloned()
            .unwrap_or_default()
    }

    pub fn store_id(&mut self, target: &str, client: &str, msg_id: &str) {
        if !self.m.has(target) {
            self.m.set(target, HashMap::new());
        }
        self.m
            .get_mut(target)
            .unwrap()
            .insert(client.to_owned(), msg_id.to_owned());
    }

    pub fn targets(&self) -> Vec<String> {
        self.m.keys().map(str::to_owned).collect()
    }

    pub fn clients(&self) -> Vec<String> {
        let mut clients: Vec<String> = Vec::new();
        for c in self.m.values() {
            for name in c.keys() {
                if !clients.contains(name) {
                    clients.push(name.clone());
                }
            }
        }
        clients
    }
}

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
    pub push_targets: Arc<Mutex<CaseMappingMap<DateTime<Utc>>>>,
    /// Shared with the runner, so it can pick up record changes (e.g. a new
    /// CertFP certificate) on reconnect.
    pub record_snapshot: Arc<Mutex<db::Network>>,
}

impl Network {
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
            casemap: ircutil_std_casemapping(),
            push_targets: Arc::new(Mutex::new(CaseMappingMap::new(cm))),
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

    /// Updates the record, and the snapshot shared with the runner.
    pub fn set_record(&mut self, record: db::Network) {
        *self.record_snapshot.lock().unwrap() = record.clone();
        self.record = record;
    }

    pub fn sync_snapshot(&self) {
        *self.record_snapshot.lock().unwrap() = self.record.clone();
    }

    /// Our current nickname on this network.
    pub fn nick(&self, user: &db::User) -> String {
        match self.upstream() {
            Some(uc) => uc.nick.clone(),
            None => get_nick(user, Some(&self.record)).to_owned(),
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
        // TODO: use case-mapping aware comparison here
        msg.prefix_name() != nick && ircutil::is_highlight(text, &nick)
    }

    pub fn detached_message_needs_relay(
        &self,
        user: &db::User,
        ch: &db::Channel,
        msg: &Message,
    ) -> bool {
        let highlight = self.is_highlight(user, msg);
        ch.relay_detached == db::MessageFilter::Message
            || ((ch.relay_detached == db::MessageFilter::Highlight
                || ch.relay_detached == db::MessageFilter::Default)
                && highlight)
    }

    /// Whether a nick is ours. If not connected, best-effort: use the
    /// configured nickname.
    pub fn is_our_nick(&self, user: &db::User, nick: &str) -> bool {
        match self.upstream() {
            Some(uc) => self.equal_casemap(&uc.nick, nick),
            None => self.equal_casemap(nick, get_nick(user, Some(&self.record))),
        }
    }
}

fn ircutil_std_casemapping() -> CaseMapping {
    xirc::CASEMAPPING_RFC1459
}

/// The ident sent to upstream servers. For privacy reasons, it doesn't
/// expose any meaningful user metadata: it's derived from a hash of the
/// user ID.
pub fn user_ident(u: &db::User) -> String {
    use sha2::Digest;
    let mut b = [0u8; 64];
    b[..8].copy_from_slice(&(u.id as u64).to_le_bytes());
    let h = sha2::Sha256::digest(b);
    hex::encode(&h[..16])
}

/// Returns the local address to use when connecting to a remote IP, for
/// per-user source addresses. None means the OS picks one.
pub fn local_tcp_addr(
    upstream_user_ips: &[ipnet::IpNet],
    user_id: i64,
    remote: IpAddr,
) -> Result<Option<IpAddr>> {
    let want_v6 = remote.to_canonical().is_ipv6();
    let Some(net) = upstream_user_ips
        .iter()
        .find(|n| n.addr().is_ipv6() == want_v6)
    else {
        return Ok(None);
    };
    let offset = (user_id + 1) as u128;
    let ip = match net.network() {
        IpAddr::V4(v4) => {
            let v = (u32::from(v4) as u128)
                .checked_add(offset)
                .filter(|v| *v <= u32::MAX as u128);
            v.map(|v| IpAddr::V4((v as u32).into()))
        }
        IpAddr::V6(v6) => u128::from(v6)
            .checked_add(offset)
            .map(|v| IpAddr::V6(v.into())),
    };
    match ip {
        Some(ip) if net.contains(&ip) => Ok(Some(ip)),
        _ => bail!("IP network {net} too small"),
    }
}

pub struct User {
    pub srv: Arc<Server>,
    pub record: db::User,
    pub logger: Logger,
    pub tx: mpsc::UnboundedSender<Event>,
    pub handle_record: Arc<Mutex<db::User>>,
    pub num_downstream_conns: Arc<AtomicI64>,

    pub networks: Vec<Network>,
    pub downstreams: Vec<DownstreamConn>,
    pub msg_store: Option<Store>,
    next_runner_id: u64,
    stopped: bool,
}

impl User {
    pub fn new(
        srv: Arc<Server>,
        record: db::User,
        tx: mpsc::UnboundedSender<Event>,
        handle: &UserHandle,
    ) -> User {
        let logger = srv
            .logger
            .with_prefix(format!("user {:?}: ", record.username));
        let cfg = srv.config();
        let msg_store = match cfg.basic.msg_store.driver {
            MsgStoreDriver::Fs => Some(Store::Fs(msgstore::FsStore::new(
                &cfg.basic.msg_store.source,
                &record,
            ))),
            MsgStoreDriver::Db => Some(Store::Db(msgstore::DbStore::new(srv.db.clone()))),
            MsgStoreDriver::Memory => Some(Store::Memory(msgstore::MemoryStore::new())),
        };
        User {
            srv,
            record,
            logger,
            tx,
            handle_record: handle.record.clone(),
            num_downstream_conns: handle.num_downstream_conns.clone(),
            networks: Vec::new(),
            downstreams: Vec::new(),
            msg_store,
            next_runner_id: 1,
            stopped: false,
        }
    }

    pub fn username(&self) -> &str {
        &self.record.username
    }

    pub fn net_index(&self, id: i64) -> Option<usize> {
        self.networks.iter().position(|n| n.id() == id)
    }

    pub fn dc_index(&self, id: u64) -> Option<usize> {
        self.downstreams.iter().position(|d| d.id == id)
    }

    /// Finds a network by address or name.
    pub fn get_network(&self, name: &str) -> Option<usize> {
        self.networks.iter().position(|n| {
            n.record.addr == name || (!n.record.name.is_empty() && n.record.name == name)
        })
    }

    pub fn has_persistent_msg_store(&self) -> bool {
        matches!(&self.msg_store, Some(s) if !s.is_memory())
    }

    pub fn has_chathistory_store(&self) -> bool {
        matches!(&self.msg_store, Some(s) if s.supports_chathistory())
    }

    pub fn format_server_time(&self, t: DateTime<Utc>) -> String {
        format_server_time(&self.msg_store, t)
    }

    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Event>) {
        let networks = match self.srv.db.list_networks(self.record.id) {
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
            let channels = match self.srv.db.list_channels(record.id) {
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
                match self.srv.db.list_delivery_receipts(network.id()) {
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
            self.handle_event(ev);
            if self.stopped {
                break;
            }
        }
        self.close_msg_store();
    }

    fn close_msg_store(&mut self) {
        if let Some(store) = &mut self.msg_store
            && let Err(e) = store.close()
        {
            logf!(
                self.logger,
                "failed to close message store for user {:?}: {e}",
                self.record.username
            );
        }
    }

    /// Starts the runner task of a network, if the user and network are
    /// enabled.
    fn start_network(&mut self, i: usize) {
        if !self.record.enabled || !self.networks[i].record.enabled {
            return;
        }
        let runner_id = self.next_runner_id;
        self.next_runner_id += 1;
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
            Event::ChannelDetach { net_id, name } => {
                let Some(ni) = self.net_index(net_id) else {
                    return;
                };
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
            Event::DownstreamDisconnected { dc_id } => {
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
            }
            Event::DownstreamMessage { dc_id, msg } => {
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
                        msg.to_string()
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
                let mut ctx = crate::service::ServiceContext::for_user(self.srv.clone(), true);
                let res = crate::service::handle_service_command(self, &mut ctx, &params);
                for line in ctx.take_prints() {
                    let _ = tx.send(UserRunMsg::Message(line));
                }
                let _ = tx.send(UserRunMsg::Done(res.map_err(|e| e.to_string())));
            }
            Event::Stop => {
                for dc in &self.downstreams {
                    dc.conn.close();
                }
                for ni in 0..self.networks.len() {
                    self.networks[ni].stop.cancel();
                    if let Some(uc) = &self.networks[ni].conn {
                        uc.conn.close();
                    }
                    for client in self.networks[ni].delivered.clients() {
                        self.store_client_delivery_receipts(ni, &client);
                    }
                }
                self.stopped = true;
            }
        }
    }

    fn downstream_connected(&mut self, mut dc: DownstreamConn) {
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

    /// Adds a network, keeping networks sorted by ID, and starts it.
    fn add_network(&mut self, network: Network) -> usize {
        let id = network.id();
        self.networks.push(network);
        self.networks.sort_by_key(|n| n.id());
        let i = self.net_index(id).unwrap();
        self.start_network(i);
        i
    }

    /// Stops and removes a network, closing the downstream connections
    /// bound to it.
    fn remove_network(&mut self, i: usize) -> Network {
        let net = self.networks.remove(i);
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

    pub fn can_enable_new_network(&self) -> Result<()> {
        let mut max = self.record.max_networks;
        if max < 0 {
            max = self.srv.config().basic.max_user_networks;
        }
        if max < 0 {
            return Ok(());
        }
        let n = self.networks.iter().filter(|n| n.record.enabled).count() as i64;
        if n >= max {
            bail!("maximum number of enabled networks reached");
        }
        Ok(())
    }

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
        Ok(self.net_index(id).unwrap())
    }

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
            .expect("tried updating a non-existing network");
        self.srv.db.store_network(self.record.id, &mut record)?;

        // Most network changes require us to re-connect to the upstream
        // server
        let channels: Vec<db::Channel> = self.networks[i].channels.values().cloned().collect();
        let mut updated = Network::new(&self.logger, record, channels);

        // If we're currently connected, disconnect and perform the
        // necessary bookkeeping
        self.networks[i].stop.cancel();
        if self.networks[i].upstream().is_some() {
            self.handle_upstream_disconnected(i);
        }
        if let Some(uc) = self.networks[i].conn.take() {
            uc.conn.close();
            uc.stop_timers();
        }

        // Remove the old network without closing downstream connections:
        // they keep using the network ID
        let old = self.networks.remove(i);

        // The filesystem message store needs to be notified whenever the
        // network is renamed
        if updated.get_name() != old.get_name()
            && let Some(store) = &mut self.msg_store
            && let Err(e) = store.rename_network(&old.record, &updated.record)
        {
            logf!(
                old.logger,
                "failed to update message store network name to {:?}: {e}",
                updated.get_name()
            );
        }

        // Transfer network attributes that aren't in the record
        let cm = old.casemap;
        updated.delivered = old.delivered;
        updated.push_targets = old.push_targets;
        if updated.record.enabled {
            updated.last_error = old.last_error;
        }
        updated.casemap = cm;
        updated.channels.set_casemapping(cm);
        updated.delivered.m.set_casemapping(cm);

        let id = updated.id();
        // This will re-connect to the upstream server
        let i = self.add_network(updated);
        for dc in self
            .downstreams
            .iter_mut()
            .filter(|d| d.network_id == Some(id))
        {
            dc.update_casemapping(self.networks[i].casemap);
        }

        // TODO: only broadcast attributes that have changed
        let attrs = self.network_attrs(i);
        self.notify_bouncer_network_state(id, &attrs);
        Ok(self.net_index(id).unwrap())
    }

    pub fn delete_network(&mut self, id: i64) -> Result<()> {
        let i = self
            .net_index(id)
            .expect("tried deleting a non-existing network");
        self.srv.db.delete_network(id)?;
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

    /// Updates the user record and applies side effects.
    pub fn update_user(&mut self, update: impl FnOnce(&mut db::User) -> Result<()>) -> Result<()> {
        let mut record = self.record.clone();
        update(&mut record)?;

        let nick_updated = self.record.nick != record.nick;
        let realname_updated = self.record.realname != record.realname;
        let enabled_updated = self.record.enabled != record.enabled;
        self.srv
            .db
            .store_user(&mut record)
            .map_err(|e| anyhow!("failed to update user {:?}: {e}", self.record.username))?;
        self.record = record;
        *self.handle_record.lock().unwrap() = self.record.clone();

        if nick_updated {
            for net in &mut self.networks {
                if !net.record.nick.is_empty() {
                    continue;
                }
                let nick = get_nick(&self.record, Some(&net.record)).to_owned();
                if let Some(uc) = net.upstream_mut() {
                    uc.send_message(msg!("NICK", nick));
                }
            }
        }

        if realname_updated || enabled_updated {
            // Re-connect to networks which use the default realname
            let mut need_update = Vec::new();
            for net in &mut self.networks {
                // If only the realname was updated, maybe we can skip the
                // re-connect
                if realname_updated && !enabled_updated {
                    // If this network has a custom realname set, no need to
                    // re-connect: the user-wide realname remains unused
                    if !net.record.realname.is_empty() {
                        continue;
                    }
                    // We only need to call update_network for upstreams that
                    // don't support setname
                    let realname = get_realname(&self.record, Some(&net.record)).to_owned();
                    if let Some(uc) = net.upstream_mut()
                        && uc.caps.is_enabled("setname")
                    {
                        uc.send_message(msg!("SETNAME", realname));
                        continue;
                    }
                }
                need_update.push(net.record.clone());
            }

            let mut net_err = None;
            for net in need_update {
                if let Err(e) = self.update_network(net, false) {
                    net_err = Some(e);
                }
            }
            if let Some(e) = net_err {
                return Err(e);
            }
        }

        if !self.record.enabled {
            // TODO: send an error message before disconnecting
            for dc in &self.downstreams {
                dc.conn.close();
            }
        }
        Ok(())
    }

    fn bump_downstream_interaction_time(&mut self) {
        if let Err(e) = self.update_user(|record| {
            record.downstream_interacted_at = Some(Utc::now());
            Ok(())
        }) {
            logf!(
                self.logger,
                "failed to bump downstream interaction time: {e}"
            );
        }
    }

    pub fn store_channel(&mut self, ni: usize, name: &str) {
        let net = &mut self.networks[ni];
        let net_id = net.id();
        if let Some(ch) = net.channels.get_mut(name)
            && let Err(e) = self.srv.db.store_channel(net_id, ch)
        {
            logf!(
                self.logger,
                "failed to store updated channel {:?}: {e}",
                ch.name
            );
        }
    }

    pub fn store_client_delivery_receipts(&mut self, ni: usize, client: &str) {
        if !self.has_persistent_msg_store() {
            return;
        }
        let net = &self.networks[ni];
        let mut receipts: Vec<db::DeliveryReceipt> = net
            .delivered
            .targets()
            .into_iter()
            .filter_map(|target| {
                let msg_id = net.delivered.load_id(&target, client);
                if msg_id.is_empty() {
                    return None;
                }
                Some(db::DeliveryReceipt {
                    target,
                    internal_msg_id: msg_id,
                    ..Default::default()
                })
            })
            .collect();
        if let Err(e) = self
            .srv
            .db
            .store_client_delivery_receipts(net.id(), client, &mut receipts)
        {
            logf!(
                net.logger,
                "failed to store delivery receipts for client {client:?}: {e}"
            );
        }
    }

    pub fn auto_save_sasl_plain(&mut self, ni: usize, username: &str, password: &str) {
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
        net.record.sasl.mechanism = "PLAIN".to_owned();
        net.record.sasl.plain_username = username.to_owned();
        net.record.sasl.plain_password = password.to_owned();
        net.sync_snapshot();
        if let Err(e) = self.srv.db.store_network(self.record.id, &mut net.record) {
            logf!(net.logger, "failed to save SASL PLAIN credentials: {e}");
        }
    }

    /// Detaches a channel.
    pub fn detach_channel(&mut self, ni: usize, name: &str) {
        let net = &mut self.networks[ni];
        let Some(ch) = net.channels.get_mut(name) else {
            return;
        };
        if ch.detached {
            return;
        }
        logf!(net.logger, "detaching channel {:?}", ch.name);
        ch.detached = true;
        let ch_name = ch.name.clone();

        if let Some(store) = &mut self.msg_store {
            let name_cm = (net.casemap)(&ch_name);
            match store.last_msg_id(&net.record, &name_cm, Utc::now()) {
                Ok(id) => net.channels.get_mut(name).unwrap().detached_internal_msg_id = id,
                Err(e) => {
                    logf!(
                        net.logger,
                        "failed to get last message ID for channel {ch_name:?}: {e}"
                    );
                    net.channels.get_mut(name).unwrap().detached_internal_msg_id = String::new();
                }
            }
        }

        if let Some(uc) = net.upstream_mut()
            && let Some(uch) = uc.channels.get_mut(&ch_name)
        {
            uch.update_auto_detach(None);
        }

        let net_id = net.id();
        for dc in dcs_for(&mut self.downstreams, net_id) {
            let prefix = dc.prefix();
            dc.send_message(msg!("PART", ch_name.clone(), "Detach").with_prefix(prefix));
        }
    }

    /// Attaches a detached channel.
    pub fn attach_channel(&mut self, ni: usize, name: &str) {
        let net = &mut self.networks[ni];
        let Some(ch) = net.channels.get_mut(name) else {
            return;
        };
        if !ch.detached {
            return;
        }
        logf!(net.logger, "attaching channel {:?}", ch.name);
        let detached_msg_id = std::mem::take(&mut ch.detached_internal_msg_id);
        ch.detached = false;
        let ch_name = ch.name.clone();

        let net_id = net.id();
        upstream::update_channel_auto_detach(self, ni, &ch_name);

        let dc_ids: Vec<u64> = dcs_for(&mut self.downstreams, net_id)
            .map(|d| d.id)
            .collect();
        for dc_id in dc_ids {
            let Some(i) = self.dc_index(dc_id) else {
                continue;
            };
            let dc = &mut self.downstreams[i];
            let prefix = dc.prefix();
            dc.send_message(msg!("JOIN", ch_name.clone()).with_prefix(prefix));

            let net = &self.networks[ni];
            if let Some(uc) = net.upstream()
                && let Some(uch) = uc.channels.get(&ch_name)
            {
                downstream::forward_channel(
                    &self.srv.db,
                    &mut self.downstreams[i],
                    uch,
                    net.id(),
                    net.casemap,
                );
            }

            if !detached_msg_id.is_empty() {
                downstream::send_target_backlog(self, i, ni, &ch_name, &detached_msg_id);
            }
        }
    }

    pub fn delete_channel(&mut self, ni: usize, name: &str) -> Result<()> {
        let net = &mut self.networks[ni];
        let Some(ch) = net.channels.get(name) else {
            bail!("unknown channel {name:?}");
        };
        let ch_id = ch.id;
        let ch_name = ch.name.clone();
        if let Some(uc) = net.upstream_mut()
            && let Some(uch) = uc.channels.get_mut(&ch_name)
        {
            uch.update_auto_detach(None);
        }
        self.srv.db.delete_channel(ch_id)?;
        self.networks[ni].channels.del(name);
        Ok(())
    }

    pub fn update_casemapping(&mut self, ni: usize, cm: CaseMapping) {
        let net = &mut self.networks[ni];
        net.casemap = cm;
        net.channels.set_casemapping(cm);
        net.delivered.m.set_casemapping(cm);
        net.push_targets.lock().unwrap().set_casemapping(cm);
        if let Some(uc) = &mut net.conn {
            uc.channels.set_casemapping(cm);
            for uch in uc.channels.values_mut() {
                uch.members.set_casemapping(cm);
            }
            uc.users.set_casemapping(cm);
            uc.monitored.set_casemapping(cm);
        }
        let net_id = net.id();
        for dc in dcs_for(&mut self.downstreams, net_id) {
            dc.update_casemapping(cm);
        }
    }

    /// Broadcasts a Web Push notification for a message, in the background.
    pub fn broadcast_web_push(&self, ni: usize, msg: Message) {
        let net = &self.networks[ni];
        spawn_web_push_broadcast(
            self.srv.clone(),
            self.record.id,
            net.id(),
            net.logger.clone(),
            msg,
        );
    }

    pub fn network_attrs(&self, ni: usize) -> Tags {
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
}

pub fn format_server_time(store: &Option<Store>, t: DateTime<Utc>) -> String {
    let mut t = t;
    if matches!(store, Some(s) if s.is_fs()) {
        // The FS message store truncates message timestamps to the second,
        // so truncate them here to get consistent timestamps.
        use chrono::Timelike;
        t = t.with_nanosecond(0).unwrap();
    }
    xirc::format_server_time(t)
}

/// The downstream connections bound to a network.
pub fn dcs_for(
    dcs: &mut [DownstreamConn],
    net_id: i64,
) -> impl Iterator<Item = &mut DownstreamConn> {
    dcs.iter_mut().filter(move |d| d.network_id == Some(net_id))
}

pub fn dc_ids_for(dcs: &[DownstreamConn], net_id: i64) -> Vec<u64> {
    dcs.iter()
        .filter(|d| d.network_id == Some(net_id))
        .map(|d| d.id)
        .collect()
}

pub fn spawn_web_push_broadcast(
    srv: Arc<Server>,
    user_id: i64,
    net_id: i64,
    logger: Logger,
    msg: Message,
) {
    tokio::spawn(async move {
        broadcast_web_push(srv, user_id, net_id, logger, msg).await;
    });
}

/// Sends a Web Push notification to all of a network's subscriptions.
pub async fn broadcast_web_push(
    srv: Arc<Server>,
    user_id: i64,
    net_id: i64,
    logger: Logger,
    msg: Message,
) {
    let Some(sender) = srv.web_push() else { return };
    let subs = match srv.db.list_web_push_subscriptions(user_id, net_id) {
        Ok(s) => s,
        Err(e) => {
            logf!(logger, "failed to list Web push subscriptions: {e}");
            return;
        }
    };
    for sub in subs {
        let res = send_web_push(&sender, &sub, &msg).await;
        match res {
            Ok(()) => {}
            Err(e) if e.is::<crate::webpush::SubscriptionExpired>() => {
                if let Err(e) = srv.db.delete_web_push_subscription(sub.id) {
                    logf!(
                        logger,
                        "failed to delete expired Web Push subscription {:?}: {e}",
                        sub.endpoint
                    );
                } else {
                    crate::debugf!(
                        logger,
                        "deleted expired Web Push subscription {:?}",
                        sub.endpoint
                    );
                }
            }
            Err(e) => {
                logf!(
                    logger,
                    "failed to send Web push notification to endpoint {:?}: {e}",
                    sub.endpoint
                );
                // If it failed for any reason and is old, delete it
                let old = sub
                    .updated_at
                    .is_none_or(|t| Utc::now() - t > chrono::Duration::days(30));
                if old {
                    if let Err(e) = srv.db.delete_web_push_subscription(sub.id) {
                        logf!(
                            logger,
                            "failed to delete pruned Web Push subscription {:?}: {e}",
                            sub.endpoint
                        );
                    } else {
                        logf!(
                            logger,
                            "deleted pruned Web Push subscription {:?}",
                            sub.endpoint
                        );
                    }
                }
            }
        }
    }
}

pub async fn send_web_push(
    sender: &crate::webpush::Sender,
    sub: &db::WebPushSubscription,
    msg: &Message,
) -> Result<()> {
    let urgency = match msg.command.as_str() {
        "PRIVMSG" | "NOTICE" | "INVITE" => crate::webpush::Urgency::High,
        _ => crate::webpush::Urgency::Normal,
    };
    sender
        .send(
            &sub.endpoint,
            &sub.keys.auth,
            &sub.keys.p256dh,
            &sub.keys.vapid,
            msg.to_string().as_bytes(),
            urgency,
        )
        .await
}

impl User {
    fn upstream_connected(&mut self, ni: usize, new: upstream::NewConn) {
        let net = &mut self.networks[ni];
        if let Some(old) = net.conn.take() {
            old.conn.close();
            old.stop_timers();
        }
        let uc = UpstreamConn::new(new, &net.logger, net.casemap);
        net.conn = Some(uc);
        upstream::register(self, ni);
    }

    fn upstream_message(&mut self, net_id: i64, conn_id: u64, msg: Message) {
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

    fn upstream_closed(&mut self, net_id: i64, conn_id: u64, read_error: Option<String>) {
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

    fn try_regain_nick(&mut self, ni: usize, nick: &str) {
        upstream::try_regain_nick(self, ni, nick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addr() {
        let nets: Vec<ipnet::IpNet> = vec![
            "192.168.0.0/30".parse().unwrap(),
            "fd00::/64".parse().unwrap(),
        ];
        assert_eq!(
            local_tcp_addr(&nets, 0, "1.2.3.4".parse().unwrap()).unwrap(),
            Some("192.168.0.1".parse().unwrap())
        );
        assert!(local_tcp_addr(&nets, 5, "1.2.3.4".parse().unwrap()).is_err());
        assert_eq!(
            local_tcp_addr(&nets, 9, "::1".parse().unwrap()).unwrap(),
            Some("fd00::a".parse().unwrap())
        );
        assert_eq!(
            local_tcp_addr(&nets[..1], 0, "::1".parse().unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn ident() {
        let mut u = db::User::new("a");
        u.id = 1;
        assert_eq!(user_ident(&u).len(), 32);
    }
}
