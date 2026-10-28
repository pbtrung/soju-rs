//! Stable message IDs, atomic HTTP writes, and an in-memory history overlay.

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, ensure};
use chrono::Utc;
use rusqlite::named_params;

use super::connection::{Connection, statement};
use super::{MessageOptions, MessageTargetLast, sqlite, strip_ansi};
use crate::irc::Message;
use crate::xirc;

pub(crate) struct MessageRow {
    pub id: i64,
    pub time: String,
    pub message: Message,
}

#[derive(Clone)]
pub(crate) struct MessageInsert {
    pub id: i64,
    pub network: i64,
    pub target: String,
    raw: String,
    time: String,
    sender: String,
    text: Option<String>,
}

impl MessageInsert {
    pub fn new(id: i64, network: i64, target: &str, msg: &Message) -> Result<Self> {
        let time = match msg.tags.get("time") {
            Some(t) => {
                xirc::parse_server_time(t).ok_or_else(|| anyhow::anyhow!("invalid message time"))?
            }
            None => Utc::now(),
        };
        Ok(Self {
            id,
            network,
            target: target.to_owned(),
            raw: msg.to_string(),
            time: xirc::format_server_time(time),
            sender: msg.prefix_name().to_owned(),
            text: match msg.command.as_str() {
                "PRIVMSG" | "NOTICE" if msg.params.len() > 1 => Some(strip_ansi(&msg.params[1])),
                _ => None,
            },
        })
    }

    pub fn bytes(&self) -> usize {
        self.raw.len() + self.target.len() + self.text.as_ref().map_or(0, String::len)
    }
}

pub(crate) struct MessageWriter {
    conn: Mutex<Connection>,
    max_lines: AtomicUsize,
}

impl MessageWriter {
    pub(super) fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
            max_lines: AtomicUsize::new(usize::MAX),
        }
    }

    /// Persist reservations before exposing IDs to clients. A process crash
    /// may leave gaps, but cannot reuse IDs held by delivery receipts.
    pub fn reserve(&self, count: usize) -> Result<RangeInclusive<i64>> {
        let count = i64::try_from(count)?;
        ensure!(count > 0, "empty message ID reservation");
        let conn = self.conn.lock().unwrap();
        let end: i64 = conn.query_row(
            "INSERT INTO MessageIdHighWater(id, last_id)
             SELECT 0, max(coalesce((SELECT max(id) FROM Message), 0),
                           coalesce((SELECT last_id FROM MessageIdHighWater WHERE id=0), 0)) + :count
             WHERE max(coalesce((SELECT max(id) FROM Message), 0),
                       coalesce((SELECT last_id FROM MessageIdHighWater WHERE id=0), 0)) <= :maximum
             ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id
             RETURNING last_id",
            named_params! {":count":count, ":maximum":i64::MAX-count},
            |r| r.get(0),
        )?;
        Ok((end - count + 1)..=end)
    }

    pub fn write(&self, messages: &[MessageInsert]) -> Result<()> {
        for chunk in messages.chunks(self.max_lines.load(Ordering::Relaxed).max(1)) {
            self.write_chunk(chunk)?;
        }
        Ok(())
    }

    fn write_chunk(&self, messages: &[MessageInsert]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut statements = Vec::with_capacity(messages.len() * 2);
        let mut targets = HashSet::new();
        for m in messages {
            if targets.insert((m.network, &m.target)) {
                statements.push(statement(
                    "INSERT INTO MessageTarget(network, target)
                     SELECT :network, :target WHERE EXISTS(SELECT 1 FROM Network WHERE id=:network)
                     ON CONFLICT(network, target) DO NOTHING",
                    named_params! {":network":m.network, ":target":m.target},
                )?);
            }
            // Retrying a lost COMMIT response is safe with reserved IDs.
            // An ID collision with different contents fails the NOT NULL
            // constraint instead of silently replacing someone else's row.
            statements.push(statement(
                "INSERT INTO Message(id, target, raw, time, sender, text)
                 SELECT :id, id, CAST(:raw AS TEXT), :time, :sender, CAST(:text AS TEXT)
                 FROM MessageTarget WHERE network=:network AND target=:target
                 ON CONFLICT(id) DO UPDATE SET raw = CASE
                   WHEN Message.target=excluded.target AND Message.raw=excluded.raw
                    AND Message.time=excluded.time AND Message.sender=excluded.sender
                    AND Message.text IS excluded.text THEN Message.raw ELSE NULL END",
                named_params! {":id":m.id, ":network":m.network, ":target":m.target,
                ":raw":m.raw.as_bytes(), ":time":m.time, ":sender":m.sender,
                ":text":m.text.as_ref().map(|text| text.as_bytes())},
            )?);
        }
        // atomic_batch checks the exact serialized pipeline size before
        // sending. Split on body limits and processing timeouts; stable IDs
        // make this safe even if the failed request actually committed.
        let result = self.conn.lock().unwrap().atomic_batch(statements);
        match result {
            Ok(()) => Ok(()),
            Err(err) => {
                let text = err.to_string();
                let size = text.contains("HTTP body budget") || text.contains("HTTP 413");
                let timeout = text.contains("TRANSACTION_TIMEOUT")
                    || text.contains("QUERY_TIMEOUT")
                    || text.contains("timed out")
                    || text.contains("HTTP 504");
                if messages.len() == 1 || !(size || timeout) {
                    return Err(err.into());
                }
                let middle = messages.len() / 2;
                if timeout {
                    self.max_lines.fetch_min(middle.max(1), Ordering::Relaxed);
                }
                self.write_chunk(&messages[..middle])?;
                self.write_chunk(&messages[middle..])
            }
        }
    }
}

