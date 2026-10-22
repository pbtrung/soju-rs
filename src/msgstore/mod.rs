//! Per-user stores for IRC messages.

mod bare;
mod db;
mod memory;

pub use db::DbStore;
pub use memory::MemoryStore;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};

use crate::database::Network;
use crate::irc::Message;

pub struct LoadMessageOptions<'a> {
    pub network: &'a Network,
    pub entity: &'a str,
    pub limit: usize,
    pub events: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatHistoryTarget {
    pub name: String,
    pub latest_message: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchMessageOptions {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub limit: usize,
    pub from: String,
    pub r#in: String,
    pub text: String,
}

/// A per-user message store.
pub enum Store {
    Memory(MemoryStore),
    Db(DbStore),
}

impl Store {
    pub fn is_memory(&self) -> bool {
        matches!(self, Store::Memory(_))
    }

    /// Whether the store supports chat history operations.
    pub fn supports_chathistory(&self) -> bool {
        !self.is_memory()
    }

    /// Whether the store supports server-side search.
    pub fn supports_search(&self) -> bool {
        !self.is_memory()
    }

    /// Drops the in-memory messages of a deleted network. Persistent stores
    /// delete them with the network.
    pub fn delete_network(&mut self, net_id: i64) {
        if let Store::Memory(s) = self {
            s.delete_network(net_id);
        }
    }

    pub fn close(&mut self) -> Result<()> {
        match self {
            Store::Memory(s) => {
                s.close();
                Ok(())
            }
            Store::Db(_) => Ok(()),
        }
    }

    /// Queries the last message ID for the given network and entity. The
    /// message ID returned may not refer to a valid message, but can be used
    /// in history queries.
    pub fn last_msg_id(&mut self, network: &Network, entity: &str) -> Result<String> {
        match self {
            Store::Memory(s) => Ok(s.last_msg_id(network, entity)),
            Store::Db(s) => s.last_msg_id(network, entity),
        }
    }

    /// Queries the latest non-event messages after the given message ID, up
    /// to a count of limit messages, sorted from oldest to newest.
    pub fn load_latest_id(
        &mut self,
        id: &str,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        match self {
            Store::Memory(s) => s.load_latest_id(id, options),
            Store::Db(s) => s.load_latest_id(id, options),
        }
    }

    /// Appends a message. Returns the internal message ID, or an empty
    /// string if the message isn't stored.
    pub fn append(&mut self, network: &Network, entity: &str, msg: &Message) -> Result<String> {
        match self {
            Store::Memory(s) => Ok(s.append(network, entity, msg)),
            Store::Db(s) => s.append(network, entity, msg),
        }
    }

    /// Lists channels and nicknames by time of the latest message. It
    /// returns up to limit targets, starting from start and ending on end,
    /// both excluded. end may be before or after start. If events is false,
    /// only PRIVMSG/NOTICE messages are considered.
    pub fn list_targets(
        &mut self,
        network: &Network,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
        events: bool,
    ) -> Result<Vec<ChatHistoryTarget>> {
        match self {
            Store::Memory(_) => bail!("chat history unsupported"),
            Store::Db(s) => s.list_targets(network, start, end, limit, events),
        }
    }

    /// Loads up to limit messages before start down to end. The returned
    /// messages are between and excluding the provided bounds. Missing
    /// bounds mean no bound.
    pub fn load_before_time(
        &mut self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        match self {
            Store::Memory(_) => bail!("chat history unsupported"),
            Store::Db(s) => s.load_before_time(start, end, options),
        }
    }

    /// Loads up to limit messages after start up to end. The returned
    /// messages are between and excluding the provided bounds. Missing
    /// bounds mean no bound.
    pub fn load_after_time(
        &mut self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        match self {
            Store::Memory(_) => bail!("chat history unsupported"),
            Store::Db(s) => s.load_after_time(start, end, options),
        }
    }

    pub fn search(
        &mut self,
        network: &Network,
        options: &SearchMessageOptions,
    ) -> Result<Vec<Message>> {
        match self {
            Store::Memory(_) => bail!("search unsupported"),
            Store::Db(s) => s.search(network, options),
        }
    }
}

const MSG_ID_NONE: u64 = 0;
const MSG_ID_MEMORY: u64 = 1;
/// Used by upstream's fs store, which soju-rs doesn't support: such IDs may
/// still be found in databases, e.g. in delivery receipts.
#[cfg(test)]
const MSG_ID_FS: u64 = 2;
const MSG_ID_DB: u64 = 3;
const MSG_ID_VERSION: u64 = 0;

/// Formats an internal message ID: a BARE header (version, network ID,
/// target, type) followed by a type-specific body, base64url-encoded.
fn format_msg_id(
    net_id: i64,
    target: &str,
    typ: u64,
    body: impl FnOnce(&mut bare::Writer),
) -> String {
    let mut w = bare::Writer::default();
    w.uint(MSG_ID_VERSION);
    w.int(net_id);
    w.string(target);
    w.uint(typ);
    body(&mut w);
    URL_SAFE_NO_PAD.encode(&w.0)
}

/// Parses an internal message ID header. If typ isn't MSG_ID_NONE, checks
/// the type and returns a reader positioned at the body.
fn parse_msg_id_with(s: &str, typ: u64) -> Result<(i64, String, Vec<u8>)> {
    let b = URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| anyhow!("invalid internal message ID: {e}"))?;
    let mut r = bare::Reader(&b);
    let header = (|| -> Result<(u64, i64, String, u64)> {
        Ok((r.uint()?, r.int()?, r.string()?, r.uint()?))
    })()
    .map_err(|e| anyhow!("invalid internal message ID: {e}"))?;
    let (version, net_id, target, got_typ) = header;
    if version != MSG_ID_VERSION {
        bail!("invalid internal message ID: got version {version}, want {MSG_ID_VERSION}");
    }
    if typ != MSG_ID_NONE && got_typ != typ {
        bail!("invalid internal message ID: got type {got_typ}, want {typ}");
    }
    Ok((net_id, target, r.0.to_vec()))
}

