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
    chat_history_params,
    read_marker,
    upstream_clock_skew,
    multiline,
    service_other_user,
    chat_history_limit,
    service_blocking_work,
    monitor_invalid_targets,
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

async fn chat_history_db() {
    chat_history(MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    })
    .await;
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

async fn statusmsg_keeps_sigil() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream_with(&mut uc, &["STATUSMSG=@+"]).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "echo-message").await;

    // A message to "@#chan" must only reach channel operators
    dc.send(msg!("PRIVMSG", "@#chan", "ops only"));
    let m = read_until(&mut uc, "PRIVMSG").await;
    assert_eq!(m.params, vec!["@#chan", "ops only"]);
    // The upstream doesn't support echo-message: the bouncer echoes it
    let echo = read_until(&mut dc, "PRIVMSG").await;
    assert_eq!(echo.params, vec!["@#chan", "ops only"]);
    env.srv.shutdown().await;
}

async fn upstream_malformed_messages() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    // Multi-byte membership prefixes used to panic when parsing NAMES
    let nick = register_upstream_with(&mut uc, &["PREFIX=(abc)\u{FFFD}"]).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;
    dc.roundtrip().await;

    // Messages without a prefix come from the server, they used to panic
    for cmd in ["NICK", "JOIN", "SETNAME", "ACCOUNT"] {
        uc.send(msg!(cmd, "x"));
    }
    let us = Prefix::full(nick.clone(), "u", "h");
    uc.send(msg!("JOIN", "#c").with_prefix(us));
    uc.send(msg!("353", nick.clone(), "=", "#c", "\u{F000}x @y").with_prefix(server_prefix()));
    uc.send(msg!("366", nick.clone(), "#c", "End of /NAMES list").with_prefix(server_prefix()));
    uc.roundtrip().await;

    // The user's event loop is still alive
    uc.send(msg!("NOTICE", nick, "still alive").with_prefix(Prefix::new("bob")));
    let m = read_until(&mut dc, "NOTICE").await;
    assert_eq!(m.params[1], "still alive");
    env.srv.shutdown().await;
}

async fn cap_req_removal() {
    let env = setup(db_store()).await;
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("CAP", "LS", "302"));
    let caps = read_cap_ls(&mut dc).await;
    assert!(
        caps.iter().any(|c| c.starts_with("draft/multiline=")),
        "{caps:?}"
    );
    dc.send(msg!("CAP", "REQ", "batch server-time"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1..], ["ACK", "batch server-time"]);
    dc.send(msg!("CAP", "REQ", "-batch"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1..], ["ACK", "-batch"]);
    // A REQ is atomic: one unavailable cap rejects all of them
    dc.send(msg!("CAP", "REQ", "batch bogus-cap"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1..], ["NAK", "batch bogus-cap"]);
    dc.send(msg!("CAP", "LIST"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1], "LIST");
    let mut caps: Vec<&str> = m.params[2].split(' ').collect();
    caps.sort();
    assert_eq!(caps, ["cap-notify", "server-time"]);
    env.srv.shutdown().await;
}

async fn labeled_response_ack() {
    let env = setup(db_store()).await;
    // labeled-response is passed through from the upstream: use a bouncer
    // connection, which has none
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, None, "labeled-response batch").await;

    // A labeled command without any reply gets an ACK
    dc.send(msg!("PONG", "foo").with_tag("label", "z"));
    let m = dc.expect("ACK").await;
    assert_eq!(m.tags.get("label"), Some("z"));

    // Errors keep the label, without a batch around them
    dc.send(msg!("CHATHISTORY", "WHAT", "#x").with_tag("label", "e"));
    let m = dc.expect("FAIL").await;
    assert_eq!(m.tags.get("label"), Some("e"));
    assert_eq!(m.params[..3], ["CHATHISTORY", "INVALID_PARAMS", "WHAT"]);
    env.srv.shutdown().await;
}

async fn sasl_errors() {
    let env = setup(db_store()).await;

    // AUTHENTICATE needs the sasl capability
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    let m = dc.expect("904").await;
    assert_eq!(
        m.params[1],
        "AUTHENTICATE requires the \"sasl\" capability to be enabled"
    );

    // Invalid base64 fails the exchange but keeps the connection
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("CAP", "LS", "302"));
    read_cap_ls(&mut dc).await;
    dc.send(msg!("CAP", "REQ", "sasl"));
    dc.expect("CAP").await;
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    let m = dc.expect("AUTHENTICATE").await;
    assert_eq!(m.params, vec!["+"]);
    dc.send(msg!("AUTHENTICATE", "!!!!"));
    let m = dc.expect("904").await;
    assert_eq!(m.params[1], "Invalid base64-encoded response");

    // A new exchange still works
    use base64::Engine;
    let creds = format!("\0{TEST_USERNAME}\0{TEST_PASSWORD}");
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    dc.expect("AUTHENTICATE").await;
    dc.send(msg!(
        "AUTHENTICATE",
        base64::engine::general_purpose::STANDARD.encode(creds)
    ));
    read_until(&mut dc, "903").await;
    env.srv.shutdown().await;
}

