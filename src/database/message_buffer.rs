//! Stable message IDs, atomic HTTP writes, and an in-memory history overlay.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, ensure};
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, ToSql, named_params};

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
    conn: Mutex<WriterConnection>,
    max_lines: AtomicUsize,
    /// Consecutive successful chunks since max_lines was last lowered.
    successes: AtomicUsize,
}

/// Successful chunks after which a lowered line limit doubles again.
const MAX_LINES_RECOVERY: usize = 8;

struct TargetId {
    id: i64,
    create: bool,
}

struct WriterConnection {
    conn: Connection,
    targets: HashMap<(i64, String), TargetId>,
}

impl WriterConnection {
    fn statements(&mut self, messages: &[MessageInsert]) -> Result<Vec<serde_json::Value>> {
        if self.targets.len() > 4096 {
            self.targets.clear();
        }
        let mut statements = Vec::new();
        let mut targets = HashMap::new();
        for m in messages {
            let key = (m.network, m.target.clone());
            if targets.contains_key(&key) {
                continue;
            }
            if !self.targets.contains_key(&key) {
                let saved: Option<Option<i64>> = self.conn.query_row(
                    "SELECT t.id FROM Network n LEFT JOIN MessageTarget t ON t.network=n.id AND t.target=:target WHERE n.id=:network",
                    named_params!{":network":m.network,":target":m.target}, |r|r.get(0),
                ).optional()?;
                let Some(saved) = saved else {
                    targets.insert(key, None);
                    continue;
                };
                self.targets.insert(
                    key.clone(),
                    TargetId {
                        id: saved.unwrap_or_else(|| {
                            ((rand::random::<u64>() & (i64::MAX as u64 >> 1)) as i64).max(1)
                        }),
                        create: saved.is_none(),
                    },
                );
            }
            let target = &self.targets[&key];
            if target.create {
                statements.push(statement(
                    "INSERT INTO MessageTarget(id,network,target) SELECT :id,:network,:target
                     WHERE EXISTS(SELECT 1 FROM Network WHERE id=:network)
                     ON CONFLICT(network,target) DO NOTHING",
                    named_params! {":id":target.id,":network":m.network,":target":m.target},
                )?);
            }
            targets.insert(key, Some(target.id));
        }
        let rows: Vec<_> = messages
            .iter()
            .filter_map(|m| targets[&(m.network, m.target.clone())].map(|target| (m, target)))
            .collect();
        // Stay below even the older SQLite 999-parameter limit. These SQL
        // statements still share one atomic HTTP batch/COMMIT.
        for chunk in rows.chunks(100) {
            let mut sql =
                String::from("INSERT INTO Message(id,target,raw,time,sender,text) VALUES ");
            let mut values = Vec::with_capacity(chunk.len() * 4);
            for (i, (m, target)) in chunk.iter().enumerate() {
                if i != 0 {
                    sql.push(',');
                }
                sql.push_str(&format!(
                    "({}, {}, CAST(? AS TEXT), ?, ?, CAST(? AS TEXT))",
                    m.id, target
                ));
                values.extend([
                    Value::Blob(m.raw.as_bytes().to_vec()),
                    Value::Text(m.time.clone()),
                    Value::Text(m.sender.clone()),
                    m.text
                        .as_ref()
                        .map_or(Value::Null, |t| Value::Blob(t.as_bytes().to_vec())),
                ]);
            }
            // An identical retry performs no UPDATE or FTS writes. A different
            // text row with the same target and ID deliberately violates NOT
            // NULL and aborts. Retries reuse the stored time, hence the key.
            sql.push_str(" ON CONFLICT DO UPDATE SET raw=NULL WHERE NOT (
                Message.target=excluded.target AND Message.raw=excluded.raw AND Message.time=excluded.time
                AND Message.sender=excluded.sender AND Message.text IS excluded.text)");
            let params: Vec<&dyn ToSql> = values.iter().map(|v| v as &dyn ToSql).collect();
            statements.push(statement(&sql, params.as_slice())?);
        }
        Ok(statements)
    }

    fn write(&mut self, messages: &[MessageInsert]) -> rusqlite::Result<()> {
        let statements = self
            .statements(messages)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
        if statements.is_empty() {
            return Ok(());
        }
        self.conn.atomic_batch(statements)?;
        for m in messages {
            if let Some(target) = self.targets.get_mut(&(m.network, m.target.clone())) {
                target.create = false;
            }
        }
        Ok(())
    }
}

