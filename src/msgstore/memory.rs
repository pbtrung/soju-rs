use std::collections::VecDeque;
use std::mem::size_of;

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;

use super::{LoadMessageOptions, MSG_ID_MEMORY, bare, body_err, format_msg_id, parse_msg_id_with};
use crate::database::Network;
use crate::irc::Message;

const MESSAGE_RING_BUFFER_CAP: usize = 4096;

/// Per-user retained-history limits. Allocator/map spare capacity is separately
/// bounded by the message and target counts; bytes account owned message/key data.
#[derive(Clone, Copy, Debug)]
pub struct MemoryLimits {
    pub targets: usize,
    pub messages: usize,
    pub bytes: usize,
}
impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            targets: 1024,
            messages: 16_384,
            bytes: 16 * 1024 * 1024,
        }
    }
}

/// Current accounted history usage; this is not total process RSS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    pub targets: usize,
    pub messages: usize,
    pub bytes: usize,
}

#[cfg(test)]
pub(super) fn format_memory_msg_id(net_id: i64, entity: &str, seq: u64) -> String {
    format_msg_id(net_id, entity, MSG_ID_MEMORY, |w| w.uint(seq))
}
#[cfg(test)]
pub(super) fn parse_memory_msg_id(s: &str) -> Result<u64> {
    let (_, _, body) = parse_msg_id_with(s, MSG_ID_MEMORY)?;
    bare::Reader(&body).uint().map_err(body_err)
}

struct StoredMessage {
    seq: u64,
    message: Message,
    bytes: usize,
}
struct RingBuffer {
    buf: VecDeque<StoredMessage>,
    cap: usize,
    cur: u64,
    bytes: usize,
}
impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap: capacity,
            cur: 1,
            bytes: 0,
        }
    }
    #[cfg(test)]
    fn append(&mut self, message: Message) -> Result<u64> {
        self.append_at(message, self.cur)
    }
    fn append_at(&mut self, message: Message, seq: u64) -> Result<u64> {
        if self.cap == 0 {
            bail!("message buffer capacity must be positive");
        }
        let next = seq
            .checked_add(1)
            .ok_or_else(|| anyhow!("memory message sequence exhausted"))?;
        if self.buf.len() == self.cap
            && let Some(old) = self.buf.pop_front()
        {
            self.bytes -= old.bytes;
        }
        let bytes = message
            .retained_size()
            .saturating_add(size_of::<u64>() + size_of::<usize>());
        self.bytes += bytes;
        self.buf.push_back(StoredMessage {
            seq,
            message,
            bytes,
        });
        self.cur = next;
        Ok(seq)
    }
    fn last_seq(&self) -> u64 {
        self.cur - 1
    }
    fn load_latest_seq(&self, seq: u64, limit: usize) -> Result<Vec<Message>> {
        if seq > self.cur {
            bail!("memory cursor is ahead of retained history");
        }
        let mut messages: Vec<_> = self
            .buf
            .iter()
            .rev()
            .filter(|entry| entry.seq > seq)
            .take(limit)
            .map(|entry| entry.message.clone())
            .collect();
        messages.reverse();
        Ok(messages)
    }
}

