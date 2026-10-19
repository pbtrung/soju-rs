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

    /// Sends a PING and returns the messages received until the PONG.
    async fn roundtrip(&mut self) -> Vec<Message> {
        self.send(msg!("PING", "roundtrip"));
        let mut msgs = Vec::new();
        loop {
            let msg = self.read().await;
            if msg.command == "PONG" {
                return msgs;
            }
            msgs.push(msg);
        }
    }
}

struct Env {
    srv: Arc<Server>,
    network: database::Network,
    upstream: TcpListener,
    _dir: Option<std::path::PathBuf>,
}

async fn setup(msg_store: MsgStore) -> Env {
    let db = Database::open_in_memory().unwrap();
    let mut user = database::User::new(TEST_USERNAME);
    user.set_password(TEST_PASSWORD).unwrap();
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
        _dir: None,
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
    c.send(msg!("PASS", TEST_PASSWORD));
    c.send(msg!("NICK", TEST_USERNAME));
    c.send(msg!(
        "USER",
        format!("{TEST_USERNAME}/{}", network.name),
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

#[tokio::test(flavor = "multi_thread")]
async fn broadcast() {
    let env = setup(MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    })
    .await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    let notice = "This is a very important server notice.";
    uc.send(msg!("NOTICE", TEST_USERNAME, notice).with_prefix(server_prefix()));
    let msg = loop {
        let msg = dc.read().await;
        if msg.command == "NOTICE" {
            break msg;
        }
    };
    assert_eq!(msg.params[1], notice);
    env.srv.shutdown().await;
}

async fn chat_history(msg_store: MsgStore) {
    let env = setup(msg_store).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;

    let texts = [
        "Hiya!",
        "How are you doing?",
        "Can I take a sip from your glass of soju?",
    ];
    let base = Utc.with_ymd_and_hms(2023, 5, 23, 6, 0, 0).unwrap();
    let at = |secs: i64| base + chrono::Duration::milliseconds(secs);
    for (i, text) in texts.iter().enumerate() {
        uc.send(
            msg!("PRIVMSG", TEST_USERNAME, *text)
                .with_prefix(Prefix::new("foo"))
                .with_tag("time", xirc::format_server_time(at(i as i64 * 1000))),
        );
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;
    dc.roundtrip().await; // drain post-connection-registration messages

    let n = texts.len() as i64;
    let ts = |ms: i64| format!("timestamp={}", xirc::format_server_time(at(ms)));
    let s = |r: std::ops::Range<usize>| texts[r].iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let cases: Vec<(&str, Vec<String>, usize, Vec<String>)> = vec![
        (
            "before_all",
            vec!["BEFORE".into(), ts(n * 1000)],
            0,
            s(0..3),
        ),
        ("before_none", vec!["BEFORE".into(), ts(0)], 0, vec![]),
        ("before_first", vec!["BEFORE".into(), ts(1000)], 0, s(0..1)),
        (
            "before_with_limit",
            vec!["BEFORE".into(), ts((n - 1) * 1000)],
            1,
            s(1..2),
        ),
        ("after_all", vec!["AFTER".into(), ts(-1000)], 0, s(0..3)),
        (
            "after_none",
            vec!["AFTER".into(), ts((n - 1) * 1000)],
            0,
            vec![],
        ),
        (
            "after_all_but_first",
            vec!["AFTER".into(), ts(0)],
            0,
            s(1..3),
        ),
        ("after_with_limit", vec!["AFTER".into(), ts(0)], 1, s(1..2)),
        (
            "between_all",
            vec!["BETWEEN".into(), ts(-1000), ts(n * 1000)],
            0,
            s(0..3),
        ),
        (
            "between_all_reverse",
            vec!["BETWEEN".into(), ts(n * 1000), ts(-1000)],
            0,
            s(0..3),
        ),
        (
            "between_middle",
            vec!["BETWEEN".into(), ts(0), ts((n - 1) * 1000)],
            0,
            s(1..2),
        ),
        (
            "between_middle_reverse",
            vec!["BETWEEN".into(), ts((n - 1) * 1000), ts(0)],
            0,
            s(1..2),
        ),
        (
            "between_last_two",
            vec!["BETWEEN".into(), ts(0), ts(n * 1000)],
            0,
            s(1..3),
        ),
        (
            "between_last_two_reverse",
            vec!["BETWEEN".into(), ts(n * 1000), ts(0)],
            0,
            s(1..3),
        ),
        (
            "between_first_two",
            vec!["BETWEEN".into(), ts(-1000), ts((n - 1) * 1000)],
            0,
            s(0..2),
        ),
        (
            "between_first_two_reverse",
            vec!["BETWEEN".into(), ts((n - 1) * 1000), ts(-1000)],
            0,
            s(0..2),
        ),
        (
            "between_with_limit",
            vec!["BETWEEN".into(), ts(0), ts(n * 1000)],
            1,
            s(1..2),
        ),
        ("around_all", vec!["AROUND".into(), ts(1000)], 0, s(0..3)),
        (
            "around_with_limit",
            vec!["AROUND".into(), ts(1500)],
            2,
            s(1..3),
        ),
    ];

    for (name, bounds, limit, want) in cases {
        let limit = if limit == 0 {
            "100".to_owned()
        } else {
            limit.to_string()
        };
        let mut params = vec![bounds[0].clone(), "foo".to_owned()];
        params.extend_from_slice(&bounds[1..]);
        params.push(limit);
        dc.send(Message::new("CHATHISTORY", params));
        let got: Vec<String> = dc
            .roundtrip()
            .await
            .into_iter()
            .map(|m| {
                assert_eq!(m.command, "PRIVMSG", "{name}: unexpected reply: {m}");
                m.params[1].clone()
            })
            .collect();
        assert_eq!(got, want, "{name}");
    }
    env.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_history_db() {
    chat_history(MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_history_fs() {
    let dir = std::env::temp_dir().join(format!("soju-rs-chathistory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    chat_history(MsgStore {
        driver: MsgStoreDriver::Fs,
        source: dir.to_str().unwrap().to_owned(),
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}
