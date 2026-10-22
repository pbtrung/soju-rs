//! The SQLite database backend (port of upstream's db_sqlite.go).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, TransactionBehavior};

use super::*;
use crate::xirc;

use super::sqlite_migrations::MIGRATIONS;

mod messages;
mod networks;
mod receipts;
mod users;
mod webpush;

/// Current schema version, the same as upstream soju.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const SCHEMA: &str = include_str!("../sqlite_schema.sql");

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
fn lossy_string(r: &rusqlite::Row, i: usize) -> rusqlite::Result<String> {
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

/// The SQLite database. All operations are serialized on a single
/// connection.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(source: &str) -> Result<Database> {
        let conn = Connection::open(source)?;
        Self::setup(conn)
    }

    pub fn open_in_memory() -> Result<Database> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(conn: Connection) -> Result<Database> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // journal_mode returns a row
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -512)?;
        let db = Database {
            conn: Mutex::new(conn),
        };
        db.upgrade()?;
        Ok(db)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn upgrade(&self) -> Result<()> {
        let mut conn = self.conn();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
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
        if version == 0 {
            tx.execute_batch(SCHEMA)
                .map_err(|e| anyhow!("failed to initialize schema: {e}"))?;
        } else {
            for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
                tx.execute_batch(migration)
                    .map_err(|e| anyhow!("failed to execute migration #{i}: {e}"))?;
            }
        }
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|e| anyhow!("failed to bump schema version: {e}"))?;
        tx.commit()?;
        Ok(())
    }

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