async fn chat_history_params() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    let base = Utc.with_ymd_and_hms(2023, 5, 23, 6, 0, 0).unwrap();
    for i in 0..3 {
        uc.send(
            msg!("PRIVMSG", nick.clone(), format!("msg {i}"))
                .with_prefix(Prefix::new("Foo"))
                .with_tag(
                    "time",
                    xirc::format_server_time(base + chrono::Duration::seconds(i)),
                ),
        );
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut dc,
        Some(&env.network),
        "batch server-time draft/chathistory",
    )
    .await;

    for limit in ["-1", "1001", "abc"] {
        dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", limit));
        let m = dc.expect("FAIL").await;
        assert_eq!(
            m.params,
            [
                "CHATHISTORY",
                "INVALID_PARAMS",
                "LATEST",
                limit,
                "Invalid limit"
            ],
        );
    }

    // Targets are casemapped; LATEST returns the most recent messages
    dc.send(msg!("CHATHISTORY", "LATEST", "FOO", "*", "2"));
    let start = dc.expect("BATCH").await;
    assert_eq!(start.params[1], "chathistory");
    let name = start.params[0].trim_start_matches('+').to_owned();
    let mut texts = Vec::new();
    loop {
        let m = dc.read().await;
        if m.command == "BATCH" {
            assert_eq!(m.params[0], format!("-{name}"));
            break;
        }
        assert_eq!(m.tags.get("batch"), Some(name.as_str()));
        assert!(m.tags.contains("time"), "{m}");
        texts.push(m.params[1].clone());
    }
    assert_eq!(texts, ["msg 1", "msg 2"]);

    // A limit of 0 gives an empty batch
    dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", "0"));
    dc.expect("BATCH").await;
    let end = dc.expect("BATCH").await;
    assert!(end.params[0].starts_with('-'));
    env.srv.shutdown().await;
}

async fn read_marker() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "draft/read-marker").await;

    dc.send(msg!("MARKREAD", "#rm"));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params, vec!["#rm", "*"]);

    let ts = "timestamp=2024-01-01T00:00:00.000Z";
    dc.send(msg!("MARKREAD", "#RM", ts));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], ts);
    // Read markers only move forward, and targets are casemapped
    dc.send(msg!(
        "MARKREAD",
        "#rm",
        "timestamp=2023-01-01T00:00:00.000Z"
    ));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], ts);
    env.srv.shutdown().await;
}

async fn upstream_clock_skew() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;

    // The upstream server's clock is an hour ahead of ours
    let ahead = Utc::now() + chrono::Duration::hours(1);
    let ahead_str = xirc::format_server_time(ahead);
    uc.send(
        msg!("PRIVMSG", nick, "from the future")
            .with_prefix(Prefix::new("foo"))
            .with_tag("time", ahead_str.clone()),
    );
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "draft/read-marker").await;

    // CHATHISTORY isn't bounded by our clock
    for params in [
        vec!["LATEST", "foo", "*", "10"],
        vec!["AFTER", "foo", "timestamp=2020-01-01T00:00:00.000Z", "10"],
    ] {
        dc.send(Message::new(
            "CHATHISTORY",
            params.iter().map(|s| s.to_string()).collect(),
        ));
        let got: Vec<String> = dc
            .roundtrip()
            .await
            .into_iter()
            .map(|m| m.params[1].clone())
            .collect();
        assert_eq!(got, ["from the future"], "{params:?}");
    }

    // Read markers can reach the latest message, but not beyond it
    dc.send(msg!("MARKREAD", "foo", format!("timestamp={ahead_str}")));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], format!("timestamp={ahead_str}"));
    let later = xirc::format_server_time(ahead + chrono::Duration::hours(1));
    dc.send(msg!("MARKREAD", "foo", format!("timestamp={later}")));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], format!("timestamp={ahead_str}"));

    // Without messages in the future, markers stop at our clock
    let before = Utc::now();
    let far = xirc::format_server_time(before + chrono::Duration::days(1));
    dc.send(msg!("MARKREAD", "#empty", format!("timestamp={far}")));
    let m = dc.expect("MARKREAD").await;
    let t = xirc::parse_server_time(m.params[1].strip_prefix("timestamp=").unwrap()).unwrap();
    // Server times have millisecond precision
    let before = before - chrono::Duration::milliseconds(1);
    assert!(t >= before && t <= Utc::now(), "{m}");
    env.srv.shutdown().await;
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

