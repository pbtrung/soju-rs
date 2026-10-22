//! Message targets, logged messages and their full-text search.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, named_params};

use super::{Database, lossy_string, time_from_sql, time_to_sql};
use crate::database::{MessageOptions, MessageTarget, MessageTargetLast, strip_ansi};
use crate::irc::Message;
use crate::xirc;

impl Database {
    /// Returns the ID of the last message for a target, or 0.
    pub fn get_message_last_id(&self, network_id: i64, name: &str) -> Result<i64> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT m.id FROM Message AS m, MessageTarget AS t
                WHERE t.network = :network AND t.target = :target AND m.target = t.id
                ORDER BY m.time DESC LIMIT 1",
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
                "UPDATE MessageTarget SET pinned = :pinned, muted = :muted, blocked = :blocked WHERE id = :id",
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
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO Message(target, raw, time, sender, text)
                SELECT id, :raw, :time, :sender, :text
                FROM MessageTarget as t
                WHERE network = :network AND target = :target",
            )?;
            for msg in msgs {
                let t = match msg.tags.get("time") {
                    Some(tag) => xirc::parse_server_time(tag)
                        .ok_or_else(|| anyhow!("failed to parse message time tag: {tag:?}"))?,
                    None => Utc::now(),
                };
                let text = match msg.command.as_str() {
                    "PRIVMSG" | "NOTICE" if msg.params.len() > 1 => {
                        Some(strip_ansi(&msg.params[1]))
                    }
                    _ => None,
                };
                stmt.execute(named_params! {
                    ":network": network_id,
                    ":target": name,
                    ":raw": msg.to_string(),
                    ":time": xirc::format_server_time(t),
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
        let mut inner = String::from(
            "SELECT time
            FROM Message
            WHERE target = MessageTarget.id ",
        );
        if !options.events {
            inner.push_str("AND text IS NOT NULL ");
        }
        inner.push_str("ORDER BY time DESC LIMIT 1");

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
        let mut query = String::from(
            "SELECT m.raw
            FROM Message AS m, MessageTarget AS t
            WHERE m.target = t.id AND t.network = :network AND t.target = :target ",
        );
        let after = time_to_sql(options.after_time);
        let before = time_to_sql(options.before_time);
        let text = quote_fts_query(&options.text);
        if !options.text.is_empty() && text.is_empty() {
            // Nothing to search for, e.g. only punctuation: MATCH '' would
            // be a syntax error
            return Ok(Vec::new());
        }
        let limit = options.limit as i64;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> =
            vec![(":network", &network_id), (":target", &name)];
        if options.after_id > 0 {
            query.push_str("AND m.id > :afterID ");
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
        if !options.text.is_empty() {
            query.push_str(
                "AND m.id IN (SELECT ROWID FROM MessageFTS WHERE MessageFTS MATCH :text) ",
            );
            binds.push((":text", &text));
        }
        if !options.events {
            query.push_str("AND m.text IS NOT NULL ");
        }
        if options.take_last {
            query.push_str("ORDER BY m.time DESC ");
        } else {
            query.push_str("ORDER BY m.time ASC ");
        }
        query.push_str("LIMIT :limit");
        binds.push((":limit", &limit));

        let conn = self.conn();
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map(binds.as_slice(), |r| lossy_string(r, 0))?;
        let mut l = Vec::new();
        for raw in rows {
            l.push(Message::parse(&raw?)?);
        }
        if options.take_last {
            l.reverse();
        }
        Ok(l)
    }

    /// Deletes messages older than before. Returns the number of deleted
    /// messages.
    pub fn delete_messages_before(&self, before: DateTime<Utc>) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM Message WHERE time < ?",
            [xirc::format_server_time(before)],
        )?)
    }
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
