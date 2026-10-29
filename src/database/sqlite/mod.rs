//! The SQLite database backend (port of upstream's db_sqlite.go).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use super::connection::{Connection, Row};
use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection as SqliteConnection, TransactionBehavior};

use super::*;
use crate::xirc;

use super::sqlite_migrations::MIGRATIONS;

mod messages;
mod networks;
mod receipts;
mod turso;
mod users;
mod webpush;

/// Current schema version, including local migrations.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const SCHEMA: &str = include_str!("../sqlite_schema.sql");
const TENANT_SCHEMA: &str = include_str!("../turso/tenant_schema.sql");
const FTS_SCHEMA: &str = include_str!("../message_fts.sql");

/// Times are stored as text in the server-time layout, which sorts
/// lexicographically.
fn time_to_sql(t: Option<DateTime<Utc>>) -> Option<String> {
    t.map(xirc::format_server_time)
}

fn time_from_sql(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        None => Ok(None),
        Some(s) => xirc::parse_server_time(&s)
            .map(Some)
            .ok_or_else(|| anyhow!("cannot parse time {s:?}")),
    }
}

/// Reads a text column without requiring valid UTF-8, NULL being read as an
/// empty string. Go soju stores whatever bytes the upstream server sent, so
/// a database it wrote may hold e.g. Latin-1 messages or channel names.
/// Invalid sequences are replaced, so the result may not match the stored
/// value in later lookups by that column.
fn lossy_string(r: &Row, i: usize) -> rusqlite::Result<String> {
    match r.get_ref(i)? {
        rusqlite::types::ValueRef::Null => Ok(String::new()),
        v => Ok(String::from_utf8_lossy(v.as_bytes()?).into_owned()),
    }
}