async fn multiline() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_caps(
        &mut uc,
        "batch echo-message labeled-response message-tags server-time draft/multiline=max-bytes=4096,max-lines=4",
    )
    .await;

    let mut ml = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut ml,
        Some(&env.network),
        "batch echo-message labeled-response message-tags draft/chathistory draft/multiline",
    )
    .await;
    let mut plain = connect_downstream(&env.srv);
    register_downstream_caps(&mut plain, Some(&env.network), "server-time").await;

    ml.send(msg!("CAP", "LIST"));
    let m = ml.expect("CAP").await;
    assert!(
        m.params[2].split(' ').any(|c| c == "draft/multiline"),
        "{m}"
    );

    // A multiline message from the upstream server
    for line in [
        "@msgid=m1 :bob!b@h BATCH +x draft/multiline #c",
        "@batch=x :bob!b@h PRIVMSG #c hello",
        "@batch=x;draft/multiline-concat :bob!b@h PRIVMSG #c :\u{20}world",
        "@batch=x :bob!b@h PRIVMSG #c :",
        "@batch=x :bob!b@h PRIVMSG #c bye",
        "BATCH -x",
    ] {
        uc.send(Message::parse(line).unwrap());
    }
    let (opener, lines) = read_multiline(&mut ml).await;
    assert_eq!(opener.params[2], "#c");
    assert_eq!(opener.prefix_name(), "bob");
    assert_eq!(opener.tags.get("msgid"), Some("m1"));
    let texts: Vec<&str> = lines.iter().map(|m| m.params[1].as_str()).collect();
    assert_eq!(texts, ["hello world", "", "bye"]);
    // Clients without draft/multiline get separate messages
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "hello world"]);
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "bye"]);

    // A multiline message sent by a client, echoed by the upstream server
    for line in [
        "@label=l1;+draft/reply=m1 BATCH +a draft/multiline #c",
        "@batch=a PRIVMSG #c one",
        "@batch=a PRIVMSG #c two",
        "BATCH -a",
    ] {
        ml.send(Message::parse(line).unwrap());
    }
    let (opener, lines) = read_multiline(&mut uc).await;
    let label = opener.tags.get("label").unwrap().to_owned();
    assert_eq!(opener.tags.get("+draft/reply"), Some("m1"));
    let texts: Vec<&str> = lines.iter().map(|m| m.params[1].as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
    assert!(lines.iter().all(|m| m.tags.len() == 1), "{lines:?}");

    let us = format!("{nick}!u@h");
    for line in [
        format!("@label={label};msgid=m2;+draft/reply=m1 :{us} BATCH +e draft/multiline #c"),
        format!("@batch=e :{us} PRIVMSG #c one"),
        format!("@batch=e :{us} PRIVMSG #c two"),
        "BATCH -e".to_owned(),
    ] {
        uc.send(Message::parse(&line).unwrap());
    }
    // The echo is the labeled response, not wrapped in another batch
    let (opener, lines) = read_multiline(&mut ml).await;
    assert_eq!(opener.tags.get("label"), Some("l1"));
    assert_eq!(opener.tags.get("msgid"), Some("m2"));
    assert_eq!(lines.len(), 2);
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "one"]);
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "two"]);

    // Each multiline message is a single history entry
    ml.send(msg!("CHATHISTORY", "LATEST", "#c", "*", "10"));
    let outer = read_until(&mut ml, "BATCH").await;
    assert_eq!(outer.params[1], "chathistory");
    let outer_ref = outer.params[0].strip_prefix('+').unwrap().to_owned();
    for want in [vec!["hello world", "", "bye"], vec!["one", "two"]] {
        let (opener, lines) = read_multiline(&mut ml).await;
        assert_eq!(opener.tags.get("batch"), Some(outer_ref.as_str()));
        let texts: Vec<&str> = lines.iter().map(|m| m.params[1].as_str()).collect();
        assert_eq!(texts, want);
    }
    let m = ml.expect("BATCH").await;
    assert_eq!(m.params[0], format!("-{outer_ref}"));

    // Limits are enforced
    ml.send(Message::parse("@label=l2 BATCH +b draft/multiline #c").unwrap());
    for _ in 0..5 {
        ml.send(Message::parse("@batch=b PRIVMSG #c x").unwrap());
    }
    ml.send(msg!("BATCH", "-b"));
    let m = ml.expect("FAIL").await;
    assert_eq!(m.tags.get("label"), Some("l2"));
    assert_eq!(m.params[..3], ["BATCH", "MULTILINE_MAX_LINES", "4"]);
    env.srv.shutdown().await;
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