impl MessageWriter {
    pub(super) fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(WriterConnection {
                conn,
                targets: HashMap::new(),
            }),
            max_lines: AtomicUsize::new(usize::MAX),
            successes: AtomicUsize::new(0),
        }
    }

    /// Persist reservations before exposing IDs to clients. A process crash
    /// may leave gaps, but cannot reuse IDs held by delivery receipts.
    pub fn reserve(&self, count: usize) -> Result<RangeInclusive<i64>> {
        let count = i64::try_from(count)?;
        ensure!(count > 0, "empty message ID reservation");
        let state = self.conn.lock().unwrap();
        let conn = &state.conn;
        // Every tenant ID is reserved here (tenant opening records older
        // IDs first): the clustered Message table can't seek max(id).
        let end: i64 = conn.query_row(
            "INSERT INTO MessageIdHighWater(id, last_id)
             SELECT 0, coalesce((SELECT last_id FROM MessageIdHighWater WHERE id=0), 0) + :count
             WHERE coalesce((SELECT last_id FROM MessageIdHighWater WHERE id=0), 0) <= :maximum
             ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id
             RETURNING last_id",
            named_params! {":count":count, ":maximum":i64::MAX-count},
            |r| r.get(0),
        )?;
        Ok((end - count + 1)..=end)
    }

    /// Writes messages, returning how many were rejected by a constraint
    /// and dropped. Other errors keep the batch for a retry.
    pub fn write(&self, messages: &[MessageInsert]) -> Result<usize> {
        let mut rejected = 0;
        for chunk in messages.chunks(self.max_lines.load(Ordering::Relaxed).max(1)) {
            rejected += self.write_chunk(chunk)?;
        }
        Ok(rejected)
    }

    fn write_chunk(&self, messages: &[MessageInsert]) -> Result<usize> {
        if messages.is_empty() {
            return Ok(0);
        }
        // atomic_batch checks the exact serialized pipeline size before
        // sending. Split on body limits and processing timeouts; stable IDs
        // make this safe even if the failed request actually committed.
        let result = {
            let mut state = self.conn.lock().unwrap();
            let result = state.write(messages);
            if result.as_ref().err().is_some_and(|e| {
                let text = e.to_string();
                text.contains("SQLITE_CONSTRAINT_FOREIGNKEY")
                    || text.contains("FOREIGN KEY constraint failed")
            }) {
                // Deletion can invalidate cached target IDs between batches.
                // The failed batch rolled back. Resolve again, omit removed
                // networks, and replay only these idempotent message rows.
                state.targets.clear();
                state.write(messages)
            } else {
                result
            }
        };
        match result {
            Ok(()) => {
                // Grow back after a transient timeout lowered the limit
                let max = self.max_lines.load(Ordering::Relaxed);
                if max != usize::MAX
                    && self.successes.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_LINES_RECOVERY
                {
                    self.successes.store(0, Ordering::Relaxed);
                    let _ = self.max_lines.compare_exchange(
                        max,
                        max.saturating_mul(2),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                }
                Ok(0)
            }
            Err(err) => {
                let text = err.to_string();
                let size = text.contains("HTTP body budget") || text.contains("HTTP 413");
                let timeout = text.contains("TRANSACTION_TIMEOUT")
                    || text.contains("QUERY_TIMEOUT")
                    || text.contains("timed out")
                    || text.contains("HTTP 504");
                // A row the database rejects, e.g. a conflicting reserved ID,
                // would otherwise block every later message of this user.
                // Isolate it; the batch rolled back, the others are retried.
                let rejected =
                    text.contains("SQLITE_CONSTRAINT") || text.contains("constraint failed");
                if messages.len() == 1 && rejected {
                    return Ok(1);
                }
                if messages.len() == 1 || !(size || timeout || rejected) {
                    return Err(err.into());
                }
                let middle = messages.len() / 2;
                if timeout {
                    self.max_lines.fetch_min(middle.max(1), Ordering::Relaxed);
                    self.successes.store(0, Ordering::Relaxed);
                }
                Ok(self.write_chunk(&messages[..middle])?
                    + self.write_chunk(&messages[middle..])?)
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
