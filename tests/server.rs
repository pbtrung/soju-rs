//! Integration tests ported from upstream's server_test.go: an in-process
//! bouncer with a fake upstream server.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use soju::config::{MsgStore, MsgStoreDriver};
use soju::conn::{Conn, ConnInfo, ConnReader};
use soju::database::{self, Database};
use soju::irc::{Message, Prefix};
use soju::logger::Logger;
use soju::msg;
use soju::server::{Server, ServerConfig};
use soju::xirc;
use tokio::net::TcpListener;

/// Runs each test on both runtimes soju may run on (see bin/soju.rs): the
/// current-thread one has no other worker to run tasks while one blocks.
macro_rules! test_on_both_runtimes {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                #[tokio::test(flavor = "multi_thread")]
                async fn multi_thread() {
                    super::$name().await
                }

                #[tokio::test(flavor = "current_thread")]
                async fn current_thread() {
                    super::$name().await
                }
            }
        )*
    };
}

test_on_both_runtimes!(
    broadcast,
    chat_history_db,
    statusmsg_keeps_sigil,
    upstream_malformed_messages,
    cap_req_removal,
    labeled_response_ack,
    sasl_errors,
    sasl_chunk_boundaries,
    disabled_admin_impersonation,
    setname_survives_network_update,
    chat_history_params,
    read_marker,
    upstream_clock_skew,
    multiline,
    service_other_user,
    chat_history_limit,
    unread_targets,
    blocked_users,
    unread_replies_before_registration,
    multiplex_not_attached_label,
    service_blocking_work,
    monitor_invalid_targets,
    multiplex,
    multiplex_unsupported,
    user_network_channel_isolation,
    disconnected_client_history,
    sqlite_history_survives_restart,
);

const TEST_USERNAME: &str = "soju-test-user";
const TEST_PASSWORD: &str = TEST_USERNAME;
const TIMEOUT: Duration = Duration::from_secs(10);

fn server_prefix() -> Prefix {
    Prefix::new("soju-test-server")
}

struct TestConn {
    conn: Conn,
    reader: ConnReader,
}

impl TestConn {
    fn new(io: soju::conn::BoxIo) -> TestConn {
        let (conn, reader) = Conn::new_stream(io, Logger::new(false), None);
        TestConn { conn, reader }
    }

    fn send(&self, msg: Message) {
        self.conn.send_message(msg);
    }

    async fn read(&mut self) -> Message {
        tokio::time::timeout(TIMEOUT, self.reader.read_message())
            .await
            .expect("timed out reading a message")
            .expect("failed to read a message")
            .expect("connection closed")
    }

    async fn expect(&mut self, cmd: &str) -> Message {
        let msg = self.read().await;
        assert_eq!(msg.command, cmd, "invalid message received: {msg}");
        msg
    }

    /// Sends a PING and returns the messages received until the PONG,
    /// acknowledging server PINGs like a real client (delivery receipts).
    async fn roundtrip(&mut self) -> Vec<Message> {
        self.send(msg!("PING", "roundtrip"));
        let mut msgs = Vec::new();
        loop {
            let msg = self.read().await;
            if msg.command == "PONG" {
                return msgs;
            }
            if msg.command == "PING" {
                let mut pong = Message::new("PONG", msg.params);
                pong.tags = msg.tags;
                self.send(pong);
                continue;
            }
            msgs.push(msg);
        }
    }
}

struct Env {
    srv: Arc<Server>,
    network: database::Network,
    upstream: TcpListener,
}

async fn setup(msg_store: MsgStore) -> Env {
    setup_with(msg_store, |_, _| {}).await
}

/// Like setup, with f customizing the test user and database first.
async fn setup_with(msg_store: MsgStore, f: impl FnOnce(&Database, &mut database::User)) -> Env {
    let db = Database::open_in_memory().unwrap();
    let mut user = database::User::new(TEST_USERNAME);
    user.set_password(TEST_PASSWORD).unwrap();
    f(&db, &mut user);
    db.store_user(&mut user).unwrap();

    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut network = database::Network::new(&format!(
        "irc+insecure://{}",
        upstream.local_addr().unwrap()
    ));
    network.name = "testnet".to_owned();
    db.store_network(user.id, &mut network).unwrap();

    let srv = Server::new(db, Logger::new(false));
    let mut cfg = ServerConfig::default();
    cfg.basic.msg_store = msg_store;
    srv.set_config(cfg);
    srv.start().unwrap();
    Env {
        srv,
        network,
        upstream,
    }
}

async fn accept_upstream(env: &Env) -> TestConn {
    let (s, _) = tokio::time::timeout(TIMEOUT, env.upstream.accept())
        .await
        .expect("timed out waiting for the upstream connection")
        .unwrap();
    TestConn::new(Box::new(s))
}

