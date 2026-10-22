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
    downstream.send(msg!("PASS", USERNAME));
    downstream.send(msg!("NICK", USERNAME));
    downstream.send(msg!(
        "USER",
        format!("{USERNAME}/testnet"),
        "0",
        "*",
        USERNAME
    ));
    downstream.expect("001").await;
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
