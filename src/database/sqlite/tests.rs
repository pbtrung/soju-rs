//! Tests of the SQLite database.

use super::messages::quote_fts_query;
use super::*;
use crate::irc::Message;
use crate::msg;
use rusqlite::Connection;

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

#[test]
fn users() {
    let db = db();
    let mut u = User::new("alice");
    u.set_password("pw").unwrap();
    db.store_user(&mut u).unwrap();
    assert_ne!(u.id, 0);
    let mut got = db.get_user("alice").unwrap().unwrap();
    assert_eq!(got, u);
    got.nick = "Alice".into();
    got.downstream_interacted_at = Some(Utc::now());
    db.store_user(&mut got).unwrap();
    let users = db.list_users().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].nick, "Alice");
    assert!(users[0].downstream_interacted_at.is_some());
    assert_eq!(
        db.get_username_by_id(u.id).unwrap().as_deref(),
        Some("alice")
    );
    assert!(db.get_user("bob").unwrap().is_none());
    let inactive = db
        .list_inactive_usernames(Utc::now() + chrono::Duration::hours(1))
        .unwrap();
    assert_eq!(inactive, vec!["alice"]);
    // Disabled users are already inactive
    let mut disabled = got.clone();
    disabled.enabled = false;
    db.store_user(&mut disabled).unwrap();
    assert!(
        db.list_inactive_usernames(Utc::now() + chrono::Duration::hours(1))
            .unwrap()
            .is_empty()
    );
    assert!(
        db.list_inactive_usernames(Utc::now() - chrono::Duration::hours(1))
            .unwrap()
            .is_empty()
    );
    db.delete_user(u.id).unwrap();
    assert!(db.list_users().unwrap().is_empty());
}

#[test]
fn networks_channels() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    n.connect_commands = vec!["PRIVMSG a :b".into(), "JOIN #x".into()];
    n.sasl.mechanism = "EXTERNAL".into();
    n.sasl.external_cert = Some(vec![1, 2, 3]);
    n.sasl.external_key = Some(vec![4]);
    db.store_network(u.id, &mut n).unwrap();
    let nets = db.list_networks(u.id).unwrap();
    assert_eq!(nets, vec![n.clone()]);
    n.sasl.mechanism = "PLAIN".into();
    n.sasl.plain_username = "acc".into();
    db.store_network(u.id, &mut n).unwrap();
    let nets = db.list_networks(u.id).unwrap();
    assert_eq!(nets[0].sasl.external_cert, None);
    assert_eq!(nets[0].sasl.plain_username, "acc");

    let mut ch = Channel {
        name: "#soju".into(),
        detach_after: Duration::from_millis(1500),
        relay_detached: MessageFilter::Message,
        ..Default::default()
    };
    db.store_channel(n.id, &mut ch).unwrap();
    let chs = db.list_channels(n.id).unwrap();
    assert_eq!(chs.len(), 1);
    assert_eq!(chs[0].detach_after, Duration::from_secs(2));
    assert_eq!(chs[0].relay_detached, MessageFilter::Message);
    db.delete_channel(ch.id).unwrap();
    assert!(db.list_channels(n.id).unwrap().is_empty());
    db.delete_network(n.id).unwrap();
    assert!(db.list_networks(u.id).unwrap().is_empty());
}

