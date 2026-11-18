//! Historical schemas, real concurrent initializers and transactional integrity.
use super::{Connection, Database, MIGRATIONS, SCHEMA_VERSION};
use crate::database::{MessageOptions, Network, User};
use crate::msg;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

struct Directory(PathBuf);
impl Directory {
    fn new() -> anyhow::Result<Self> {
        let path =
            std::env::temp_dir().join(format!("soju-stress-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "Temporary test cleanup must not panic during unwinding."
        )]
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn historical(path: &Path, version: usize) -> anyhow::Result<()> {
    let connection = Connection::open(path)?;
    connection.execute_batch(super::migrations::SQLITE_V0_SCHEMA)?;
    for migration in MIGRATIONS.iter().skip(1).take(version - 1) {
        connection.execute_batch(migration)?;
    }
    connection.pragma_update(None, "user_version", i64::try_from(version)?)?;
    // Reproduce the original application's FTS rebuild hook after schema 32,
    // without modifying historical migration text or fixture contents.
    if version >= 32 {
        connection.execute_batch("DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; DROP TRIGGER MessageFTSUpdate; DROP TABLE MessageFTS;")?;
        connection.execute_batch(super::FTS_SCHEMA)?;
    }
    connection.execute_batch(
        "INSERT INTO User(id,username) VALUES(1,'alice');
        INSERT INTO Network(id,user,addr) VALUES(1,1,'irc.example');
        INSERT INTO MessageTarget(id,network,target) VALUES(1,1,'#room');
        INSERT INTO Message(id,target,time,raw,sender,text) VALUES
          (41,1,'2024-01-01T00:00:00.000Z','PRIVMSG #room :retained','bob','retained');
        INSERT INTO DeliveryReceipt(network,target,client,internal_msgid)
          VALUES(1,'#room','phone','retained-cursor');",
    )?;
    Ok(())
}

fn integrity(database: &Database) -> anyhow::Result<()> {
    let connection = database.conn();
    assert_eq!(
        connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?,
        "ok"
    );
    assert_eq!(
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get::<_, i64>(0)
        })?,
        0
    );
    connection
        .execute_batch("INSERT INTO MessageFTS(MessageFTS,rank) VALUES('integrity-check',1);")?;
    assert_eq!(
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
        SCHEMA_VERSION
    );
    Ok(())
}

#[test]
fn schemas_30_through_35_preserve_history_receipts_and_search() -> anyhow::Result<()> {
    let directory = Directory::new()?;
    for version in 30..=35 {
        let path = directory.0.join(format!("schema-{version}.db"));
        historical(&path, version)?;
        let source = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 fixture path"))?;
        for reopen in 0..2 {
            let database = Database::open(source)?;
            let messages = database.list_messages(
                1,
                "#room",
                &MessageOptions {
                    text: "retained".into(),
                    limit: 10,
                    ..Default::default()
                },
            )?;
            assert_eq!(messages.len(), 1, "schema={version} reopen={reopen}");
            assert_eq!(
                messages.first().map(|message| message.param(1)),
                Some("retained")
            );
            assert_eq!(
                database
                    .list_delivery_receipts(1)?
                    .first()
                    .map(|receipt| receipt.internal_msg_id.as_str()),
                Some("retained-cursor")
            );
            integrity(&database)?;
        }
    }
    Ok(())
}

#[test]
fn concurrent_file_initialization_and_upgrade_is_serialized() -> anyhow::Result<()> {
    let directory = Directory::new()?;
    for previous in [None, Some(30), Some(34)] {
        let path = directory.0.join(format!("concurrent-{previous:?}.db"));
        if let Some(version) = previous {
            historical(&path, version)?;
        }
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || -> anyhow::Result<()> {
                    let connection = Connection::open(path)?;
                    connection.busy_timeout(std::time::Duration::from_secs(5))?;
                    barrier.wait();
                    let database = Database::setup(connection)?;
                    let mut user = User::new(&format!("initializer-{index}"));
                    database.store_user(&mut user)?;
                    Ok(())
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .map_err(|error| anyhow::anyhow!("initializer panicked: {error:?}"))
                    .and_then(|result| result)
            })
            .collect();
        for result in results {
            result?;
        }
        let source = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 fixture path"))?;
        let database = Database::open(source)?;
        let initialized = database
            .list_users()?
            .into_iter()
            .filter(|user| user.username.starts_with("initializer-"))
            .count();
        assert_eq!(initialized, 8);
        integrity(&database)?;
    }
    Ok(())
}

#[test]
fn failed_upgrade_rolls_back_schema_data_and_version() -> anyhow::Result<()> {
    let directory = Directory::new()?;
    let path = directory.0.join("rollback.db");
    historical(&path, 30)?;
    let mut connection = Connection::open(path)?;
    connection.execute_batch("CREATE TABLE MessageNew(obstruction INTEGER);")?;
    assert!(Database::run_migrations(&mut connection).is_err());
    assert_eq!(
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
        30
    );
    assert_eq!(
        connection.query_row("SELECT text FROM Message WHERE id=41", [], |row| row
            .get::<_, String>(0))?,
        "retained"
    );
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='MessageTextIndex'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    connection.execute_batch("DROP TABLE MessageNew;")?;
    let database = Database::setup(connection)?;
    integrity(&database)?;
    Ok(())
}

#[test]
fn failed_message_batch_rolls_back_fts_ids_and_foreign_keys() -> anyhow::Result<()> {
    let database = Database::open_in_memory()?;
    let mut user = User::new("rollback");
    database.store_user(&mut user)?;
    let mut network = Network::new("irc.example");
    database.store_network(user.id, &mut network)?;
    database.conn().execute_batch(
        "CREATE TRIGGER reject_message BEFORE INSERT ON Message
        WHEN new.text='reject' BEGIN SELECT RAISE(ABORT,'injected batch failure'); END;",
    )?;
    assert!(
        database
            .store_messages(
                network.id,
                "#room",
                &[
                    msg!("PRIVMSG", "#room", "first"),
                    msg!("PRIVMSG", "#room", "reject")
                ]
            )
            .is_err()
    );
    assert_eq!(database.get_message_last_id(network.id, "#room")?, 0);
    database
        .conn()
        .execute_batch("DROP TRIGGER reject_message;")?;
    assert_eq!(
        database.store_messages(network.id, "#room", &[msg!("PRIVMSG", "#room", "retained")])?,
        vec![1]
    );
    assert!(
        database
            .conn()
            .execute("DELETE FROM User WHERE id=?1", [user.id])
            .is_err()
    );
    assert!(database.conn().execute("INSERT INTO Message(target,time,id,raw,sender,text) VALUES(-1,'',2,'','','orphan')",[]).is_err());
    integrity(&database)?;
    Ok(())
}
