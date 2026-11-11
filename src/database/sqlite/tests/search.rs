//! Search regression scenarios.

use super::*;

#[test]
fn fts_quote() {
    assert_eq!(quote_fts_query("foo bar"), "\"foo\" \"bar\"");
    assert_eq!(quote_fts_query("a\"b OR c*"), "\"a\" \"b\" \"OR\" \"c\"");
    assert_eq!(quote_fts_query(""), "");
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
        INSERT INTO Message(id, target, raw, time, sender, text)
            SELECT t.id * 1000 + m.id, t.id, 'raw',
                printf('2024-01-01T00:%02d:%02d.000Z', m.id / 60, m.id % 60),
                '', CASE WHEN m.id % 10 = 0 THEN 'hello' END
            FROM MessageTarget t, messages m;",
    )
    .unwrap();

    // Check both a fresh database without statistics and one with statistics.
    for analyze in [false, true] {
        if analyze {
            conn.execute_batch("ANALYZE;").unwrap();
        }
        // The SQL of list_message_last_per_target and list_message_rows
        for (query, index) in [
            (
                "SELECT target, (SELECT time FROM Message INDEXED BY MessageTextIndex WHERE \
                    target=MessageTarget.id AND text IS NOT NULL ORDER BY time DESC LIMIT 1) \
                    latest FROM MessageTarget WHERE network=1 ORDER BY latest DESC LIMIT 10",
                "MessageTextIndex",
            ),
            (
                "SELECT target, (SELECT time FROM Message WHERE target=MessageTarget.id ORDER BY \
                    time DESC LIMIT 1) latest FROM MessageTarget WHERE network=1 ORDER BY latest \
                    DESC LIMIT 10",
                "PRIMARY KEY",
            ),
            (
                "SELECT m.raw FROM Message AS m INDEXED BY MessageTextIndex WHERE m.target=(SELECT \
                    id FROM MessageTarget WHERE network=1 AND target='#channel1') AND m.text IS \
                    NOT NULL ORDER BY time DESC, id DESC LIMIT 5",
                "MessageTextIndex",
            ),
            (
                "SELECT m.raw FROM Message AS m WHERE m.target=(SELECT id FROM MessageTarget WHERE \
                    network=1 AND target='#channel1') ORDER BY time DESC, id DESC LIMIT 5",
                "PRIMARY KEY",
            ),
            (
                "SELECT id FROM Message WHERE target=1 AND text IS NOT NULL ORDER BY id DESC LIMIT \
                    1",
                "MessageTextIdIndex",
            ),
            (
                "SELECT m.id FROM Message AS m WHERE m.target=1 AND (m.time, m.id) IN (SELECT \
                    time, id FROM Message WHERE target=1 AND text IS NOT NULL AND id > 1000) AND \
                    m.text IS NOT NULL ORDER BY time ASC, id ASC LIMIT 5",
                "MessageTextIdIndex",
            ),
            (
                "SELECT m.target, m.time, m.id FROM MessageTarget t CROSS JOIN Message m ON \
                    m.target = t.id WHERE m.time < '2024' LIMIT 10",
                "PRIMARY KEY (target=? AND time<?)",
            ),
        ] {
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                plan.iter().any(|detail| detail.contains(index)),
                "expected {index} (analyze={analyze}): {query}: {plan:?}"
            );
            // History doesn't sort: the indexes are in time and ID order.
            // Backlog sorts the messages after its cursor, and target
            // lists their latest times.
            if !query.contains("latest") && !query.contains(" IN (") {
                assert!(
                    !plan.iter().any(|detail| detail.contains("TEMP B-TREE")),
                    "unexpected sort (analyze={analyze}): {query}: {plan:?}"
                );
            }
        }
    }
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