#[test]
fn messages() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();

    let m1 = msg!("PRIVMSG", "#a", "hello \x02world\x02")
        .with_prefix(crate::irc::Prefix::new("bob"))
        .with_tag("time", "2024-01-01T00:00:00.000Z");
    let m2 = msg!("JOIN", "#a")
        .with_prefix(crate::irc::Prefix::new("carol"))
        .with_tag("time", "2024-01-01T00:00:01.000Z");
    let m3 = msg!("NOTICE", "#a", "goodbye world")
        .with_prefix(crate::irc::Prefix::new("bob"))
        .with_tag("time", "2024-01-01T00:00:02.000Z");
    let ids = db
        .store_messages(n.id, "#a", &[m1.clone(), m2.clone(), m3.clone()])
        .unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[2]);
    assert_eq!(db.get_message_last_id(n.id, "#b").unwrap(), 0);

    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m1.clone(), m3.clone()]
    );
    let opts = MessageOptions {
        limit: 10,
        events: true,
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 3);
    let opts = MessageOptions {
        limit: 1,
        take_last: true,
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m3.clone()]
    );
    let opts = MessageOptions {
        limit: 10,
        text: "world\"".into(),
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 2);
    // Nothing to search for, rather than an FTS syntax error
    let opts = MessageOptions {
        limit: 10,
        text: "!!!".into(),
        ..Default::default()
    };
    assert!(db.list_messages(n.id, "#a", &opts).unwrap().is_empty());
    let opts = MessageOptions {
        limit: 10,
        text: "hello world".into(),
        sender: "bob".into(),
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m1.clone()]
    );
    let opts = MessageOptions {
        limit: 10,
        after_time: xirc::parse_server_time("2024-01-01T00:00:00.000Z"),
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m3.clone()]
    );

    db.store_messages(
        n.id,
        "#b",
        &[msg!("PRIVMSG", "#b", "x").with_tag("time", "2024-01-01T00:00:05.000Z")],
    )
    .unwrap();
    let opts = MessageOptions {
        limit: 10,
        after_time: xirc::parse_server_time("2023-01-01T00:00:00.000Z"),
        before_time: xirc::parse_server_time("2025-01-01T00:00:00.000Z"),
        ..Default::default()
    };
    let targets = db.list_message_last_per_target(n.id, &opts).unwrap();
    assert_eq!(
        targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["#a", "#b"]
    );
    assert_eq!(
        targets[0].latest_message,
        xirc::parse_server_time("2024-01-01T00:00:02.000Z")
    );

    db.delete_messages_before(
        xirc::parse_server_time("2024-01-01T00:00:01.500Z").unwrap(),
        usize::MAX,
    )
    .unwrap();
    let opts = MessageOptions {
        limit: 10,
        events: true,
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap(), vec![m3]);
}

#[test]
fn messages_same_time() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();

    // e.g. the lines of a multiline message
    let msgs: Vec<Message> = (0..5)
        .map(|i| msg!("PRIVMSG", "#a", i.to_string()).with_tag("time", "2024-01-01T00:00:00.000Z"))
        .collect();
    let ids = db.store_messages(n.id, "#a", &msgs).unwrap();
    assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[4]);
    let texts = |opts: &MessageOptions| -> Vec<String> {
        db.list_messages(n.id, "#a", opts)
            .unwrap()
            .into_iter()
            .map(|m| m.params[1].clone())
            .collect()
    };
    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(texts(&opts), ["0", "1", "2", "3", "4"]);
    let opts = MessageOptions {
        limit: 2,
        take_last: true,
        ..Default::default()
    };
    assert_eq!(texts(&opts), ["3", "4"]);
    let opts = MessageOptions {
        after_id: ids[2],
        limit: 10,
        take_last: true,
        ..Default::default()
    };
    assert_eq!(texts(&opts), ["3", "4"]);
}

