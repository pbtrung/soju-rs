//! Migrations regression scenarios.

use super::*;

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
        INSERT INTO Network(name, user, addr, nick) VALUES ('net', 'alice', 'irc.example.org', \
            'alice');
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
    assert_eq!(SCHEMA_VERSION, 35);
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
                    ..opts.clone()
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
        // Schema 34 clustered the table, keeping IDs and the search index
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'Message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("WITHOUT ROWID"), "{sql}");
        let ids = conn
            .prepare("SELECT id FROM Message ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, vec![1, 2]);
        conn.execute_batch("INSERT INTO MessageFTS(MessageFTS) VALUES ('integrity-check');")
            .unwrap();
        drop(conn);
        let found = db
            .list_messages(
                1,
                "#a",
                &MessageOptions {
                    text: "hello".into(),
                    ..opts.clone()
                },
            )
            .unwrap();
        assert_eq!(found.len(), 1);
        // New IDs continue after the highest stored one
        assert_eq!(
            db.store_messages(1, "#a", &[msg!("PRIVMSG", "#a", "again")])
                .unwrap(),
            vec![3]
        );
        // Opening an already upgraded database is harmless.
        db.upgrade().unwrap();
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
fn repeated_migration_after_another_initializer_is_a_noop() {
    let mut connection = Connection::open_in_memory().unwrap();
    Database::run_migrations(&mut connection).unwrap();
    connection
        .execute(
            "INSERT INTO User(username,created_at) VALUES('alice','')",
            [],
        )
        .unwrap();
    Database::run_migrations(&mut connection).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM User WHERE username='alice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}
