//! Message targets, logged messages and their full-text search.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, named_params};

use super::{Database, lossy_string, time_from_sql, time_to_sql};
use crate::database::{MessageOptions, MessageTarget, MessageTargetLast, strip_ansi};
use crate::irc::Message;
use crate::xirc;

impl Database {
    /// Returns the ID of the last text message stored for a target, or 0.
    ///
    /// Delivery receipts are cursors in insertion order (see after_id in
    /// list_messages): this is the last message stored, not the one with the
    /// latest time, which upstream servers may send out of order. Backlog
    /// replays only text messages after the cursor, so skipping later events
    /// selects the same messages; Turso tenants only index text message IDs.
    pub fn get_message_last_id(&self, network_id: i64, name: &str) -> Result<i64> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT id FROM Message
                WHERE target = (SELECT id FROM MessageTarget WHERE network = :network AND target = :target)
                AND text IS NOT NULL ORDER BY id DESC LIMIT 1",
                named_params! {":network": network_id, ":target": name},
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Returns the message target. If it doesn't exist, a target with a zero
    /// ID is returned.
    pub fn get_message_target(&self, network_id: i64, target: &str) -> Result<MessageTarget> {
        let conn = self.conn();
        let mut mt = MessageTarget {
            target: target.to_owned(),
            ..Default::default()
        };
        if let Some((id, pinned, muted, blocked)) = conn
            .query_row(
                "SELECT id, pinned, muted, blocked FROM MessageTarget WHERE network = :network AND target = :target",
                named_params! {":network": network_id, ":target": target},
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
        {
            mt.id = id;
            mt.pinned = pinned;
            mt.muted = muted;
            mt.blocked = blocked;
        }
        Ok(mt)
    }

    pub fn list_message_targets(&self, network_id: i64) -> Result<Vec<MessageTarget>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, target, pinned, muted, blocked
            FROM MessageTarget
            WHERE network = ? AND (pinned OR muted OR blocked)",
        )?;
        let rows = stmt.query_map([network_id], |r| {
            Ok(MessageTarget {
                id: r.get(0)?,
                target: lossy_string(r, 1)?,
                pinned: r.get(2)?,
                muted: r.get(3)?,
                blocked: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_message_target(&self, network_id: i64, mt: &mut MessageTarget) -> Result<()> {
        let conn = self.conn();
        if mt.id != 0 {
            conn.execute(
                "UPDATE MessageTarget SET pinned = :pinned, muted = :muted, blocked = :blocked WHERE id = :id AND (pinned IS NOT :pinned OR muted IS NOT :muted OR blocked IS NOT :blocked)",
                named_params! {
                    ":pinned": mt.pinned,
                    ":muted": mt.muted,
                    ":blocked": mt.blocked,
                    ":id": mt.id,
                },
            )?;
        } else {
            conn.execute(
                "INSERT INTO
                MessageTarget(network, target, pinned, muted, blocked)
                VALUES (:network, :target, :pinned, :muted, :blocked)",
                named_params! {
                    ":network": network_id,
                    ":target": mt.target,
                    ":pinned": mt.pinned,
                    ":muted": mt.muted,
                    ":blocked": mt.blocked,
                },
            )?;
            mt.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn store_messages(
        &self,
        network_id: i64,
        name: &str,
        msgs: &[Message],
    ) -> Result<Vec<i64>> {
        if msgs.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Messages are stored one by one as they arrive: keep the statements
        // compiled
        tx.prepare_cached(
            "INSERT INTO MessageTarget(network, target)
            VALUES (:network, :target)
            ON CONFLICT DO NOTHING",
        )?
        .execute(named_params! {":network": network_id, ":target": name})?;
        let mut ids = Vec::with_capacity(msgs.len());
        if tx.is_remote() {
            // Turso tenants can't seek max(id) of their clustered table:
            // reserve IDs like the background writer does.
            let count = msgs.len() as i64;
            let end: i64 = tx.query_row(
                "INSERT INTO MessageIdHighWater(id, last_id)
                VALUES (0, coalesce((SELECT last_id FROM MessageIdHighWater WHERE id = 0), 0) + :count)
                ON CONFLICT(id) DO UPDATE SET last_id = excluded.last_id
                RETURNING last_id",
                named_params! {":count": count},
                |r| r.get(0),
            )?;
            let mut stmt = tx.prepare_cached(
                "INSERT INTO Message(id, target, raw, time, sender, text)
                SELECT :id, id, :raw, :time, :sender, :text
                FROM MessageTarget WHERE network = :network AND target = :target",
            )?;
            for (msg, id) in msgs.iter().zip(end - count + 1..) {
                let (time, text) = message_time_text(msg)?;
                stmt.execute(named_params! {
                    ":id": id,
                    ":network": network_id,
                    ":target": name,
                    ":raw": msg.to_string(),
                    ":time": time,
                    ":sender": msg.prefix_name(),
                    ":text": text,
                })?;
                ids.push(id);
            }
        } else {
            // Message IDs are delivery receipt cursors: never reuse the IDs
            // of deleted messages, see record_message_id_high_water
            let mut stmt = tx.prepare_cached(
                "INSERT INTO Message(id, target, raw, time, sender, text)
                SELECT max(
                        coalesce((SELECT max(id) FROM Message), 0),
                        coalesce((SELECT last_id FROM MessageIdHighWater), 0)
                    ) + 1,
                    id, :raw, :time, :sender, :text
                FROM MessageTarget as t
                WHERE network = :network AND target = :target",
            )?;
            for msg in msgs {
                let (time, text) = message_time_text(msg)?;
                stmt.execute(named_params! {
                    ":network": network_id,
                    ":target": name,
                    ":raw": msg.to_string(),
                    ":time": time,
                    ":sender": msg.prefix_name(),
                    ":text": text,
                })?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn list_message_last_per_target(
        &self,
        network_id: i64,
        options: &MessageOptions,
    ) -> Result<Vec<MessageTargetLast>> {
        let remote = self.conn().is_remote();
        let inner = if options.events && remote {
            // Turso tenants cluster messages by target and time
            "SELECT time FROM Message WHERE target=MessageTarget.id ORDER BY time DESC LIMIT 1"
        } else if options.events {
            "SELECT max(latest) FROM (
                SELECT max(time) latest FROM Message WHERE target=MessageTarget.id AND text IS NULL
                UNION ALL
                SELECT max(time) latest FROM Message WHERE target=MessageTarget.id AND text IS NOT NULL
            )"
        } else {
            "SELECT time FROM Message WHERE target=MessageTarget.id AND text IS NOT NULL ORDER BY time DESC LIMIT 1"
        };

        let mut query = format!(
            "SELECT target, ({inner}) latest
            FROM MessageTarget
            WHERE network = :network "
        );
        if options.after_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND latest > :after ");
        }
        if options.before_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND latest < :before ");
        }
        if options.take_last {
            query.push_str("ORDER BY latest DESC ");
        } else {
            query.push_str("ORDER BY latest ASC ");
        }
        query.push_str("LIMIT :limit");

        let conn = self.conn();
        let mut stmt = conn.prepare(&query)?;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> = vec![(":network", &network_id)];
        let after = time_to_sql(options.after_time);
        let before = time_to_sql(options.before_time);
        let limit = options.limit as i64;
        if options.after_time.is_some() {
            binds.push((":after", &after));
        }
        if options.before_time.is_some() {
            binds.push((":before", &before));
        }
        binds.push((":limit", &limit));
        let rows = stmt.query_map(binds.as_slice(), |r| {
            Ok((lossy_string(r, 0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut l = Vec::new();
        for row in rows {
            let (name, ts) = row?;
            l.push(MessageTargetLast {
                name,
                latest_message: time_from_sql(ts)?,
            });
        }
        if options.take_last {
            // We ordered by DESC to limit to the last lines. Reverse the list
            // to order by ASC these last lines.
            l.reverse();
        }
        Ok(l)
    }

    pub fn list_messages(
        &self,
        network_id: i64,
        name: &str,
        options: &MessageOptions,
    ) -> Result<Vec<Message>> {
        Ok(self
            .list_message_rows(network_id, name, options)?
            .into_iter()
            .map(|r| r.message)
            .collect())
    }

    pub(crate) fn list_message_rows(
        &self,
        network_id: i64,
        name: &str,
        options: &MessageOptions,
    ) -> Result<Vec<crate::database::MessageRow>> {
        let after = time_to_sql(options.after_time);
        let before = time_to_sql(options.before_time);
        let text = quote_fts_query(&options.text);
        if !options.text.is_empty() && text.is_empty() {
            // Nothing to search for, e.g. only punctuation: MATCH '' would
            // be a syntax error
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let remote = conn.is_remote();
        let target = "(SELECT id FROM MessageTarget WHERE network = :network AND target = :target)";
        let mut query = String::from("SELECT m.id AS id, m.time AS time, m.raw AS raw FROM ");
        if !options.text.is_empty() {
            // Seek each search result by ID, rather than scanning the
            // target's history for them
            query.push_str(
                "(SELECT rowid AS r FROM MessageFTS WHERE MessageFTS MATCH :text) AS f
                CROSS JOIN Message AS m ON m.id = f.r ",
            );
        } else {
            query.push_str("Message AS m ");
        }
        query.push_str(&format!("WHERE m.target = {target} "));
        let limit = options.limit as i64;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> =
            vec![(":network", &network_id), (":target", &name)];
        if !options.text.is_empty() {
            binds.push((":text", &text));
        }
        let text_only = !options.events || !options.text.is_empty();
        if options.after_id > 0 {
            if text_only {
                // Delivery cursors: seek the text message ID index
                query.push_str(&format!(
                    "AND (m.time, m.id) IN (SELECT time, id FROM Message
                    WHERE target = {target} AND text IS NOT NULL AND id > :afterID) "
                ));
            } else {
                query.push_str("AND m.id > :afterID ");
            }
            binds.push((":afterID", &options.after_id));
        }
        if options.after_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND m.time > :after ");
            binds.push((":after", &after));
        }
        if options.before_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND m.time < :before ");
            binds.push((":before", &before));
        }
        if !options.sender.is_empty() {
            query.push_str("AND m.sender = :sender ");
            binds.push((":sender", &options.sender));
        }
        if text_only {
            query.push_str("AND m.text IS NOT NULL ");
        } else if !remote {
            // Merge two disjoint ordered indexes, avoiding a second timestamp
            // index entry on every text message. LIMIT applies to their union.
            // Turso tenants are clustered by target and time instead.
            let text_query = format!("{query}AND m.text IS NOT NULL ");
            query.push_str("AND m.text IS NULL UNION ALL ");
            query.push_str(&text_query);
        }
        // Messages often share a time, e.g. the lines of a multiline
        // message: order them as they were stored. The (target, time) index
        // entries end with the row ID, so this doesn't need an extra sort.
        if options.take_last {
            query.push_str("ORDER BY time DESC, id DESC ");
        } else {
            query.push_str("ORDER BY time ASC, id ASC ");
        }
        query.push_str("LIMIT :limit");
        binds.push((":limit", &limit));

        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map(binds.as_slice(), |r| {
            Ok((r.get(0)?, lossy_string(r, 1)?, lossy_string(r, 2)?))
        })?;
        let mut l = Vec::new();
        for row in rows {
            let (id, time, raw) = row?;
            l.push(crate::database::MessageRow {
                id,
                time,
                message: Message::parse(&raw)?,
            });
        }
        if options.take_last {
            l.reverse();
        }
        Ok(l)
    }

    /// Deletes up to limit messages older than before. Returns the number of
    /// deleted messages: less than limit once there are none left.
    ///
    /// The database is locked meanwhile: deleting in chunks lets other
    /// queries run in between. Expired messages are mostly the oldest, with
    /// the lowest IDs: each chunk finds them at the start of the table.
    pub fn delete_messages_before(&self, before: DateTime<Utc>, limit: usize) -> Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        record_message_id_high_water(&tx)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        if tx.is_remote() {
            // Seek each target's oldest messages in the clustered table
            let n = tx.execute(
                "DELETE FROM Message WHERE (target, time, id) IN (
                    SELECT m.target, m.time, m.id FROM MessageTarget t
                    CROSS JOIN Message m ON m.target = t.id
                    WHERE m.time < :before LIMIT :limit
                )",
                named_params! {":before": xirc::format_server_time(before), ":limit": limit},
            )?;
            tx.commit()?;
            return Ok(n);
        }
        let n = tx.execute(
            "DELETE FROM Message WHERE id IN (
                SELECT id FROM (
                    SELECT m.id FROM MessageTarget t CROSS JOIN Message m ON m.target=t.id
                    WHERE m.text IS NULL AND m.time<:before
                    UNION ALL
                    SELECT m.id FROM MessageTarget t CROSS JOIN Message m ON m.target=t.id
                    WHERE m.text IS NOT NULL AND m.time<:before
                ) LIMIT :limit
            )",
            named_params! {":before":xirc::format_server_time(before),":limit":limit},
        )?;
        tx.commit()?;
        Ok(n)
    }
}

/// Records the highest message ID before deleting messages. SQLite would
/// otherwise reuse the IDs of deleted messages with the highest IDs, and
/// delivery receipts pointing past them would hide new messages.
///
/// The table isn't part of upstream's schema: Go soju ignores it, and keeps
/// reusing IDs if it deletes messages itself.
///
/// Turso tenants reserve every ID through this row before inserting, so it
/// is already up to date there; their clustered table can't seek max(id).
pub(super) fn record_message_id_high_water(conn: &super::Connection) -> Result<()> {
    if conn.is_remote() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO MessageIdHighWater(id, last_id)
        SELECT 0,id FROM Message WHERE true ORDER BY id DESC LIMIT 1
        ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id
        WHERE last_id<excluded.last_id",
        [],
    )?;
    Ok(())
}

/// Returns the stored time and searchable text of a message.
fn message_time_text(msg: &Message) -> Result<(String, Option<String>)> {
    let t = match msg.tags.get("time") {
        Some(tag) => xirc::parse_server_time(tag)
            .ok_or_else(|| anyhow!("failed to parse message time tag: {tag:?}"))?,
        None => Utc::now(),
    };
    let text = match msg.command.as_str() {
        "PRIVMSG" | "NOTICE" if msg.params.len() > 1 => Some(strip_ansi(&msg.params[1])),
        _ => None,
    };
    Ok((xirc::format_server_time(t), text))
}

/// Returns whether a character is a token character for FTS5's unicode61
/// tokenizer: general categories L*, N* and Co.
fn is_fts_token_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

/// Quotes each token of a search query so that FTS5 operators are matched
/// literally.
pub(super) fn quote_fts_query(query: &str) -> String {
    query
        .split(|c: char| !is_fts_token_char(c))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}