/// BouncerServ commands waiting on another user's event loop.
async fn service_other_user() {
    let env = setup_with(db_store(), |db, user| {
        user.admin = true;
        db.store_user(&mut database::User::new("other")).unwrap();
    })
    .await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user update other -max-networks 5"
    ));
    assert_eq!(read_service_reply(&mut dc).await, r#"updated user "other""#);

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user run other user update -max-networks 3"
    ));
    assert_eq!(read_service_reply(&mut dc).await, r#"updated user "other""#);

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user run other network status"
    ));
    let reply = read_service_reply(&mut dc).await;
    assert!(!reply.starts_with("error:"), "unexpected reply: {reply}");

    let hash = {
        use sha1::{Digest, Sha1};
        hex::encode(&Sha1::digest(b"other")[..3])
    };
    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        format!("user delete other {hash}")
    ));
    assert_eq!(read_service_reply(&mut dc).await, r#"deleted user "other""#);
    assert!(env.srv.get_user("other").is_none());
    env.srv.shutdown().await;
}

/// A lower chathistory-limit is advertised and caps replies.
async fn chat_history_limit() {
    let env = setup(db_store()).await;
    let mut cfg = (*env.srv.config()).clone();
    cfg.basic.chat_history_limit = 2;
    env.srv.set_config(cfg);
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    for i in 0..3 {
        uc.send(msg!("PRIVMSG", nick.clone(), format!("msg {i}")).with_prefix(Prefix::new("foo")));
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    let isupport: Vec<String> = register_downstream_caps(
        &mut dc,
        Some(&env.network),
        "batch server-time draft/chathistory",
    )
    .await
    .into_iter()
    .filter(|m| m.command == "005")
    .flat_map(|m| m.params)
    .collect();
    assert!(
        isupport.iter().any(|p| p == "CHATHISTORY=2"),
        "missing CHATHISTORY=2 in {isupport:?}"
    );

    dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", "1001"));
    let m = dc.expect("FAIL").await;
    assert_eq!(m.params[..2], ["CHATHISTORY", "INVALID_PARAMS"]);

    // Bigger limits are capped, e.g. TARGETS still works with 1000
    dc.send(msg!(
        "CHATHISTORY",
        "TARGETS",
        "timestamp=2000-01-01T00:00:00.000Z",
        "timestamp=2100-01-01T00:00:00.000Z",
        "1000"
    ));
    let m = read_until(&mut dc, "CHATHISTORY").await;
    assert_eq!(m.params[..2], ["TARGETS", "foo"]);
    read_until(&mut dc, "BATCH").await;
    dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", "1000"));
    let outer = dc.expect("BATCH").await;
    assert_eq!(outer.params[1], "chathistory");
    for want in ["msg 1", "msg 2"] {
        let m = dc.expect("PRIVMSG").await;
        assert_eq!(m.params[1], want);
    }
    dc.expect("BATCH").await;
    env.srv.shutdown().await;
}

/// BouncerServ commands doing CPU-heavy work (key generation, password
/// hashing) off the event loop, then applying it.
async fn service_blocking_work() {
    let env = setup_with(db_store(), |_, user| user.admin = true).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "certfp generate -key-type ed25519"
    ));
    assert_eq!(read_service_reply(&mut dc).await, "certificate generated");
    assert!(
        read_service_reply(&mut dc)
            .await
            .starts_with("SHA-1 fingerprint: ")
    );
    read_service_reply(&mut dc).await;
    read_service_reply(&mut dc).await;
    let user_id = env.srv.db.get_user(TEST_USERNAME).unwrap().unwrap().id;
    let sasl = &env.srv.db.list_networks(user_id).unwrap()[0].sasl;
    assert_eq!(sasl.mechanism, "EXTERNAL");
    assert!(sasl.external_key.is_some());

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user update -password newpass"
    ));
    assert_eq!(
        read_service_reply(&mut dc).await,
        format!("updated user {TEST_USERNAME:?}")
    );
    let mut record = env.srv.db.get_user(TEST_USERNAME).unwrap().unwrap();
    record.check_password("newpass").unwrap();

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user create -username created -password pw"
    ));
    assert_eq!(
        read_service_reply(&mut dc).await,
        r#"created user "created""#
    );
    assert!(env.srv.get_user("created").is_some());
    env.srv.shutdown().await;
}

/// MONITOR ignores empty targets and targets containing spaces.
async fn monitor_invalid_targets() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream_with(&mut uc, &["MONITOR=100"]).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    dc.send(msg!("MONITOR", "+", "a,,b"));
    dc.send(msg!("MONITOR", "+", "c d"));
    dc.send(msg!("MONITOR", "L"));
    let mut names = Vec::new();
    loop {
        let m = dc.read().await;
        match m.command.as_str() {
            "732" => names.push(m.params[1].clone()),
            "733" => break,
            _ => {}
        }
    }
    names.sort();
    assert_eq!(names, ["a", "b"]);
    env.srv.shutdown().await;
}
