//! Per-user state and event loop (port of upstream's user.go).
//!
//! Each user has one task owning all of the user's state: networks, their
//! upstream connections and downstream connections. Connection readers post
//! events to that task, so the dispatch logic never races.

use std::net::IpAddr;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::MsgStoreDriver;
use crate::database::{self as db, get_nick, get_realname};
use crate::downstream::DownstreamConn;
use crate::irc::Message;
use crate::logger::Logger;
use crate::msgstore::{self, Store};
use crate::server::Server;
use crate::upstream;
use crate::{logf, msg};

mod channels;
mod delivered;
mod events;
mod network;
mod upstream_conn;
mod webpush;

pub use delivered::DeliveredStore;
pub use network::{Network, reattaches_detached_channel, relays_detached_message};
pub use webpush::{send_web_push, send_web_push_to, web_push_subscriptions};

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
        conn_id: u64,
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
    /// A line of output of a deferred BouncerServ command run by a
    /// downstream.
    ServiceReply {
        dc_id: u64,
        text: String,
    },
    /// Asks whether the user has a network, by address or name.
    HasNetwork {
        name: String,
        reply: oneshot::Sender<bool>,
    },
    /// The test notification of a new Web Push subscription was sent.
    WebPushTested {
        dc_id: u64,
        label: String,
        network_id: i64,
        sub: db::WebPushSubscription,
        res: Result<(), String>,
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
    pub msg_store: Store,
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
            MsgStoreDriver::Db => Store::Db(msgstore::DbStore::new(srv.db.clone())),
            MsgStoreDriver::Memory => Store::Memory(msgstore::MemoryStore::new()),
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
        !self.msg_store.is_memory()
    }

    pub fn has_chathistory_store(&self) -> bool {
        self.msg_store.supports_chathistory()
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
            for dc in &mut self.downstreams {
                dc.send_message(msg!("ERROR", "This bouncer account is disabled"));
                dc.conn.shutdown();
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
}

/// The downstream connections bound to a network.
pub fn dcs_for(
    dcs: &mut [DownstreamConn],
    net_id: i64,
) -> impl Iterator<Item = &mut DownstreamConn> {
    dcs.iter_mut().filter(move |d| d.network_id == Some(net_id))
}

/// Runs the rest of a BouncerServ command run by a downstream, replying to
/// it through the user's event loop.
pub fn spawn_deferred_service_command(
    tx: mpsc::UnboundedSender<Event>,
    dc_id: u64,
    deferred: crate::service::Deferred,
) {
    tokio::spawn(async move {
        let print_tx = tx.clone();
        let print = Box::new(move |text| {
            let _ = print_tx.send(Event::ServiceReply { dc_id, text });
        });
        if let Err(e) = deferred(print).await {
            let text = format!("error: {e}");
            let _ = tx.send(Event::ServiceReply { dc_id, text });
        }
    });
}

pub fn dc_ids_for(dcs: &[DownstreamConn], net_id: i64) -> Vec<u64> {
    dcs.iter()
        .filter(|d| d.network_id == Some(net_id))
        .map(|d| d.id)
        .collect()
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
    fn highlight() {
        let mut user = db::User::new("alice");
        user.nick = "Alice".to_owned();
        let net = Network::new(
            &Logger::new(false),
            db::Network::new("irc.example.org"),
            vec![],
        );
        let msg = |from: &str, text: &str| {
            msg!("PRIVMSG", "#chan", text).with_prefix(crate::irc::Prefix::new(from))
        };
        assert!(net.is_highlight(&user, &msg("bob", "hi Alice")));
        assert!(!net.is_highlight(&user, &msg("bob", "hi")));
        // Our own messages aren't highlights, whatever the case
        assert!(!net.is_highlight(&user, &msg("ALICE", "hi Alice")));
    }

    #[test]
    fn ident() {
        let mut u = db::User::new("a");
        u.id = 1;
        assert_eq!(user_ident(&u).len(), 32);
    }
}
