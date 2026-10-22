//! Tests of upstream message handling, with an in-process bouncer and a fake
//! upstream server.

use std::time::Duration;

use tokio::net::TcpListener;

use crate::conn::{Conn, ConnInfo, ConnReader};
use crate::database::{self as db, Database};
use crate::irc::Message;
use crate::logger::Logger;
use crate::msg;
use crate::server::{Server, ServerConfig};

const USERNAME: &str = "soju-test-user";
const TIMEOUT: Duration = Duration::from_secs(10);

struct TestConn {
    conn: Conn,
    reader: ConnReader,
}

impl TestConn {
    fn new(io: crate::conn::BoxIo) -> TestConn {
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

    /// Reads messages until one with the given command.
    async fn read_until(&mut self, cmd: &str) -> Message {
        loop {
            let msg = self.read().await;
            if msg.command == cmd {
                return msg;
            }
        }
    }
}

struct Env {
    srv: std::sync::Arc<Server>,
    upstream: TestConn,
    downstream: TestConn,
}

/// Starts a bouncer with a registered upstream and downstream connection.
async fn setup() -> Env {
    setup_with(Vec::new(), "").await
}

/// Like setup, with saved channels, and caps enabled on both connections.
async fn setup_with(channels: Vec<db::Channel>, caps: &str) -> Env {
    let database = Database::open_in_memory().unwrap();
    let mut user = db::User::new(USERNAME);
    user.set_password(USERNAME).unwrap();
    database.store_user(&mut user).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut network = db::Network::new(&format!(
        "irc+insecure://{}",
        listener.local_addr().unwrap()
    ));
    network.name = "testnet".to_owned();
    database.store_network(user.id, &mut network).unwrap();
    for mut ch in channels {
        database.store_channel(network.id, &mut ch).unwrap();
    }

    let srv = Server::new(database, Logger::new(false));
    srv.set_config(ServerConfig::default());
    srv.start().unwrap();

    let (s, _) = tokio::time::timeout(TIMEOUT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut upstream = TestConn::new(Box::new(s));
    upstream.expect("CAP").await;
    upstream.expect("NICK").await;
    upstream.expect("USER").await;
    if !caps.is_empty() {
        upstream.send(msg!("CAP", "*", "LS", caps));
        let m = upstream.expect("CAP").await;
        assert_eq!(m.params[0], "REQ", "invalid CAP: {m}");
        upstream.send(msg!("CAP", "*", "ACK", m.params[1].clone()));
        upstream.expect("CAP").await;
    }
    for m in [
        msg!("001", USERNAME, "Welcome!"),
        msg!("002", USERNAME, "Your host is test"),
        msg!("003", USERNAME, "Created"),
        msg!(
            "004",
            USERNAME,
            "test",
            "soju",
            "aiwroO",
            "OovaimnqpsrtklbeI"
        ),
        msg!("422", USERNAME, "No MOTD"),
    ] {
        upstream.send(m.with_prefix(crate::irc::Prefix::new("test")));
    }

    let (a, b) = tokio::io::duplex(1 << 16);
    let logger = Logger::new(false);
    let (conn, reader) = Conn::new_stream(Box::new(a), logger.clone(), None);
    let info = ConnInfo {
        remote_addr: "127.0.0.1:1234".to_owned(),
        ..Default::default()
    };
    tokio::spawn(crate::downstream::serve(
        srv.clone(),
        conn,
        reader,
        info,
        logger,
    ));
    let mut downstream = TestConn::new(Box::new(b));
    downstream.send(msg!("CAP", "LS", "302"));
    downstream.send(msg!("PASS", USERNAME));
    downstream.send(msg!("NICK", USERNAME));
    downstream.send(msg!(
        "USER",
        format!("{USERNAME}/testnet"),
        "0",
        "*",
        USERNAME
    ));
    downstream.send(msg!("CAP", "END"));
    downstream.read_until("001").await;
    // Caps passed through from the upstream are available once bound to
    // the network
    if !caps.is_empty() {
        let req = msg!("CAP", "REQ", caps);
        downstream.send(req.clone());
        loop {
            let m = downstream.read_until("CAP").await;
            match m.params[1].as_str() {
                "ACK" => break,
                // The upstream connection wasn't ready yet: wait for it
                "NAK" => {}
                "NEW" => downstream.send(req.clone()),
                _ => panic!("invalid CAP: {m}"),
            }
        }
    }
    // Wait for the end of the welcome burst
    downstream.send(msg!("PING", "welcome"));
    downstream.read_until("PONG").await;

    Env {
        srv,
        upstream,
        downstream,
    }
}

/// The RPL_ENDOFWHOIS servers send after ERR_NOSUCHNICK belongs to the
/// failed WHOIS, not to the next one in the queue.
#[tokio::test]
async fn whois_no_such_nick() {
    let mut env = setup().await;
    env.downstream.send(msg!("WHOIS", "ghost"));
    env.downstream.send(msg!("WHOIS", "alice"));

    let m = env.upstream.read_until("WHOIS").await;
    assert_eq!(m.params, ["ghost"]);
    env.upstream
        .send(msg!("401", USERNAME, "ghost", "No such nick/channel"));
    env.upstream
        .send(msg!("318", USERNAME, "ghost", "End of /WHOIS list"));
    let m = env.upstream.read_until("WHOIS").await;
    assert_eq!(m.params, ["alice"]);
    env.upstream
        .send(msg!("311", USERNAME, "alice", "a", "host", "*", "Alice"));
    env.upstream
        .send(msg!("318", USERNAME, "alice", "End of /WHOIS list"));

    let want = [
        ("401", "ghost"),
        ("318", "ghost"),
        ("311", "alice"),
        ("318", "alice"),
    ];
    for (cmd, nick) in want {
        let m = env.downstream.expect(cmd).await;
        assert_eq!(m.params[1], nick, "invalid reply: {m}");
    }
    env.srv.shutdown().await;
}

/// Same for RPL_TRYAGAIN followed by RPL_ENDOFWHO.
#[tokio::test]
async fn who_try_again() {
    let mut env = setup().await;
    env.downstream.send(msg!("WHO", "#busy"));
    env.downstream.send(msg!("WHO", "bob"));

    let m = env.upstream.read_until("WHO").await;
    assert_eq!(m.params, ["#busy"]);
    env.upstream.send(msg!(
        "263",
        USERNAME,
        "WHO",
        "Please wait a while and try again."
    ));
    env.upstream
        .send(msg!("315", USERNAME, "#busy", "End of /WHO list"));
    let m = env.upstream.read_until("WHO").await;
    assert_eq!(m.params, ["bob"]);
    env.upstream.send(msg!(
        "352", USERNAME, "*", "b", "host", "test", "bob", "H", "0 Bob"
    ));
    env.upstream
        .send(msg!("315", USERNAME, "bob", "End of /WHO list"));

    let want = [
        ("263", "WHO"),
        ("315", "#busy"),
        ("352", "*"),
        ("315", "bob"),
    ];
    for (cmd, param) in want {
        let m = env.downstream.expect(cmd).await;
        assert_eq!(m.params[1], param, "invalid reply: {m}");
    }
    env.srv.shutdown().await;
}

/// Messages in a channel postpone its auto-detach, which happens once the
/// channel has been quiet for detach-after.
#[tokio::test]
async fn auto_detach() {
    let detach_after = Duration::from_millis(600);
    let ch = db::Channel {
        name: "#chan".to_owned(),
        detach_after,
        ..Default::default()
    };
    let mut env = setup_with(vec![ch], "").await;
    let m = env.upstream.read_until("JOIN").await;
    assert_eq!(m.params[0], "#chan");
    let us = crate::irc::Prefix::full(USERNAME, "u", "host");
    env.upstream.send(msg!("JOIN", "#chan").with_prefix(us));
    env.downstream.read_until("JOIN").await;

    let bob = crate::irc::Prefix::full("bob", "b", "host");
    let start = tokio::time::Instant::now();
    for i in 0..4 {
        tokio::time::sleep(detach_after / 2).await;
        let text = format!("message {i}");
        env.upstream
            .send(msg!("PRIVMSG", "#chan", text).with_prefix(bob.clone()));
        loop {
            let m = env.downstream.read().await;
            assert_ne!(m.command, "PART", "channel detached too early: {m}");
            if m.command == "PRIVMSG" {
                break;
            }
        }
    }
    let last = tokio::time::Instant::now();
    let m = env.downstream.read_until("PART").await;
    assert_eq!(m.params, ["#chan", "Detach"]);
    assert!(last.elapsed() >= detach_after - Duration::from_millis(50));
    assert!(start.elapsed() > detach_after * 2);
    env.srv.shutdown().await;
}

/// A labeled response is relayed in a single labeled batch, batches nested
/// in it included.
#[tokio::test]
async fn labeled_nested_batch() {
    let mut env = setup_with(Vec::new(), "batch labeled-response message-tags").await;
    env.downstream
        .send(msg!("WHOIS", "bob").with_tag("label", "l1"));
    let m = env.upstream.read_until("WHOIS").await;
    let label = m.tags.get("label").expect("missing label").to_owned();

    let p = crate::irc::Prefix::new("test");
    let upstream_msgs = [
        msg!("BATCH", "+o", "labeled-response").with_tag("label", &label),
        msg!("BATCH", "+i", "example/nested").with_tag("batch", "o"),
        msg!("311", USERNAME, "bob", "b", "host", "*", "Bob").with_tag("batch", "i"),
        msg!("318", USERNAME, "bob", "End of /WHOIS list").with_tag("batch", "i"),
        msg!("BATCH", "-i"),
        msg!("BATCH", "-o"),
    ];
    for m in upstream_msgs {
        env.upstream.send(m.with_prefix(p.clone()));
    }

    let m = env.downstream.expect("BATCH").await;
    assert_eq!(m.params, ["+o", "labeled-response"]);
    assert_eq!(m.tags.get("label"), Some("l1"));
    let m = env.downstream.expect("BATCH").await;
    assert_eq!(m.params, ["+i", "example/nested"]);
    assert_eq!(m.tags.get("label"), None, "nested batch labeled: {m}");
    assert_eq!(m.tags.get("batch"), Some("o"));
    for cmd in ["311", "318"] {
        let m = env.downstream.expect(cmd).await;
        assert_eq!(m.tags.get("batch"), Some("i"));
    }
    assert_eq!(env.downstream.expect("BATCH").await.params, ["-i"]);
    assert_eq!(env.downstream.expect("BATCH").await.params, ["-o"]);
    env.srv.shutdown().await;
}