#[test]
fn message_ids_not_reused() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let m = msg!("PRIVMSG", "#a", "x").with_tag("time", "2024-01-01T00:00:00.000Z");
    let after = |id| MessageOptions {
        after_id: id,
        limit: 10,
        ..Default::default()
    };

    // Expiry empties the table
    let id1 = db
        .store_messages(n.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    assert_eq!(
        db.delete_messages_before(Utc::now(), usize::MAX).unwrap(),
        1
    );
    let id2 = db
        .store_messages(n.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    assert!(id2 > id1);
    assert_eq!(db.list_messages(n.id, "#a", &after(id1)).unwrap().len(), 1);

    // Deleting a network or a user removes the highest IDs
    let mut n2 = Network::new("irc.example.com");
    db.store_network(u.id, &mut n2).unwrap();
    let id3 = db
        .store_messages(n2.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    db.delete_network(n2.id).unwrap();
    let id4 = db
        .store_messages(n.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    assert!(id4 > id3);
    let mut u2 = User::new("bob");
    db.store_user(&mut u2).unwrap();
    let mut n3 = Network::new("irc.example.org");
    db.store_network(u2.id, &mut n3).unwrap();
    let id5 = db
        .store_messages(n3.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    db.delete_user(u2.id).unwrap();
    let id6 = db.store_messages(n.id, "#a", &[m]).unwrap()[0];
    assert!(id6 > id5);
}

#[test]
fn delete_messages_in_chunks() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let old = msg!("PRIVMSG", "#a", "x").with_tag("time", "2024-01-01T00:00:00.000Z");
    let new = msg!("PRIVMSG", "#a", "y").with_tag("time", "2030-01-01T00:00:00.000Z");
    let mut msgs = vec![old; 5];
    msgs.push(new);
    db.store_messages(n.id, "#a", &msgs).unwrap();
    let before = xirc::parse_server_time("2025-01-01T00:00:00.000Z").unwrap();
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 2);
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 2);
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 1);
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 0);
    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 1);
}

#[test]
fn message_last_id_insertion_order() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    // The upstream server's clock went backwards
    let m1 = msg!("PRIVMSG", "#a", "x").with_tag("time", "2030-01-01T00:00:00.000Z");
    let m2 = msg!("PRIVMSG", "#a", "y").with_tag("time", "2024-01-01T00:00:00.000Z");
    let ids = db.store_messages(n.id, "#a", &[m1, m2]).unwrap();
    assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[1]);
}