/// Use the same SQLite queries and FTS tokenizer as persistent history, so
/// pending messages support history, search, and insertion-order receipts.
pub(crate) struct MessageBuffer(sqlite::Database);

impl MessageBuffer {
    pub fn new() -> Result<Self> {
        let db = sqlite::Database::open_in_memory()?;
        db.conn()
            .execute_batch("INSERT INTO User(id, username, created_at) VALUES(1, 'pending', '')")?;
        Ok(Self(db))
    }

    pub fn insert(&self, m: &MessageInsert) -> Result<()> {
        let conn = self.0.conn();
        conn.execute("INSERT INTO Network(id, user, addr) VALUES(:network, 1, :addr) ON CONFLICT(id) DO NOTHING",
            named_params!{":network":m.network, ":addr":m.network.to_string()})?;
        conn.execute("INSERT INTO MessageTarget(network, target) VALUES(:network, :target) ON CONFLICT DO NOTHING",
            named_params!{":network":m.network, ":target":m.target})?;
        conn.execute("INSERT INTO Message(id, target, raw, time, sender, text)
            VALUES(:id, (SELECT id FROM MessageTarget WHERE network=:network AND target=:target), :raw, :time, :sender, :text)",
            named_params! {":id":m.id, ":network":m.network, ":target":m.target,
                ":raw":m.raw, ":time":m.time, ":sender":m.sender, ":text":m.text})?;
        Ok(())
    }

    pub fn remove(&self, messages: &[MessageInsert]) -> Result<()> {
        let mut conn = self.0.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for m in messages {
            tx.execute("DELETE FROM Message WHERE id=?", [m.id])?;
        }
        // Keep metadata bounded too; retain only targets/networks with pending messages.
        tx.execute_batch(
            "DELETE FROM MessageTarget WHERE id NOT IN (SELECT target FROM Message);
            DELETE FROM Network WHERE id NOT IN (SELECT network FROM MessageTarget);",
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn last_id(&self, network: i64, target: &str) -> Result<i64> {
        self.0.get_message_last_id(network, target)
    }

    pub fn list(
        &self,
        network: i64,
        target: &str,
        opts: &MessageOptions,
    ) -> Result<Vec<MessageRow>> {
        self.0.list_message_rows(network, target, opts)
    }

    pub fn targets(&self, network: i64, opts: &MessageOptions) -> Result<Vec<MessageTargetLast>> {
        self.0.list_message_last_per_target(network, opts)
    }
}