fn connect_downstream(srv: &Arc<Server>) -> TestConn {
    let (a, b) = tokio::io::duplex(1 << 16);
    let logger = Logger::new(false);
    let (conn, reader) = Conn::new_stream(Box::new(a), logger.clone(), None);
    let info = ConnInfo {
        remote_addr: "127.0.0.1:1234".to_owned(),
        ..Default::default()
    };
    tokio::spawn(soju::downstream::serve(
        srv.clone(),
        conn,
        reader,
        info,
        logger,
    ));
    TestConn::new(Box::new(b))
}

async fn register_downstream(c: &mut TestConn, network: &database::Network) {
    register_downstream_as(c, TEST_USERNAME, network).await;
}

async fn register_downstream_as(c: &mut TestConn, username: &str, network: &database::Network) {
    c.send(msg!("PASS", TEST_PASSWORD));
    c.send(msg!("NICK", TEST_USERNAME));
    c.send(msg!(
        "USER",
        format!("{username}/{}", network.name),
        "0",
        "*",
        TEST_USERNAME
    ));
    c.expect("001").await;
}

async fn register_upstream(c: &mut TestConn) {
    let msg = c.expect("CAP").await;
    assert_eq!(msg.params[0], "LS", "invalid CAP LS: {msg}");
    let msg = c.expect("NICK").await;
    let nick = msg.params[0].clone();
    assert_eq!(nick, TEST_USERNAME);
    c.expect("USER").await;

    let p = server_prefix();
    c.send(msg!("001", nick.clone(), "Welcome!").with_prefix(p.clone()));
    c.send(msg!("002", nick.clone(), "Your host is soju-test-server").with_prefix(p.clone()));
    c.send(
        msg!(
            "003",
            nick.clone(),
            "Who cares when the server was created?"
        )
        .with_prefix(p.clone()),
    );
    c.send(
        msg!(
            "004",
            nick.clone(),
            p.name.clone(),
            "soju",
            "aiwroO",
            "OovaimnqpsrtklbeI"
        )
        .with_prefix(p.clone()),
    );
    c.send(msg!("422", nick, "No MOTD").with_prefix(p));
}

/// Registers an upstream connection like register_upstream, advertising
/// ISUPPORT tokens.
async fn register_upstream_with(c: &mut TestConn, isupport: &[&str]) -> String {
    c.expect("CAP").await;
    let nick = c.expect("NICK").await.params[0].clone();
    c.expect("USER").await;
    let p = server_prefix();
    c.send(msg!("001", nick.clone(), "Welcome!").with_prefix(p.clone()));
    let mut params = vec![nick.clone()];
    params.extend(isupport.iter().map(|s| s.to_string()));
    params.push("are supported".to_owned());
    c.send(Message::new("005", params).with_prefix(p.clone()));
    c.send(msg!("422", nick.clone(), "No MOTD").with_prefix(p));
    nick
}

/// Registers a downstream connection after negotiating capabilities, bound
/// to a network or else to the bouncer itself. Returns the messages received
/// after RPL_WELCOME.
async fn register_downstream_caps(
    c: &mut TestConn,
    network: Option<&database::Network>,
    caps: &str,
) -> Vec<Message> {
    c.send(msg!("CAP", "LS", "302"));
    c.send(msg!("CAP", "REQ", caps));
    let ack = loop {
        let m = c.expect("CAP").await;
        if m.params[1] != "LS" {
            break m;
        }
    };
    assert_eq!(ack.params[1], "ACK", "{ack}");
    c.send(msg!("PASS", TEST_PASSWORD));
    c.send(msg!("NICK", TEST_USERNAME));
    let username = match network {
        Some(n) => format!("{TEST_USERNAME}/{}", n.name),
        None => TEST_USERNAME.to_owned(),
    };
    c.send(msg!("USER", username, "0", "*", TEST_USERNAME));
    c.send(msg!("CAP", "END"));
    // cap-notify may remove caps the upstream doesn't support first
    read_until(c, "001").await;
    c.roundtrip().await
}

/// Reads a CAP LS 302 reply, possibly split over several lines, each of
/// which must fit in an IRC message. Returns the capabilities.
async fn read_cap_ls(c: &mut TestConn) -> Vec<String> {
    let mut caps = Vec::new();
    loop {
        let m = c.expect("CAP").await;
        assert_eq!(m.params[1], "LS", "{m}");
        assert!(m.to_string().len() + 2 <= 512, "CAP LS line too long: {m}");
        caps.extend(m.params.last().unwrap().split(' ').map(str::to_owned));
        if m.params[2] != "*" {
            return caps;
        }
    }
}

/// Reads messages until one with the given command.
async fn read_until(c: &mut TestConn, cmd: &str) -> Message {
    loop {
        let m = c.read().await;
        if m.command == cmd {
            return m;
        }
    }
}

fn db_store() -> MsgStore {
    MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    }
}

