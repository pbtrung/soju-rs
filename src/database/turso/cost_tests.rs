//! Real engine counters; driven only against a disposable localhost server.
use super::{http::Http, *};
use crate::database::{MessageInsert, MessageOptions, MessageWriter};
use crate::msg;

fn rows(start: i64, events: bool) -> Vec<MessageInsert> {
    (start..start + 200)
        .map(|id| {
            let message = if events {
                msg!("JOIN", "#bench")
            } else {
                msg!("PRIVMSG", "#bench", format!("cost fixture {id}"))
            };
            MessageInsert::new(
                id,
                1,
                "#bench",
                &message.with_tag("time", "2026-10-01T00:00:00.000Z"),
            )
            .unwrap()
        })
        .collect()
}

fn observe(
    http: &Http,
    label: &str,
    op: impl FnOnce() -> Result<()>,
    out: &mut Vec<serde_json::Value>,
) {
    let before = http.observed_usage();
    op().unwrap();
    let after = http.observed_usage();
    out.push(json!({"case":label,"rows_read":after.0-before.0,"rows_written":after.1-before.1}));
}

fn before_write(conn: &Connection, rows: &[MessageInsert]) -> Result<()> {
    let sql = include_str!("../../../tests/fixtures/turso-insert-before.sql");
    let mut stmts = vec![crate::database::connection::statement(
        "INSERT INTO MessageTarget(network,target) SELECT 1,'#bench' WHERE EXISTS(SELECT 1 FROM Network WHERE id=1) ON CONFLICT(network,target) DO NOTHING",
        [],
    )?];
    for row in rows {
        let id = row.id;
        let event = id >= 201;
        let message = if event {
            msg!("JOIN", "#bench")
        } else {
            msg!("PRIVMSG", "#bench", format!("cost fixture {id}"))
        };
        let raw = message
            .with_tag("time", "2026-10-01T00:00:00.000Z")
            .to_string();
        let time = "2026-10-01T00:00:00.000Z";
        let sender = "";
        let text = if event {
            None
        } else {
            Some(format!("cost fixture {id}"))
        };
        stmts.push(crate::database::connection::statement(sql,named_params!{
            ":id":row.id,":network":1,":target":"#bench",":raw":raw.as_bytes(),":time":time,":sender":sender,":text":text.as_ref().map(|s|s.as_bytes())
        })?);
    }
    conn.atomic_batch(stmts)?;
    Ok(())
}