fn null_string(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// Maps the zero ID, meaning none, to NULL.
fn null_id(id: i64) -> Option<i64> {
    if id == 0 { None } else { Some(id) }
}

/// Tenant databases share the server's ID namespace. Local SQLite retains
/// its usual sequential IDs; remote networks, channels and subscriptions
/// use random positive IDs to avoid collisions between tenant databases.
fn new_row_id(conn: &Connection) -> Option<i64> {
    conn.is_remote()
        .then(|| ((rand::random::<u64>() & i64::MAX as u64) as i64).max(1))
}

/// The SQLite database. All operations are serialized on a single
/// connection.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub(super) fn open_remote_controller(conn: Connection) -> Result<Database> {
        conn.initialize_remote_schema_version()?;
        let db = Database {
            conn: Mutex::new(conn),
        };
        {
            let mut conn = db.conn();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version = tx.schema_version()?;
            match version {
                0 => {
                    tx.execute_batch(include_str!("../turso/controller_schema.sql"))?;
                }
                1 => {}
                2 => {}
                _ => bail!("unsupported Turso controller schema version {version}"),
            }
            tx.execute_batch("CREATE INDEX IF NOT EXISTS DeviceCertificate_user_index ON DeviceCertificate(user);")?;
            if version < 2 {
                tx.set_schema_version(2)?;
            }
            tx.commit()?;
        }
        Ok(db)
    }

    pub fn open(source: &str) -> Result<Database> {
        let conn = SqliteConnection::open(source)?;
        Self::setup(conn)
    }

    pub fn open_in_memory() -> Result<Database> {
        Self::setup(SqliteConnection::open_in_memory()?)
    }

    fn setup(conn: SqliteConnection) -> Result<Database> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // journal_mode returns a row
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -512)?;
        let db = Database {
            conn: Mutex::new(Connection::Local(conn)),
        };
        db.upgrade()?;
        // Not part of upstream's schema, see
        // messages::record_message_id_high_water
        db.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS MessageIdHighWater (
                id INTEGER PRIMARY KEY CHECK (id = 0),
                last_id INTEGER NOT NULL
            );",
        )?;
        Ok(db)
    }

    pub(super) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn upgrade(&self) -> Result<()> {
        let mut conn = self.conn();
        let version = conn.schema_version()?;
        if version == SCHEMA_VERSION {
            return Ok(());
        } else if version > SCHEMA_VERSION {
            bail!("soju (version {SCHEMA_VERSION}) older than schema (version {version})");
        }
        // Some migrations rebuild tables referenced by foreign keys. As
        // recommended by SQLite, disable foreign key enforcement while
        // altering the schema (this can't be changed inside a transaction).
        conn.pragma_update(None, "foreign_keys", false)?;
        let res = Self::run_migrations(&mut conn, version);
        conn.pragma_update(None, "foreign_keys", true)?;
        res
    }

    fn run_migrations(conn: &mut Connection, version: i64) -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another process may have initialized the database while BEGIN waited.
        let version = if tx.is_remote() {
            tx.schema_version()?
        } else {
            version
        };
        if version > SCHEMA_VERSION {
            bail!("unsupported database schema version {version}");
        }
        if version == SCHEMA_VERSION {
            tx.commit()?;
            return Ok(());
        }
        if version == 0 {
            tx.execute_batch(if tx.is_remote() {
                TENANT_SCHEMA
            } else {
                SCHEMA
            })
            .map_err(|e| anyhow!("failed to initialize schema: {e}"))?;
            tx.execute_batch(FTS_SCHEMA)?;
        } else {
            for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
                // Turso's hosted SQL allowlist rejects ANALYZE. The partial
                // index itself is supported; local SQLite still analyzes it.
                // The Message rebuild that follows replaces the timestamp
                // indexes: don't write an index entry for every message first.
                let migration = if tx.is_remote() {
                    migration.replace("ANALYZE MessageTextIndex;", "").replace(
                        "CREATE INDEX IF NOT EXISTS MessageEventIndex ON Message(target, time) WHERE text IS NULL;",
                        "",
                    )
                } else {
                    (*migration).to_owned()
                };
                tx.execute_batch(&migration)
                    .map_err(|e| anyhow!("failed to execute migration #{i}: {e}"))?;
            }
            if !tx.is_remote() && version < 32 {
                tx.execute_batch("DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; DROP TRIGGER MessageFTSUpdate; DROP TABLE MessageFTS;")?;
                tx.execute_batch(FTS_SCHEMA)?;
                tx.execute_batch("INSERT INTO MessageFTS(rowid, text) SELECT id, text FROM Message WHERE text IS NOT NULL;")?;
            }
        }
        tx.set_schema_version(SCHEMA_VERSION)
            .map_err(|e| anyhow!("failed to bump schema version: {e}"))?;
        tx.commit()?;
        Ok(())
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let conn = self.conn();
        let users = if conn.is_remote() {
            "0"
        } else {
            "(SELECT COUNT(*) FROM User)"
        };
        Ok(conn.query_row(
            &format!(
                "SELECT
                {users} AS users,
                (SELECT COUNT(*) FROM Network) AS networks,
                (SELECT COUNT(*) FROM Channel) AS channels"
            ),
            [],
            |r| {
                Ok(DatabaseStats {
                    users: r.get(0)?,
                    networks: r.get(1)?,
                    channels: r.get(2)?,
                })
            },
        )?)
    }

    /// The fraction of the database file made of free pages: space freed by
    /// deleted rows, reused for new rows, or reclaimed by vacuum.
    pub fn free_page_ratio(&self) -> Result<f64> {
        let conn = self.conn();
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        Ok(if pages > 0 {
            free as f64 / pages as f64
        } else {
            0.0
        })
    }

    /// Merges the search index after deleting messages. Without this, the
    /// index keeps growing with segments full of deleted entries.
    pub fn optimize(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("INSERT INTO MessageFTS(MessageFTS) VALUES ('optimize');")?;
        // Keep the WAL from staying as large as what was rewritten
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Frees the page cache. After deleting expired messages, optimizing or
    /// vacuuming, it holds pages from scanning or rewriting the whole
    /// database, which day-to-day queries won't reuse.
    pub fn shrink_memory(&self) -> Result<()> {
        self.conn().execute_batch("PRAGMA shrink_memory;")?;
        Ok(())
    }

    /// Reclaims the free pages, by rewriting the whole database file.
    pub fn vacuum(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("VACUUM;")?;
        // VACUUM goes through the WAL, which keeps its size on disk until
        // truncated by a checkpoint
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