/// Registers an upstream connection like register_upstream, negotiating
/// capabilities.
async fn register_upstream_caps(c: &mut TestConn, caps: &str) -> String {
    let p = server_prefix();
    c.expect("CAP").await;
    let nick = c.expect("NICK").await.params[0].clone();
    c.expect("USER").await;
    c.send(msg!("CAP", "*", "LS", caps).with_prefix(p.clone()));
    let req = c.expect("CAP").await;
    assert_eq!(req.params[0], "REQ", "{req}");
    c.send(msg!("CAP", nick.clone(), "ACK", req.params[1].clone()).with_prefix(p.clone()));
    let end = c.expect("CAP").await;
    assert_eq!(end.params[0], "END", "{end}");
    c.send(msg!("001", nick.clone(), "Welcome!").with_prefix(p.clone()));
    c.send(msg!("422", nick.clone(), "No MOTD").with_prefix(p));
    nick
}

/// Reads a draft/multiline batch, returning its opener and lines.
async fn read_multiline(c: &mut TestConn) -> (Message, Vec<Message>) {
    let opener = read_until(c, "BATCH").await;
    assert_eq!(opener.params[1], "draft/multiline", "{opener}");
    let r = opener.params[0].strip_prefix('+').unwrap().to_owned();
    let mut lines = Vec::new();
    loop {
        let m = c.read().await;
        if m.command == "BATCH" {
            assert_eq!(m.params[0], format!("-{r}"));
            return (opener, lines);
        }
        assert_eq!(m.tags.get("batch"), Some(r.as_str()), "{m}");
        lines.push(m);
    }
}

/// Reads the next BouncerServ reply.
async fn read_service_reply(c: &mut TestConn) -> String {
    loop {
        let m = read_until(c, "PRIVMSG").await;
        if m.prefix.as_ref().is_some_and(|p| p.name == "BouncerServ") {
            return m.params[1].clone();
        }
    }
}

/// Lists the targets of CHATHISTORY TARGETS with their soju.im/unread and
/// soju.im/mention tags.
async fn unread_target_tags(dc: &mut TestConn) -> Vec<(String, bool, bool)> {
    dc.send(msg!(
        "CHATHISTORY",
        "TARGETS",
        "timestamp=2000-01-01T00:00:00.000Z",
        "timestamp=2100-01-01T00:00:00.000Z",
        "1000"
    ));
    read_until(dc, "BATCH").await;
    let mut targets = Vec::new();
    loop {
        let m = dc.read().await;
        if m.command == "BATCH" {
            targets.sort();
            return targets;
        }
        assert_eq!(m.params[0], "TARGETS", "{m}");
        targets.push((
            m.params[1].clone(),
            m.tags.get("soju.im/unread").is_some(),
            m.tags.get("soju.im/mention").is_some(),
        ));
    }
}

const NETWORK_TAG: &str = "soju.im/network";

/// Reads messages until one with the given command, tagged with the network.
async fn read_until_tagged(c: &mut TestConn, cmd: &str, net: &str) -> Message {
    loop {
        let m = c.read().await;
        if m.command == cmd && m.tags.get(NETWORK_TAG) == Some(net) {
            return m;
        }
    }
}

/// Fetches history through IRC, checking the complete reply rather than
/// inspecting the database or the user's event-loop state.
async fn history_texts(c: &mut TestConn, target: &str) -> Vec<String> {
    c.send(msg!("CHATHISTORY", "LATEST", target, "*", "100"));
    c.roundtrip()
        .await
        .into_iter()
        .map(|m| {
            assert_eq!(m.command, "PRIVMSG", "unexpected history reply: {m}");
            // Incoming private messages address our nick; their sender is
            // the history target. Channel messages address the channel.
            assert!(
                m.params[0].eq_ignore_ascii_case(target)
                    || m.prefix_name().eq_ignore_ascii_case(target),
                "{m}"
            );
            m.params[1].clone()
        })
        .collect()
}

#[path = "server/protocol.rs"]
mod cases_protocol;
use cases_protocol::{
    broadcast, cap_req_removal, labeled_response_ack, monitor_invalid_targets,
    setname_survives_network_update, statusmsg_keeps_sigil, unread_replies_before_registration,
    upstream_malformed_messages,
};

#[path = "server/authentication.rs"]
mod cases_authentication;
use cases_authentication::{disabled_admin_impersonation, sasl_chunk_boundaries, sasl_errors};

#[path = "server/history.rs"]
mod cases_history;
use cases_history::{
    chat_history_db, chat_history_limit, chat_history_params, upstream_clock_skew,
};

#[path = "server/read_markers.rs"]
mod cases_read_markers;
use cases_read_markers::{read_marker, unread_targets};

#[path = "server/persistence.rs"]
mod cases_persistence;
use cases_persistence::{disconnected_client_history, sqlite_history_survives_restart};

#[path = "server/messages.rs"]
mod cases_messages;
use cases_messages::{blocked_users, multiline};

#[path = "server/multiplex.rs"]
mod cases_multiplex;
use cases_multiplex::{multiplex, multiplex_not_attached_label, multiplex_unsupported};

#[path = "server/service.rs"]
mod cases_service;
use cases_service::{service_blocking_work, service_other_user, user_network_channel_isolation};
