use std::collections::HashMap;
use std::sync::Arc;

use super::turso::BatchStore;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::{
    ChatHistoryTarget, LoadMessageOptions, MSG_ID_DB, SearchMessageOptions, bare, body_err,
    format_msg_id, parse_msg_id_with,
};
use crate::database::{Database, MessageOptions, Network};
use crate::irc::Message;

pub(super) fn format_db_msg_id(net_id: i64, target: &str, msg_id: i64) -> String {
    format_msg_id(net_id, target, MSG_ID_DB, |w| w.uint(msg_id as u64))
}

pub(super) fn parse_db_msg_id(s: &str) -> Result<i64> {
    let (_, _, body) = parse_msg_id_with(s, MSG_ID_DB)?;
    Ok(bare::Reader(&body).uint().map_err(body_err)? as i64)
}

/// A persistent store for IRC messages, using the soju database.
pub struct DbStore {
    db: Arc<Database>,
    batch: Option<Box<BatchStore>>,
}

impl DbStore {
    pub fn new(db: Arc<Database>) -> DbStore {
        DbStore { db, batch: None }
    }

    pub fn new_turso(db: Arc<Database>, user_id: i64, limit: usize) -> Self {
        Self {
            batch: Some(Box::new(BatchStore::new(db.clone(), user_id, limit))),
            db,
        }
    }

    pub(super) fn set_batch_limit(&self, limit: usize) {
        if let Some(b) = &self.batch {
            b.set_limit(limit);
        }
    }

    pub(super) fn prepare(&self) -> Result<()> {
        match &self.batch {
            Some(b) => b.prepare(),
            None => Ok(()),
        }
    }

    pub(super) fn has_capacity(&self) -> bool {
        self.batch.as_ref().is_none_or(|b| b.has_capacity())
    }

    pub(super) fn poll(&self) -> Result<()> {
        match &self.batch {
            Some(b) => b.poll(),
            None => Ok(()),
        }
    }

    pub(super) fn flush(&self) -> Result<()> {
        match &self.batch {
            Some(b) => b.flush(),
            None => Ok(()),
        }
    }

    fn list(&self, network: i64, target: &str, opts: &MessageOptions) -> Result<Vec<Message>> {
        let Some(batch) = &self.batch else {
            return self.db.list_messages(network, target, opts);
        };
        // Snapshot the overlay BEFORE the HTTP read. Completion may remove
        // rows meanwhile, but this read cannot miss or duplicate those rows.
        let mut rows = batch.list(network, target, opts)?;
        rows.extend(self.db.list_message_rows(network, target, opts)?);
        rows.sort_by(|a, b| (&a.time, a.id).cmp(&(&b.time, b.id)));
        let mut ids = std::collections::HashSet::new();
        rows.retain(|r| ids.insert(r.id));
        if opts.take_last && rows.len() > opts.limit {
            rows.drain(..rows.len() - opts.limit);
        }
        rows.truncate(opts.limit);
        Ok(rows.into_iter().map(|r| r.message).collect())
    }

    pub(super) fn last_msg_id(&self, network: &Network, entity: &str) -> Result<String> {
        let pending = match &self.batch {
            Some(b) => b.last_id(network.id, entity)?,
            None => 0,
        };
        let id = pending.max(self.db.get_message_last_id(network.id, entity)?);
        Ok(format_db_msg_id(network.id, entity, id))
    }

    pub(super) fn load_latest_id(
        &self,
        id: &str,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        let msg_id = parse_db_msg_id(id)?;
        self.list(
            options.network.id,
            options.entity,
            &MessageOptions {
                after_id: msg_id,
                limit: options.limit,
                take_last: true,
                ..Default::default()
            },
        )
    }

    pub(super) fn append(&self, network: &Network, entity: &str, msg: &Message) -> Result<String> {
        if let Some(batch) = &self.batch {
            return batch.append(network.id, entity, msg);
        }
        let ids = self
            .db
            .store_messages(network.id, entity, std::slice::from_ref(msg))?;
        Ok(format_db_msg_id(network.id, entity, ids[0]))
    }

    pub(super) fn list_targets(
        &self,
        network: &Network,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
        events: bool,
    ) -> Result<Vec<ChatHistoryTarget>> {
        let opts = if start < end {
            MessageOptions {
                after_time: Some(start),
                before_time: Some(end),
                limit,
                events,
                ..Default::default()
            }
        } else {
            MessageOptions {
                after_time: Some(end),
                before_time: Some(start),
                limit,
                events,
                take_last: true,
                ..Default::default()
            }
        };
        let l = if let Some(batch) = &self.batch {
            // Bounds apply to each target's latest message AFTER combining
            // pending and committed data, including out-of-order server times.
            let all = MessageOptions {
                limit: i64::MAX as usize,
                events: opts.events,
                ..Default::default()
            };
            // Pending targets have no highlight or read marker: those come
            // from the database
            let mut targets: HashMap<String, crate::database::MessageTargetLast> = HashMap::new();
            for t in batch
                .targets(network.id, &all)?
                .into_iter()
                .chain(self.db.list_message_last_per_target(network.id, &all)?)
            {
                match targets.get_mut(&t.name) {
                    Some(last) => {
                        last.latest_message = last.latest_message.max(t.latest_message);
                        last.latest_text = last.latest_text.max(t.latest_text);
                        last.highlight = last.highlight.max(t.highlight);
                        last.read = last.read.max(t.read);
                    }
                    None => {
                        targets.insert(t.name.clone(), t);
                    }
                }
            }
            let mut targets: Vec<_> = targets
                .into_values()
                .filter(|t| {
                    t.latest_message.is_some_and(|time| {
                        !opts.after_time.is_some_and(|after| time <= after)
                            && !opts.before_time.is_some_and(|before| time >= before)
                    })
                })
                .collect();
            targets.sort_by(|a, b| (a.latest_message, &a.name).cmp(&(b.latest_message, &b.name)));
            if opts.take_last && targets.len() > opts.limit {
                targets.drain(..targets.len() - opts.limit);
            }
            targets.truncate(opts.limit);
            targets
        } else {
            self.db.list_message_last_per_target(network.id, &opts)?
        };
        Ok(l.into_iter()
            .filter_map(|t| {
                Some(ChatHistoryTarget {
                    unread: t.unread(),
                    mention: t.mention(),
                    latest_message: t.latest_message?,
                    name: t.name,
                })
            })
            .collect())
    }

    pub(super) fn load_before_time(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        self.list(
            options.network.id,
            options.entity,
            &MessageOptions {
                after_time: end,
                before_time: start,
                limit: options.limit,
                events: options.events,
                take_last: true,
                ..Default::default()
            },
        )
    }

    pub(super) fn load_after_time(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        self.list(
            options.network.id,
            options.entity,
            &MessageOptions {
                after_time: start,
                before_time: end,
                limit: options.limit,
                events: options.events,
                ..Default::default()
            },
        )
    }

    pub(super) fn search(
        &self,
        network: &Network,
        options: &SearchMessageOptions,
    ) -> Result<Vec<Message>> {
        self.list(
            network.id,
            &options.r#in,
            &MessageOptions {
                after_time: options.start,
                before_time: options.end,
                limit: options.limit,
                sender: options.from.clone(),
                text: options.text.clone(),
                take_last: true,
                ..Default::default()
            },
        )
    }
}
