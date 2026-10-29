//! Tenant-only schemas and resumable cloud migrations.

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, named_params, params};

use super::{Connection, Database};
use crate::database::connection::statement;

impl Database {
    pub(in crate::database) fn open_remote_tenant(
        conn: Connection,
        controller: &str,
        owner: i64,
    ) -> Result<Self> {
        conn.initialize_remote_schema_version()?;
        let db = Self {
            conn: std::sync::Mutex::new(conn),
        };
        db.check_turso_owner(controller, owner)?;
        db.upgrade()?;
        db.conn().execute_batch("CREATE TABLE IF NOT EXISTS MessageIdHighWater (id INTEGER PRIMARY KEY CHECK(id=0), last_id INTEGER NOT NULL);")?;
        db.rebuild_turso_messages()
            .context("rebuild Turso message table; restart to resume")?;
        db.migrate_turso_search()
            .context("migrate Turso search index; restart to resume")?;
        db.remove_turso_user()?;
        Ok(db)
    }

    fn check_turso_owner(&self, controller: &str, owner: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let legacy: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='User')",
            [],
            |r| r.get(0),
        )?;
        if legacy {
            let invalid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM User WHERE id!=? OR username!=? OR password IS NOT NULL)",
                params![owner, format!("user-{owner}")], |r| r.get(0),
            )?;
            ensure!(
                !invalid,
                "Turso user database belongs to a different account"
            );
        }
        tx.execute_batch("CREATE TABLE IF NOT EXISTS TursoOwner(id INTEGER PRIMARY KEY CHECK(id=0), controller TEXT NOT NULL, user INTEGER NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS TursoOwner_user_index ON TursoOwner(user);")?;
        let saved: Option<(String, i64)> = tx
            .query_row(
                "SELECT controller,user FROM TursoOwner WHERE id=0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((saved_controller, saved_owner)) = saved {
            ensure!(
                saved_controller == controller && saved_owner == owner,
                "Turso user database has a different controller or owner"
            );
        } else {
            ensure!(
                legacy || tx.schema_version()? == 0,
                "Turso user database is missing its owner"
            );
            tx.execute(
                "INSERT INTO TursoOwner(id,controller,user) VALUES(0,?,?)",
                params![controller, owner],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn remove_turso_user(&self) -> Result<()> {
        let mut conn = self.conn();
        let legacy: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='User')",
            [],
            |r| r.get(0),
        )?;
        if !legacy {
            return Ok(());
        }
        conn.pragma_update(None, "foreign_keys", false)?;
        let result = (|| -> Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for table in ["DeviceCertificate", "WebPushConfig"] {
                let has_data: bool =
                    tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                        r.get(0)
                    })?;
                ensure!(
                    !has_data,
                    "unexpected authentication data in tenant {table}; migrate it to the controller before removing User"
                );
            }
            for table in ["Network", "WebPushSubscription"] {
                let invalid: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE user!=(SELECT user FROM TursoOwner WHERE id=0))"), [], |r|r.get(0))?;
                ensure!(!invalid, "tenant {table} belongs to a different owner");
            }
            tx.execute_batch(include_str!("../turso/tenant_user_migration.sql"))?;
            // Only these tables were rebuilt. A database-wide check would
            // unnecessarily scan the entire message history at startup.
            for table in ["Network", "WebPushSubscription"] {
                let violation: Option<String> = tx
                    .query_row(&format!("PRAGMA foreign_key_check({table})"), [], |r| {
                        r.get(0)
                    })
                    .optional()?;
                ensure!(
                    violation.is_none(),
                    "tenant migration would leave a foreign-key violation"
                );
            }
            tx.commit()?;
            Ok(())
        })();
        conn.pragma_update(None, "foreign_keys", true)?;
        result
    }

    /// Copy a rowid Message table into the clustered WITHOUT ROWID layout in
    /// bounded transactions. Message IDs are kept, so the search index stays
    /// valid. Its cursor commits with each chunk, so restart or an ambiguous
    /// response cannot copy a row twice. Startup has not made this tenant
    /// available to writers yet.
    fn rebuild_turso_messages(&self) -> Result<()> {
        let mut conn = self.conn();
        let sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='Message'",
            [],
            |r| r.get(0),
        )?;
        if sql
            .split_whitespace()
            .collect::<String>()
            .to_ascii_uppercase()
            .contains("WITHOUTROWID")
        {
            return Ok(());
        }
        let (table, _) = super::TENANT_SCHEMA
            .split_once("CREATE TABLE Message (")
            .and_then(|(_, rest)| rest.split_once(") WITHOUT ROWID;"))
            .ok_or_else(|| anyhow::anyhow!("missing tenant Message schema"))?;
        // Reservations take over from max(id), which the clustered table
        // can't seek. Older releases inserted without updating this row.
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS MessageNext ({table}) WITHOUT ROWID;
            CREATE UNIQUE INDEX IF NOT EXISTS MessageTextIdIndex ON MessageNext(target, id) WHERE text IS NOT NULL;
            CREATE TABLE IF NOT EXISTS TursoMessageRebuild(id INTEGER PRIMARY KEY CHECK(id=0), message INTEGER NOT NULL);
            INSERT INTO TursoMessageRebuild(id,message) VALUES(0,0) ON CONFLICT DO NOTHING;
            INSERT INTO MessageIdHighWater(id,last_id) SELECT 0,id FROM Message WHERE true ORDER BY id DESC LIMIT 1
                ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id WHERE last_id<excluded.last_id;"
        ))?;
        let mut page = 1000;
        loop {
            let cursor: i64 = conn.query_row(
                "SELECT message FROM TursoMessageRebuild WHERE id=0",
                [],
                |r| r.get(0),
            )?;
            let end: Option<i64> = conn.query_row(
                "SELECT max(id) FROM (SELECT id FROM Message WHERE id>? ORDER BY id LIMIT ?)",
                params![cursor, page],
                |r| r.get(0),
            )?;
            let Some(end) = end else {
                break;
            };
            let result = conn.atomic_batch(vec![
                statement(
                    "INSERT INTO MessageNext(target,time,id,raw,sender,text)
                    SELECT target,time,id,raw,sender,text FROM Message WHERE id>:cursor AND id<=:end
                    AND EXISTS(SELECT 1 FROM TursoMessageRebuild WHERE id=0 AND message=:cursor)",
                    named_params! {":cursor":cursor,":end":end},
                )?,
                statement(
                    "UPDATE TursoMessageRebuild SET message=:end WHERE id=0 AND message=:cursor",
                    named_params! {":cursor":cursor,":end":end},
                )?,
            ]);
            if let Err(err) = result {
                let text = err.to_string();
                if page > 1
                    && (text.contains("TIMEOUT")
                        || text.contains("timed out")
                        || text.contains("HTTP 504"))
                {
                    page = (page / 2).max(1);
                    continue; // Read the durable cursor before retrying.
                }
                return Err(err.into());
            }
        }
        // Nothing references Message. Dropping it also drops its indexes
        // and search triggers; the search index keeps the same row IDs.
        let (_, triggers) = super::FTS_SCHEMA
            .split_once("CREATE TRIGGER")
            .ok_or_else(|| anyhow::anyhow!("missing FTS triggers"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!(
            "DROP TABLE Message; ALTER TABLE MessageNext RENAME TO Message;
            DROP TABLE TursoMessageRebuild; CREATE TRIGGER{triggers}"
        ))?;
        tx.commit()?;
        Ok(())
    }

    /// Build sparse FTS in bounded transactions. Its cursor commits with
    /// each chunk, so restart or an ambiguous response cannot duplicate it.
    /// Startup has not made this tenant available to writers yet.
    fn migrate_turso_search(&self) -> Result<()> {
        let mut conn = self.conn();
        let sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name='MessageFTS'",
            [],
            |r| r.get(0),
        )?;
        if sql
            .split_whitespace()
            .collect::<String>()
            .contains("columnsize=0")
        {
            return Ok(());
        }
        conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS MessageFTSNext USING fts5(text,content=Message,content_rowid=id,columnsize=0);
            CREATE TABLE IF NOT EXISTS TursoSearchMigration(id INTEGER PRIMARY KEY CHECK(id=0), target INTEGER NOT NULL,time TEXT NOT NULL,message INTEGER NOT NULL);
            INSERT INTO TursoSearchMigration(id,target,time,message) VALUES(0,0,'',0) ON CONFLICT DO NOTHING;")?;
        let mut page = 200;
        loop {
            let (target, time, message): (i64, String, i64) = conn.query_row(
                "SELECT target,time,message FROM TursoSearchMigration WHERE id=0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let rows = conn
                .prepare(
                    "SELECT time,id FROM Message WHERE target=:target AND text IS NOT NULL
                AND time>=:time AND (time>:time OR id>:id) ORDER BY time,id LIMIT :limit",
                )?
                .query_map(
                    named_params! {":target":target,":time":time,":id":message,":limit":page},
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some((end_time, end_id)) = rows.last() {
                let result = conn.atomic_batch(vec![
                    statement("INSERT INTO MessageFTSNext(rowid,text) SELECT id,text FROM Message
                        WHERE target=:target AND text IS NOT NULL AND time>=:time
                        AND (time>:time OR id>:id) AND time<=:end_time
                        AND (time<:end_time OR id<=:end_id)
                        AND EXISTS(SELECT 1 FROM TursoSearchMigration WHERE id=0
                            AND target=:target AND time=:time AND message=:id)",
                        named_params!{":target":target,":time":time,":id":message,":end_time":end_time,":end_id":end_id})?,
                    statement("UPDATE TursoSearchMigration SET time=:end_time,message=:end_id WHERE id=0
                        AND target=:target AND time=:time AND message=:id", named_params!{":target":target,":time":time,":id":message,":end_time":end_time,":end_id":end_id})?,
                ]);
                if let Err(err) = result {
                    let text = err.to_string();
                    if page > 1
                        && (text.contains("TIMEOUT")
                            || text.contains("timed out")
                            || text.contains("HTTP 504"))
                    {
                        page = (page / 2).max(1);
                        continue; // Read the durable cursor before retrying.
                    }
                    return Err(err.into());
                }
                continue;
            }
            let next: Option<i64> = conn
                .query_row(
                    "SELECT id FROM MessageTarget t WHERE id>? AND EXISTS(
                SELECT 1 FROM Message WHERE target=t.id AND text IS NOT NULL)
                ORDER BY id LIMIT 1",
                    [target],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(next) = next {
                conn.execute(
                    "UPDATE TursoSearchMigration SET target=:next,time='',message=0 WHERE id=0
                        AND target=:target AND time=:time AND message=:id",
                    named_params! {":next":next,":target":target,":time":time,":id":message},
                )?;
                continue;
            }
            break;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; DROP TRIGGER MessageFTSUpdate;
            DROP TABLE MessageFTS; ALTER TABLE MessageFTSNext RENAME TO MessageFTS; DROP TABLE TursoSearchMigration;")?;
        // The replacement virtual table already exists; install only triggers.
        let (_, triggers) = super::FTS_SCHEMA
            .split_once("CREATE TRIGGER")
            .ok_or_else(|| anyhow::anyhow!("missing FTS triggers"))?;
        tx.execute_batch(&format!("CREATE TRIGGER{triggers}"))?;
        tx.commit()?;
        Ok(())
    }
}