#[test]
fn delete_messages_vacuum() {
    let dir = std::env::temp_dir().join(format!("soju-rs-vacuum-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.db");
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let text = "x".repeat(1000);
    let msgs: Vec<Message> = (0..2000)
        .map(|_| msg!("PRIVMSG", "#a", text.as_str()).with_tag("time", "2024-01-01T00:00:00.000Z"))
        .collect();
    db.store_messages(n.id, "#a", &msgs).unwrap();
    let size = || std::fs::metadata(&path).unwrap().len();
    db.conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let before = size();

    assert!(db.free_page_ratio().unwrap() < 0.05);
    assert_eq!(
        db.delete_messages_before(Utc::now(), usize::MAX).unwrap(),
        2000
    );
    assert!(db.free_page_ratio().unwrap() > 0.5);
    db.optimize().unwrap();
    db.vacuum().unwrap();
    db.shrink_memory().unwrap();
    assert_eq!(db.free_page_ratio().unwrap(), 0.0);
    assert!(size() < before / 2, "{} >= {before} / 2", size());
    let wal = std::fs::metadata(dir.join("main.db-wal")).map_or(0, |m| m.len());
    assert_eq!(wal, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn messages_invalid_utf8() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    db.store_messages(n.id, "#a", &[msg!("PRIVMSG", "#a", "x")])
        .unwrap();

    // Go soju stores raw upstream bytes, which may be e.g. Latin-1
    let raw: &[u8] = b"@time=2024-01-01T00:00:00.000Z :bob PRIVMSG #a :caf\xe9";
    db.conn()
        .execute(
            "INSERT INTO Message(target, raw, time, sender, text)
            SELECT id, CAST(?1 AS TEXT), '2024-01-01T00:00:00.000Z', 'bob', 'caf'
            FROM MessageTarget WHERE target = '#a'",
            [raw],
        )
        .unwrap();

    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    let l = db.list_messages(n.id, "#a", &opts).unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[0].params, vec!["#a", "caf\u{fffd}"]);

    // Channel names and delivery receipt targets come from upstream too
    let name: &[u8] = b"#caf\xe9";
    db.conn()
        .execute(
            "INSERT INTO Channel(network, name) VALUES (?1, CAST(?2 AS TEXT))",
            rusqlite::params![n.id, name],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO DeliveryReceipt(network, target, internal_msgid)
            VALUES (?1, CAST(?2 AS TEXT), 'x')",
            rusqlite::params![n.id, name],
        )
        .unwrap();
    let channels = db.list_channels(n.id).unwrap();
    assert_eq!(channels[0].name, "#caf\u{fffd}");
    let receipts = db.list_delivery_receipts(n.id).unwrap();
    assert_eq!(receipts[0].target, "#caf\u{fffd}");
    assert_eq!(receipts[0].client, "");
}

#[test]
fn receipts_targets_webpush_certs() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();

    let mut rcpts = vec![DeliveryReceipt {
        target: "#a".into(),
        internal_msg_id: "id1".into(),
        ..Default::default()
    }];
    db.store_client_delivery_receipts(n.id, "", &mut rcpts)
        .unwrap();
    db.store_client_delivery_receipts(n.id, "", &mut rcpts)
        .unwrap();
    db.store_client_delivery_receipts(n.id, "phone", &mut rcpts)
        .unwrap();
    assert_eq!(db.list_delivery_receipts(n.id).unwrap().len(), 2);

    assert!(db.get_read_receipt(n.id, "#a").unwrap().is_none());
    let mut r = ReadReceipt {
        target: "#a".into(),
        timestamp: Some(Utc::now()),
        ..Default::default()
    };
    db.store_read_receipt(n.id, &mut r).unwrap();
    db.store_read_receipt(n.id, &mut r).unwrap();
    assert_eq!(db.get_read_receipt(n.id, "#a").unwrap().unwrap().id, r.id);

    let mut mt = db.get_message_target(n.id, "#a").unwrap();
    assert_eq!(mt.id, 0);
    mt.pinned = true;
    db.store_message_target(n.id, &mut mt).unwrap();
    assert_eq!(db.list_message_targets(n.id).unwrap(), vec![mt.clone()]);
    mt.pinned = false;
    db.store_message_target(n.id, &mut mt).unwrap();
    assert!(db.list_message_targets(n.id).unwrap().is_empty());

    let mut cfg = WebPushConfig {
        vapid_public: "pub".into(),
        vapid_private: "priv".into(),
        ..Default::default()
    };
    db.store_web_push_config(&mut cfg).unwrap();
    assert_eq!(db.list_web_push_configs().unwrap(), vec![cfg]);
    let mut sub = WebPushSubscription {
        endpoint: "https://push.example.org/x".into(),
        ..Default::default()
    };
    db.store_web_push_subscription(u.id, 0, &mut sub).unwrap();
    assert_eq!(db.list_web_push_subscriptions(u.id, 0).unwrap().len(), 1);
    assert_eq!(db.list_web_push_networks(u.id).unwrap(), [0]);
    assert!(
        db.list_web_push_subscriptions(u.id, n.id)
            .unwrap()
            .is_empty()
    );
    db.delete_web_push_subscription(sub.id).unwrap();
    assert!(db.list_web_push_subscriptions(u.id, 0).unwrap().is_empty());
    assert!(db.list_web_push_networks(u.id).unwrap().is_empty());

    let mut cert = DeviceCertificate {
        label: "laptop".into(),
        fingerprint: vec![7; 64],
        ..Default::default()
    };
    cert.mark_used(Some("127.0.0.1:1234".parse().unwrap()));
    db.store_device_certificate(u.id, &mut cert).unwrap();
    let mut dup = DeviceCertificate {
        fingerprint: vec![7; 64],
        ..Default::default()
    };
    dup.mark_used(None);
    let err = db.store_device_certificate(u.id, &mut dup).unwrap_err();
    assert!(err.is::<DuplicateDeviceCertificate>());
    let (owner, got) = db.get_device_certificate(&[7; 64]).unwrap().unwrap();
    assert_eq!(owner, u.id);
    assert_eq!(got.last_ip, "127.0.0.1");
    assert_eq!(db.list_device_certificates(u.id).unwrap().len(), 1);
    db.delete_device_certificate(u.id, &[7; 64]).unwrap();
    assert!(db.get_device_certificate(&[7; 64]).unwrap().is_none());

    assert_eq!(
        db.stats().unwrap(),
        DatabaseStats {
            users: 1,
            networks: 1,
            channels: 0
        }
    );
}

