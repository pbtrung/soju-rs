//! The SQLite database backend (port of upstream's `db_sqlite.go`).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use jiff::Timestamp;
use rusqlite::{Connection, Row, TransactionBehavior};

use super::DatabaseStats;
use crate::xirc;

use super::sqlite_migrations::MIGRATIONS;

mod messages;
mod networks;
mod receipts;
mod users;
mod webpush;

/// Current schema version, including local migrations.
#[expect(
    clippy::cast_possible_wrap,
    reason = "The compile-time migration list has 35 entries, far below i64::MAX."
)]
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const SCHEMA: &str = include_str!("../sqlite_schema.sql");
const FTS_SCHEMA: &str = include_str!("../message_fts.sql");

/// Times are stored as text in the server-time layout, which sorts
/// lexicographically.
fn time_to_sql(t: Option<Timestamp>) -> Option<String> {
    t.map(xirc::format_server_time)
}

fn time_from_sql(s: Option<String>) -> Result<Option<Timestamp>> {
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

/// Concurrent initializers can get `SQLITE_BUSY` from `journal_mode` without
/// invoking SQLite's busy handler. Retry that operation within one five-second
/// budget; migration serialization remains BEGIN IMMEDIATE on each connection.
fn initialize_journal(connection: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        connection.busy_timeout(deadline.saturating_duration_since(std::time::Instant::now()))?;
        let result = connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        });
        match result {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(std::time::Instant::now())),
                );
            }
            Err(error) => return Err(anyhow!(error).context("setting SQLite WAL journal mode")),
        }
    }
}

/// The SQLite database. All operations are serialized on a single
/// connection.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn open_config(cfg: &crate::config::Server) -> Result<Self> {
        Self::open(&cfg.db.source)
    }

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn open(source: &str) -> Result<Database> {
        let conn = Connection::open(source)?;
        Self::setup(conn)
    }

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn open_in_memory() -> Result<Database> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(conn: Connection) -> Result<Database> {
        receipts::register_case_functions(&conn)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let journal = initialize_journal(&conn);
        // Initialization uses the remaining retry budget; ordinary operations
        // retain the existing five-second SQLite busy timeout.
        conn.busy_timeout(Duration::from_secs(5))?;
        journal?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -512)?;
        let db = Database {
            conn: Mutex::new(conn),
        };
        db.upgrade()?;
        Ok(db)
    }

    // rusqlite transactions roll back on drop, including unwinding. Recovery
    // keeps SQLite authoritative rather than recovering partially changed actor state.
    pub(super) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn upgrade(&self) -> Result<()> {
        let mut conn = self.conn();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        } else if version > SCHEMA_VERSION {
            bail!("soju (version {SCHEMA_VERSION}) older than schema (version {version})");
        }
        // Some migrations rebuild tables referenced by foreign keys. As
        // recommended by SQLite, disable foreign key enforcement while
        // altering the schema (this can't be changed inside a transaction).
        conn.pragma_update(None, "foreign_keys", false)?;
        let res = Self::run_migrations(&mut conn);
        conn.pragma_update(None, "foreign_keys", true)?;
        res
    }

    fn run_migrations(conn: &mut Connection) -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // BEGIN may have waited for another process to finish the upgrade.
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            bail!("unsupported database schema version {version}");
        }
        if version == SCHEMA_VERSION {
            tx.commit()?;
            return Ok(());
        }
        if version == 0 {
            tx.execute_batch(SCHEMA)
                .map_err(|e| anyhow!("failed to initialize schema: {e}"))?;
            tx.execute_batch(FTS_SCHEMA)?;
        } else {
            for (i, migration) in MIGRATIONS
                .iter()
                .enumerate()
                .skip(usize::try_from(version)?)
            {
                tx.execute_batch(migration)
                    .map_err(|e| anyhow!("failed to execute migration #{i}: {e}"))?;
            }
            if version < 32 {
                tx.execute_batch(
                    "DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; \
                    DROP TRIGGER MessageFTSUpdate; DROP TABLE MessageFTS;",
                )?;
                tx.execute_batch(FTS_SCHEMA)?;
                tx.execute_batch(
                    "INSERT INTO MessageFTS(rowid, text) SELECT id, text FROM Message \
                    WHERE text IS NOT NULL;",
                )?;
            }
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| anyhow!("failed to bump schema version: {e}"))?;
        tx.commit()?;
        Ok(())
    }

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn stats(&self) -> Result<DatabaseStats> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM User) AS users,
                (SELECT COUNT(*) FROM Network) AS networks,
                (SELECT COUNT(*) FROM Channel) AS channels",
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

    /// Deletes expired messages in chunks, yielding database access between chunks.
    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn expire_messages(
        &self,
        before: Timestamp,
        chunk: usize,
        pause: Duration,
        stopped: &dyn Fn() -> bool,
    ) -> Result<usize> {
        anyhow::ensure!(chunk > 0, "message expiry chunk must be positive");
        let mut count = 0;
        while !stopped() {
            let deleted = self.delete_messages_before(before, chunk)?;
            count += deleted;
            if deleted < chunk {
                break;
            }
            std::thread::sleep(pause);
        }
        Ok(count)
    }

    /// The fraction of the database file made of free pages: space freed by
    /// deleted rows, reused for new rows, or reclaimed by vacuum.
    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn free_page_ratio(&self) -> Result<f64> {
        let conn = self.conn();
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        Ok(if pages > 0 {
            f64::from(u32::try_from(free)?) / f64::from(u32::try_from(pages)?)
        } else {
            0.0
        })
    }

    /// Merges the search index after deleting messages. Without this, the
    /// index keeps growing with segments full of deleted entries.
    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
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
    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn shrink_memory(&self) -> Result<()> {
        self.conn().execute_batch("PRAGMA shrink_memory;")?;
        Ok(())
    }

    /// Reclaims the free pages, by rewriting the whole database file.
    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
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