/// Parses the network ID and target of an internal message ID.
pub fn parse_msg_id(s: &str) -> Result<(i64, String)> {
    let (net_id, target, _) = parse_msg_id_with(s, MSG_ID_NONE)?;
    Ok((net_id, target))
}

fn body_err(e: anyhow::Error) -> anyhow::Error {
    anyhow!("invalid internal message ID: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference IDs produced by upstream Go soju
    #[test]
    fn go_compat() {
        assert_eq!(
            memory::format_memory_msg_id(42, "#soju", 1234567),
            "AFQFI3NvanUBh61L"
        );
        assert_eq!(db::format_db_msg_id(7, "bob", 99999), "AA4DYm9iA5-NBg");
        assert_eq!(db::format_db_msg_id(-3, "", 0), "AAUAAwA");

        assert_eq!(
            parse_msg_id("AFQFI3NvanUBh61L").unwrap(),
            (42, "#soju".to_owned())
        );
        assert_eq!(
            memory::parse_memory_msg_id("AFQFI3NvanUBh61L").unwrap(),
            1234567
        );
        assert!(db::parse_db_msg_id("AFQFI3NvanUBh61L").is_err());
        assert_eq!(db::parse_db_msg_id("AA4DYm9iA5-NBg").unwrap(), 99999);
        // An fs store ID made by upstream: recognized, but not a DB one
        assert_eq!(
            parse_msg_id("AAoFI2NoYW4C0B8GEoBA").unwrap(),
            (5, "#chan".to_owned())
        );
        let (_, _, body) = parse_msg_id_with("AAoFI2NoYW4C0B8GEoBA", MSG_ID_FS).unwrap();
        assert!(!body.is_empty());
        assert!(db::parse_db_msg_id("AAoFI2NoYW4C0B8GEoBA").is_err());
        assert!(parse_msg_id("!!!").is_err());
        assert!(parse_msg_id("").is_err());
    }
}