#[test]
fn fts_quote() {
    assert_eq!(quote_fts_query("foo bar"), "\"foo\" \"bar\"");
    assert_eq!(quote_fts_query("a\"b OR c*"), "\"a\" \"b\" \"OR\" \"c\"");
    assert_eq!(quote_fts_query(""), "");
}

// SQLite version 0 schema, from upstream. DO NOT EDIT.
const SQLITE_V0_SCHEMA: &str = "
CREATE TABLE User (
	username VARCHAR(255) NOT NULL UNIQUE,
	password VARCHAR(255)
);

CREATE TABLE Network (
	id INTEGER PRIMARY KEY,
	name VARCHAR(255),
	user VARCHAR(255) NOT NULL,
	addr VARCHAR(255) NOT NULL,
	nick VARCHAR(255) NOT NULL,
	username VARCHAR(255),
	realname VARCHAR(255),
	pass VARCHAR(255),
	sasl_mechanism VARCHAR(255),
	sasl_plain_username VARCHAR(255),
	sasl_plain_password VARCHAR(255),
	UNIQUE(user, addr, nick),
	UNIQUE(user, name)
);

CREATE TABLE Channel (
	id INTEGER PRIMARY KEY,
	network INTEGER NOT NULL,
	name VARCHAR(255) NOT NULL,
	key VARCHAR(255),
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, name)
);

PRAGMA user_version = 1;
";

#[test]
fn migrations() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SQLITE_V0_SCHEMA).unwrap();
    conn.execute_batch(
        "INSERT INTO User(username, password) VALUES ('alice', NULL);
        INSERT INTO Network(name, user, addr, nick) VALUES ('net', 'alice', 'irc.example.org', 'alice');
        INSERT INTO Channel(network, name) VALUES (1, '#soju');",
    )
    .unwrap();
    let db = Database::setup(conn).unwrap();
    let users = db.list_users().unwrap();
    assert_eq!(users.len(), 1);
    let nets = db.list_networks(users[0].id).unwrap();
    assert_eq!(nets[0].name, "net");
    assert!(nets[0].enabled && nets[0].auto_away);
    assert_eq!(db.list_channels(nets[0].id).unwrap()[0].name, "#soju");
    let version: i64 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 33);
}

