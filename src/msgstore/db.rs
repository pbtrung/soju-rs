use std::sync::Arc;

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
}

impl DbStore {
    pub fn new(db: Arc<Database>) -> DbStore {
        DbStore { db }
    }

    pub(super) fn last_msg_id(&self, network: &Network, entity: &str) -> Result<String> {
        let id = self.db.get_message_last_id(network.id, entity)?;
        Ok(format_db_msg_id(network.id, entity, id))
    }

    pub(super) fn load_latest_id(
        &self,
        id: &str,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        let msg_id = parse_db_msg_id(id)?;
        self.db.list_messages(
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
        let l = self.db.list_message_last_per_target(network.id, &opts)?;
        Ok(l.into_iter()
            .filter_map(|t| {
                Some(ChatHistoryTarget {
                    name: t.name,
                    latest_message: t.latest_message?,
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
        self.db.list_messages(
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
        self.db.list_messages(
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
        self.db.list_messages(
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