#[test]
#[ignore = "requires scripts/bench-query-cost.py and a disposable local libSQL server"]
fn real_engine_query_cost() {
    let endpoint = std::env::var("SOJU_QUERY_COST_URL").expect("run scripts/bench-query-cost.py");
    let url = reqwest::Url::parse(&endpoint).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    let http = Http::new().unwrap();
    let conn = Connection::remote(http.clone(), endpoint.clone(), String::new());
    conn.execute_batch(include_str!("../../../tests/fixtures/turso-schema-31.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE MessageIdHighWater(id INTEGER PRIMARY KEY,last_id INTEGER NOT NULL);
        INSERT INTO User(id,username,created_at) VALUES(1,'user-1','');
        INSERT INTO Network(id,user,addr)VALUES(1,1,'irc');
        INSERT INTO MessageTarget(id,network,target)VALUES(1,1,'#bench');",
    )
    .unwrap();
    let mut output = Vec::new();
    observe(
        &http,
        "before_text_200",
        || before_write(&conn, &rows(1, false)),
        &mut output,
    );
    observe(
        &http,
        "before_identical_retry_200",
        || before_write(&conn, &rows(1, false)),
        &mut output,
    );
    observe(
        &http,
        "before_events_200",
        || before_write(&conn, &rows(201, true)),
        &mut output,
    );
    let receipt_fixture="WITH RECURSIVE t(i)AS(VALUES(1) UNION ALL SELECT i+1 FROM t WHERE i<200),
        c(i)AS(VALUES(0) UNION ALL SELECT i+1 FROM c WHERE i<9)
        INSERT INTO DeliveryReceipt(network,target,client,internal_msgid) SELECT 1,'#'||t.i,'c'||c.i,'same' FROM t,c;
        INSERT INTO ReadReceipt(id,network,target,timestamp)VALUES(1,1,'#bench','2026-10-01T00:00:00.000Z');";
    conn.execute_batch(receipt_fixture).unwrap();
    observe(
        &http,
        "before_unchanged_200_of_2000_receipts",
        || {
            let mut statements = vec![crate::database::connection::statement(
                "DELETE FROM DeliveryReceipt WHERE network=1 AND client='c0'",
                [],
            )?];
            for i in 1..=200 {
                statements.push(crate::database::connection::statement("INSERT INTO DeliveryReceipt(network,target,client,internal_msgid)VALUES(1,?,'c0','same')",[format!("#{i}")])?);
            }
            conn.atomic_batch(statements)?;
            Ok(())
        },
        &mut output,
    );
    observe(
        &http,
        "before_unchanged_read_receipt",
        || {
            conn.execute(
                "UPDATE ReadReceipt SET timestamp='2026-10-01T00:00:00.000Z' WHERE id=1",
                [],
            )?;
            Ok(())
        },
        &mut output,
    );
    let fixture="WITH RECURSIVE t(id) AS(VALUES(2) UNION ALL SELECT id+1 FROM t WHERE id<100)
        INSERT INTO MessageTarget(id,network,target) SELECT id,1,'#target'||id FROM t;
        WITH RECURSIVE m(id) AS(VALUES(1000) UNION ALL SELECT id+1 FROM m WHERE id<100999)
        INSERT INTO Message(id,target,raw,time,sender,text) SELECT id,1+(id%100),'JOIN #fixture','2026-10-01T00:00:00.000Z','',NULL FROM m;";
    conn.execute_batch(fixture).unwrap();
    observe(
        &http,
        "before_expiry_zero_of_100400",
        || {
            conn.atomic_batch(vec![
            crate::database::connection::statement("DELETE FROM Message WHERE id IN(SELECT id FROM Message WHERE time<'2000-01-01' LIMIT 1000)",[])?,
        ])?;
            Ok(())
        },
        &mut output,
    );
    conn.execute_batch("CREATE TABLE SojuSchemaVersion(id INTEGER PRIMARY KEY CHECK(id=0),version INTEGER NOT NULL); INSERT INTO SojuSchemaVersion VALUES(0,31);").unwrap();
    let mut migrated = None;
    observe(
        &http,
        "one_time_migration_100400",
        || {
            migrated = Some(sqlite::Database::open_remote_tenant(
                Connection::remote(http.clone(), endpoint.clone(), String::new()),
                "cost-controller",
                1,
            )?);
            Ok(())
        },
        &mut output,
    );
    let migrated = migrated.unwrap();
    let count: i64 = migrated
        .conn()
        .query_row("SELECT count(*) FROM Message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 100400);
    let user_exists: bool = migrated
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='User')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!user_exists);
    migrated
        .conn()
        .execute_batch("INSERT INTO MessageFTS(MessageFTS,rank)VALUES('integrity-check',1)")
        .unwrap();
    drop(migrated);
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.execute_batch(
        "DROP TABLE IF EXISTS MessageFTS;DROP TABLE IF EXISTS Message;DROP TABLE IF EXISTS MessageTarget;
        DROP TABLE IF EXISTS Channel;DROP TABLE IF EXISTS DeviceCertificate;DROP TABLE IF EXISTS WebPushSubscription;
        DROP TABLE IF EXISTS DeliveryReceipt;DROP TABLE IF EXISTS ReadReceipt;DROP TABLE IF EXISTS Network;
        DROP TABLE IF EXISTS WebPushConfig;DROP TABLE IF EXISTS User;DROP TABLE IF EXISTS MessageIdHighWater;
        DROP TABLE IF EXISTS TursoOwner;DROP TABLE IF EXISTS SojuSchemaVersion;",
    )
    .unwrap();
    drop(conn);
    let db = sqlite::Database::open_remote_tenant(
        Connection::remote(http.clone(), endpoint, String::new()),
        "cost-controller",
        1,
    )
    .unwrap();
    db.conn().execute_batch("INSERT INTO Network(id,user,addr)VALUES(1,1,'irc'); INSERT INTO MessageTarget(id,network,target)VALUES(1,1,'#bench');").unwrap();
    let writer = MessageWriter::new(db.conn().fork_remote().unwrap());
    observe(
        &http,
        "after_text_200_cold_target",
        || writer.write(&rows(1, false)).map(|n| assert_eq!(n, 0)),
        &mut output,
    );
    observe(
        &http,
        "after_identical_retry_200",
        || writer.write(&rows(1, false)).map(|n| assert_eq!(n, 0)),
        &mut output,
    );
    observe(
        &http,
        "after_events_200",
        || writer.write(&rows(201, true)).map(|n| assert_eq!(n, 0)),
        &mut output,
    );
    db.conn().execute_batch(receipt_fixture).unwrap();
    let mut receipts: Vec<_> = (1..=200)
        .map(|i| crate::database::DeliveryReceipt {
            target: format!("#{i}"),
            internal_msg_id: "same".into(),
            ..Default::default()
        })
        .collect();
    observe(
        &http,
        "after_unchanged_200_of_2000_receipts",
        || db.store_client_delivery_receipts(1, "c0", &mut receipts),
        &mut output,
    );
    receipts[0].internal_msg_id = "updated".into();
    receipts.pop();
    receipts.push(crate::database::DeliveryReceipt {
        target: "#new".into(),
        internal_msg_id: "new".into(),
        ..Default::default()
    });
    observe(
        &http,
        "after_three_receipt_changes",
        || db.store_client_delivery_receipts(1, "c0", &mut receipts),
        &mut output,
    );
    let mut receipt = crate::database::ReadReceipt {
        id: 1,
        target: "#bench".into(),
        timestamp: crate::xirc::parse_server_time("2026-10-01T00:00:00.000Z"),
    };
    observe(
        &http,
        "after_unchanged_read_receipt",
        || db.store_read_receipt(1, &mut receipt),
        &mut output,
    );
    db.conn().execute_batch(fixture).unwrap();
    observe(
        &http,
        "after_expiry_zero_of_100400",
        || {
            assert_eq!(
                db.delete_messages_before(
                    crate::xirc::parse_server_time("2000-01-01T00:00:00.000Z").unwrap(),
                    1000
                )?,
                0
            );
            Ok(())
        },
        &mut output,
    );
    let db = &db;
    let history = |target: &'static str, options: MessageOptions| {
        move || -> Result<()> {
            db.list_messages(1, target, &options)?;
            Ok(())
        }
    };
    let latest = |events| MessageOptions {
        limit: 50,
        events,
        take_last: true,
        ..Default::default()
    };
    observe(
        &http,
        "after_event_history_50_of_1000",
        history("#target2", latest(true)),
        &mut output,
    );
    observe(
        &http,
        "after_text_history_50_behind_1000_events",
        history("#bench", latest(false)),
        &mut output,
    );
    let mut last_id = 0;
    observe(
        &http,
        "after_last_text_id",
        || {
            last_id = db.get_message_last_id(1, "#bench")?;
            Ok(())
        },
        &mut output,
    );
    assert_eq!(last_id, 200);
    observe(
        &http,
        "after_backlog_after_last_id",
        history(
            "#bench",
            MessageOptions {
                after_id: last_id,
                limit: 1000,
                take_last: true,
                ..Default::default()
            },
        ),
        &mut output,
    );
    observe(
        &http,
        "after_backlog_10_new",
        history(
            "#bench",
            MessageOptions {
                after_id: last_id - 10,
                limit: 1000,
                take_last: true,
                ..Default::default()
            },
        ),
        &mut output,
    );
    observe(
        &http,
        "after_search_1_of_200",
        history(
            "#bench",
            MessageOptions {
                text: "fixture 150".into(),
                limit: 50,
                ..Default::default()
            },
        ),
        &mut output,
    );
    db.conn()
        .execute_batch("INSERT INTO MessageFTS(MessageFTS,rank)VALUES('integrity-check',1)")
        .unwrap();
    let value = |case: &str, key: &str| {
        output.iter().find(|r| r["case"] == case).unwrap()[key]
            .as_u64()
            .unwrap()
    };
    println!(
        "SOJU_QUERY_COST {}",
        json!({"measurements":output,"fixture_messages":100400})
    );
    assert_eq!(value("after_identical_retry_200", "rows_written"), 0);
    // One clustered row per event; text adds its ID index and search entry
    assert!(value("after_events_200", "rows_written") <= 200);
    assert!(value("after_text_200_cold_target", "rows_written") <= 602);
    for case in ["after_event_history_50_of_1000", "after_search_1_of_200"] {
        assert!(value(case, "rows_read") <= 60, "{case}");
    }
    // Text-only history steps over newer events in the clustered table
    assert!(value("after_text_history_50_behind_1000_events", "rows_read") <= 1300);
    assert!(value("after_last_text_id", "rows_read") <= 2);
    assert!(value("after_backlog_after_last_id", "rows_read") <= 6);
    assert!(value("after_backlog_10_new", "rows_read") <= 50);
    assert_eq!(
        value("after_unchanged_200_of_2000_receipts", "rows_written"),
        0
    );
    for (before, after) in [
        ("before_text_200", "after_text_200_cold_target"),
        ("before_events_200", "after_events_200"),
    ] {
        for key in ["rows_read", "rows_written"] {
            assert!(
                value(after, key) < value(before, key),
                "{after} did not reduce {key}"
            );
        }
    }
    assert!(
        value("after_expiry_zero_of_100400", "rows_read")
            < value("before_expiry_zero_of_100400", "rows_read") / 10
    );
}