#[test]
fn message_text_index_migration() {
    for manual_index in [false, true] {
        // Build the previous schema using its original migrations.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SQLITE_V0_SCHEMA).unwrap();
        for migration in &MIGRATIONS[1..30] {
            conn.execute_batch(migration).unwrap();
        }
        conn.execute_batch(
            "PRAGMA user_version = 30;
            INSERT INTO User(id, username) VALUES (1, 'alice');
            INSERT INTO Network(id, user, addr) VALUES (1, 1, 'irc.example.org');
            INSERT INTO MessageTarget(id, network, target) VALUES (1, 1, '#a');
            INSERT INTO Message(target, raw, time, sender, text) VALUES
                (1, 'PRIVMSG #a :hello', '2024-01-01T00:00:00.000Z', '', 'hello'),
                (1, 'JOIN #a', '2024-01-01T00:00:01.000Z', '', NULL);",
        )
        .unwrap();
        if manual_index {
            conn.execute_batch(
                "CREATE INDEX MessageTextIndex ON Message(target, time) WHERE text IS NOT NULL;",
            )
            .unwrap();
        }

        let db = Database::setup(conn).unwrap();
        let opts = MessageOptions {
            limit: 10,
            take_last: true,
            ..Default::default()
        };
        let messages = db.list_messages(1, "#a", &opts).unwrap();
        assert_eq!(messages, vec![msg!("PRIVMSG", "#a", "hello")]);
        let targets = db.list_message_last_per_target(1, &opts).unwrap();
        assert_eq!(
            targets[0].latest_message,
            xirc::parse_server_time("2024-01-01T00:00:00.000Z")
        );
        assert_eq!(
            db.list_messages(
                1,
                "#a",
                &MessageOptions {
                    events: true,
                    ..opts
                }
            )
            .unwrap()
            .len(),
            2
        );

        let conn = db.conn();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let stat: String = conn
            .query_row(
                "SELECT stat FROM sqlite_stat1 WHERE idx = 'MessageTextIndex'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Only the text message is indexed, and the migration analyzed it.
        assert_eq!(stat, "1 1 1");
        drop(conn);
        // Opening an already upgraded database is harmless.
        db.upgrade().unwrap();
    }
}

#[test]
fn message_text_index_query_plans() {
    let db = db();
    let conn = db.conn();
    conn.execute_batch(
        "INSERT INTO User(id, username, created_at) VALUES (1, 'alice', '2024-01-01T00:00:00.000Z');
        INSERT INTO Network(id, user, addr) VALUES (1, 1, 'irc.example.org');
        WITH RECURSIVE targets(id) AS (
            VALUES (1) UNION ALL SELECT id + 1 FROM targets WHERE id < 16
        )
        INSERT INTO MessageTarget(id, network, target)
            SELECT id, 1, '#channel' || id FROM targets;
        WITH RECURSIVE messages(id) AS (
            VALUES (1) UNION ALL SELECT id + 1 FROM messages WHERE id < 100
        )
        INSERT INTO Message(target, raw, time, sender, text)
            SELECT t.id, 'raw', printf('2024-01-01T00:%02d:%02d.000Z', m.id / 60, m.id % 60),
                '', CASE WHEN m.id % 10 = 0 THEN 'hello' END
            FROM MessageTarget t, messages m;",
    )
    .unwrap();

    // Check both a fresh database without statistics and one with statistics.
    for analyze in [false, true] {
        if analyze {
            conn.execute_batch("ANALYZE;").unwrap();
        }
        for events in [false, true] {
            let filter = if events {
                "text IS NULL"
            } else {
                "text IS NOT NULL"
            };
            for query in [
                format!(
                    "SELECT target, (SELECT time FROM Message WHERE target=MessageTarget.id AND {filter} ORDER BY time DESC LIMIT 1) latest FROM MessageTarget WHERE network=1 ORDER BY latest DESC LIMIT 10"
                ),
                format!(
                    "SELECT m.raw FROM Message m,MessageTarget t WHERE m.target=t.id AND t.network=1 AND t.target='#channel1' AND m.{filter} ORDER BY m.time DESC,m.id DESC LIMIT 5"
                ),
            ] {
                let plan = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                    .unwrap()
                    .query_map([], |r| r.get::<_, String>(3))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                let index = if events {
                    "MessageEventIndex"
                } else {
                    "MessageTextIndex"
                };
                assert!(
                    plan.iter().any(|detail| detail.contains(index)),
                    "expected {index} (analyze={analyze}): {plan:?}"
                );
            }
        }
    }
}

#[test]
fn newer_schema_rejected() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA user_version = 999").unwrap();
    let err = Database::setup(conn).err().unwrap();
    assert!(err.to_string().contains("older than schema"));
}