/// A bounded per-user memory history. Target eviction follows append recency.
/// Cursors carry an incarnation token and are scoped to network and target.
pub struct MemoryStore {
    buffers: IndexMap<(i64, String), RingBuffer>,
    limits: MemoryLimits,
    usage: MemoryUsage,
    next_seq: u64,
    incarnation: [u64; 2],
}
impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            buffers: IndexMap::new(),
            limits: MemoryLimits::default(),
            usage: MemoryUsage::default(),
            next_seq: 1,
            incarnation: rand::random(),
        }
    }
}
impl MemoryStore {
    /// Uses explicit positive limits.
    ///
    /// # Errors
    /// Returns an error if any limit is zero.
    pub fn with_limits(limits: MemoryLimits) -> Result<Self> {
        if limits.targets == 0 || limits.messages == 0 || limits.bytes == 0 {
            bail!("memory limits must be positive");
        }
        Ok(Self {
            limits,
            ..Self::default()
        })
    }
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn usage(&self) -> MemoryUsage {
        self.usage
    }
    pub fn delete_network(&mut self, net_id: i64) {
        self.buffers.retain(|(id, _), _| *id != net_id);
        self.recount();
    }
    pub(super) fn close(&mut self) {
        self.buffers.clear();
        self.usage = MemoryUsage::default();
    }
    fn target_bytes(entity: &str) -> usize {
        size_of::<((i64, String), RingBuffer)>().saturating_add(entity.len())
    }
    fn recount(&mut self) {
        self.usage = MemoryUsage {
            targets: self.buffers.len(),
            ..MemoryUsage::default()
        };
        for ((_, entity), ring) in &self.buffers {
            self.usage.messages += ring.buf.len();
            self.usage.bytes += Self::target_bytes(entity).saturating_add(ring.bytes);
        }
    }
    fn cursor(&self, network: i64, entity: &str, seq: u64) -> String {
        format_msg_id(network, entity, MSG_ID_MEMORY, |w| {
            w.uint(seq);
            for word in self.incarnation {
                w.uint(word);
            }
        })
    }
    pub(super) fn last_msg_id(&self, network: &Network, entity: &str) -> String {
        let seq = self
            .buffers
            .get(&(network.id, entity.to_owned()))
            .map_or(self.next_seq - 1, RingBuffer::last_seq);
        self.cursor(network.id, entity, seq)
    }
    pub(super) fn append(
        &mut self,
        network: &Network,
        entity: &str,
        msg: &Message,
    ) -> Result<String> {
        if !matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE") {
            return Ok(String::new());
        }
        let next = self
            .next_seq
            .checked_add(1)
            .ok_or_else(|| anyhow!("memory message sequence exhausted"))?;
        let record_bytes = msg
            .retained_size()
            .saturating_add(size_of::<u64>() + size_of::<usize>());
        if Self::target_bytes(entity).saturating_add(record_bytes) > self.limits.bytes {
            bail!("message exceeds memory history byte budget");
        }
        let key = (network.id, entity.to_owned());
        let mut ring = self
            .buffers
            .shift_remove(&key)
            .unwrap_or_else(|| RingBuffer::new(MESSAGE_RING_BUFFER_CAP));
        // Count the active target independently, then evict older whole targets.
        let old_messages = ring.buf.len();
        let old_bytes = ring.bytes;
        ring.append_at(msg.clone(), self.next_seq)?;
        self.usage.messages = self.usage.messages - old_messages + ring.buf.len();
        self.usage.bytes = self
            .usage
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(ring.bytes);
        if old_messages == 0 {
            self.usage.bytes = self.usage.bytes.saturating_add(Self::target_bytes(entity));
            self.usage.targets += 1;
        }
        while self.usage.targets > self.limits.targets
            || self.usage.messages > self.limits.messages
            || self.usage.bytes > self.limits.bytes
        {
            if let Some(((_, old_entity), old)) = self.buffers.shift_remove_index(0) {
                self.usage.targets -= 1;
                self.usage.messages -= old.buf.len();
                self.usage.bytes -= Self::target_bytes(&old_entity).saturating_add(old.bytes);
            } else {
                // Only the active target remains: trim its oldest records.
                let Some(old) = ring.buf.pop_front() else {
                    break;
                };
                ring.bytes -= old.bytes;
                self.usage.messages -= 1;
                self.usage.bytes -= old.bytes;
            }
        }
        let seq = self.next_seq;
        self.next_seq = next;
        self.buffers.insert(key, ring);
        Ok(self.cursor(network.id, entity, seq))
    }
    pub(super) fn load_latest_id(
        &self,
        id: &str,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        if options.events {
            bail!("events are unsupported for memory message store");
        }
        let (network, entity, body) = parse_msg_id_with(id, MSG_ID_MEMORY)?;
        if network != options.network.id || entity != options.entity {
            bail!("memory cursor belongs to another conversation");
        }
        let mut reader = bare::Reader(&body);
        let seq = reader.uint().map_err(body_err)?;
        if reader.0.is_empty() {
            bail!("legacy memory cursor has no store incarnation");
        }
        let generation = [
            reader.uint().map_err(body_err)?,
            reader.uint().map_err(body_err)?,
        ];
        if generation != self.incarnation || !reader.0.is_empty() {
            bail!("memory cursor belongs to another store incarnation");
        }
        if seq >= self.next_seq {
            bail!("memory cursor is ahead of this store");
        }
        match self.buffers.get(&(network, entity)) {
            Some(ring) => ring.load_latest_seq(seq, options.limit),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    #[expect(
        clippy::indexing_slicing,
        reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
    )]
    fn ring() {
        let net = Network {
            id: 1,
            ..Network::new("x")
        };
        let mut s = MemoryStore::new();
        let id0 = s.last_msg_id(&net, "#a");
        assert_eq!(s.append(&net, "#a", &msg!("JOIN", "#a")).unwrap(), "");
        for i in 0..10 {
            s.append(&net, "#a", &msg!("PRIVMSG", "#a", i.to_string()))
                .unwrap();
        }
        let opts = LoadMessageOptions {
            network: &net,
            entity: "#a",
            limit: 3,
            events: false,
        };
        let l = s.load_latest_id(&id0, &opts).unwrap();
        assert_eq!(
            l.iter().map(|m| m.params[1].as_str()).collect::<Vec<_>>(),
            ["7", "8", "9"]
        );
        let id = s.last_msg_id(&net, "#a");
        assert_eq!(
            s.load_latest_id(&id, &opts).unwrap(),
            [] as [crate::irc::Message; 0]
        );
        // Messages appended after taking the last ID are loaded
        s.append(&net, "#a", &msg!("PRIVMSG", "#a", "10")).unwrap();
        let l = s.load_latest_id(&id, &opts).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].params[1], "10");

        let mut rb = RingBuffer::new(4);
        for i in 0..10 {
            rb.append(msg!("PRIVMSG", "#a", i.to_string())).unwrap();
        }
        let l = rb.load_latest_seq(1, 100).unwrap();
        assert_eq!(
            l.iter().map(|m| m.params[1].as_str()).collect::<Vec<_>>(),
            ["6", "7", "8", "9"]
        );
        assert!(rb.load_latest_seq(100, 1).is_err());
        assert_eq!(
            rb.load_latest_seq(11, 1).unwrap(),
            [] as [crate::irc::Message; 0]
        );
        assert!(rb.load_latest_seq(12, 1).is_err());
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn delete_network() {
        let mut s = MemoryStore::new();
        for id in [1, 2] {
            let net = Network {
                id,
                ..Network::new("x")
            };
            s.append(&net, "#a", &msg!("PRIVMSG", "#a", "hi")).unwrap();
        }
        s.delete_network(1);
        assert_eq!(s.buffers.keys().map(|k| k.0).collect::<Vec<_>>(), [2]);
    }
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn exhausted_sequence_and_zero_capacity_do_not_mutate_history() {
        let mut ring = RingBuffer::new(1);
        ring.append(msg!("PRIVMSG", "#a", "retained")).unwrap();
        ring.cur = u64::MAX;
        assert!(ring.append(msg!("PRIVMSG", "#a", "rejected")).is_err());
        assert_eq!(
            ring.buf.front().unwrap().message.params.last().unwrap(),
            "retained"
        );
        assert_eq!(ring.cur, u64::MAX);
        assert_eq!(ring.load_latest_seq(0, usize::MAX).unwrap().len(), 1);
        assert!(
            RingBuffer::new(0)
                .append(msg!("PRIVMSG", "#a", "rejected"))
                .is_err()
        );
    }
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "History-budget fixture operations must succeed unless rejection is asserted."
    )]
    fn aggregate_budgets_and_cursor_recreation() {
        let net = Network {
            id: 1,
            ..Network::new("x")
        };
        let mut store = MemoryStore::with_limits(MemoryLimits {
            targets: 2,
            messages: 3,
            bytes: 4096,
        })
        .unwrap();
        let old = store.last_msg_id(&net, "#a");
        store
            .append(&net, "#a", &msg!("PRIVMSG", "#a", "old"))
            .unwrap();
        for target in ["#b", "#c"] {
            store
                .append(&net, target, &msg!("PRIVMSG", target, "other"))
                .unwrap();
        }
        assert_eq!(store.usage.targets, 2);
        store
            .append(&net, "#a", &msg!("PRIVMSG", "#a", "new"))
            .unwrap();
        let opts = LoadMessageOptions {
            network: &net,
            entity: "#a",
            limit: usize::MAX,
            events: false,
        };
        let messages = store.load_latest_id(&old, &opts).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages.first().unwrap().param(1), "new");
        let before = store.usage();
        assert!(
            store
                .append(&net, "#a", &msg!("PRIVMSG", "#a", "x".repeat(4096)))
                .is_err()
        );
        assert_eq!(store.usage(), before);
        let mut restarted = MemoryStore::new();
        restarted.incarnation = [store.incarnation[0].wrapping_add(1), store.incarnation[1]];
        assert!(restarted.load_latest_id(&old, &opts).is_err());
        let wrong = LoadMessageOptions {
            entity: "#b",
            ..opts
        };
        assert!(store.load_latest_id(&old, &wrong).is_err());
        store.delete_network(net.id);
        assert_eq!(store.usage(), MemoryUsage::default());
        store
            .append(&net, "#a", &msg!("PRIVMSG", "#a", "after deletion"))
            .unwrap();
        assert_eq!(
            store
                .load_latest_id(&old, &opts)
                .unwrap()
                .first()
                .unwrap()
                .param(1),
            "after deletion"
        );
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Churn fixture appends must fit the configured history budget."
    )]
    fn byte_and_message_accounting_stays_bounded_under_churn() {
        let net = Network {
            id: 1,
            ..Network::new("x")
        };
        let limits = MemoryLimits {
            targets: 32,
            messages: 64,
            bytes: 32 * 1024,
        };
        let mut store = MemoryStore::with_limits(limits).unwrap();
        for i in 0..10_000 {
            let target = format!("#target{i}");
            store
                .append(&net, &target, &msg!("PRIVMSG", &target, "x".repeat(512)))
                .unwrap();
            let usage = store.usage();
            assert!(
                usage.targets <= limits.targets
                    && usage.messages <= limits.messages
                    && usage.bytes <= limits.bytes
            );
        }
        for _ in 0..1000 {
            store
                .append(&net, "#hot", &msg!("PRIVMSG", "#hot", "x".repeat(1024)))
                .unwrap();
        }
        assert!(store.usage.bytes <= limits.bytes);
        let counted = store.usage();
        store.recount();
        assert_eq!(store.usage(), counted);
        store.close();
        assert_eq!(store.usage(), MemoryUsage::default());
    }
}