#[test]
fn unchanged_records_and_receipts_do_not_write() {
    let db = db();
    let mut user = User::new("cost-test");
    db.store_user(&mut user).unwrap();
    let mut network = Network::new("irc.cost-test");
    db.store_network(user.id, &mut network).unwrap();
    let mut channel = Channel {
        name: "#cost".into(),
        ..Default::default()
    };
    db.store_channel(network.id, &mut channel).unwrap();
    let mut target = db.get_message_target(network.id, "#cost").unwrap();
    db.store_message_target(network.id, &mut target).unwrap();
    let mut receipt = ReadReceipt {
        target: "#cost".into(),
        timestamp: Some(Utc::now()),
        ..Default::default()
    };
    db.store_read_receipt(network.id, &mut receipt).unwrap();
    let mut cert = DeviceCertificate {
        label: "cost".into(),
        fingerprint: vec![17; 64],
        last_used: Some(Utc::now()),
        ..Default::default()
    };
    db.store_device_certificate(user.id, &mut cert).unwrap();
    let mut delivered = vec![DeliveryReceipt {
        target: "#cost".into(),
        internal_msg_id: "same".into(),
        ..Default::default()
    }];
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    let cursor = delivered[0].id;
    let before: i64 = db
        .conn()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    db.store_user(&mut user).unwrap();
    db.store_user_keep_password(&user).unwrap();
    db.store_network(user.id, &mut network).unwrap();
    db.store_channel(network.id, &mut channel).unwrap();
    db.store_message_target(network.id, &mut target).unwrap();
    db.store_read_receipt(network.id, &mut receipt).unwrap();
    db.store_device_certificate(user.id, &mut cert).unwrap();
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    let after: i64 = db
        .conn()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(delivered[0].id, cursor);
    delivered[0].internal_msg_id = "changed".into();
    delivered.push(DeliveryReceipt {
        target: "#new".into(),
        internal_msg_id: "new".into(),
        ..Default::default()
    });
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    assert_eq!(delivered[0].id, cursor);
    assert_ne!(delivered[1].id, 0);
    delivered.remove(0);
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    assert_eq!(db.list_delivery_receipts(network.id).unwrap().len(), 1);
}

#[test]
fn sparse_fts_preserves_updates_and_event_history_ordering() {
    let db = db();
    let mut user = User::new("fts-test");
    db.store_user(&mut user).unwrap();
    let mut network = Network::new("irc.fts-test");
    db.store_network(user.id, &mut network).unwrap();
    let at = |m: Message| m.with_tag("time", "2024-01-01T00:00:00.000Z");
    let messages = vec![
        at(msg!("PRIVMSG", "#cost", "hello")),
        at(msg!("JOIN", "#cost")),
        at(msg!("NOTICE", "#cost", "world")),
    ];
    let ids = db.store_messages(network.id, "#cost", &messages).unwrap();
    for (events, wanted) in [
        (true, messages.clone()),
        (false, vec![messages[0].clone(), messages[2].clone()]),
    ] {
        for take_last in [false, true] {
            let actual = db
                .list_messages(
                    network.id,
                    "#cost",
                    &MessageOptions {
                        limit: 2,
                        events,
                        take_last,
                        ..Default::default()
                    },
                )
                .unwrap();
            let expected = if take_last {
                wanted[wanted.len() - 2..].to_vec()
            } else {
                wanted[..2].to_vec()
            };
            assert_eq!(actual, expected);
        }
    }
    let conn = db.conn();
    conn.execute("UPDATE Message SET text=NULL WHERE id=?", [ids[0]])
        .unwrap();
    conn.execute("UPDATE Message SET text='fresh' WHERE id=?", [ids[1]])
        .unwrap();
    conn.execute("UPDATE Message SET id=999 WHERE id=?", [ids[1]])
        .unwrap();
    let id: i64 = conn
        .query_row(
            "SELECT rowid FROM MessageFTS WHERE MessageFTS MATCH 'fresh'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(id, 999);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM MessageFTS WHERE MessageFTS MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    conn.execute(
        "INSERT INTO MessageFTS(MessageFTS,rank)VALUES('integrity-check',1)",
        [],
    )
    .unwrap();
}
