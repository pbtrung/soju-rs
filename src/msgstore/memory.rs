use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};

use super::{LoadMessageOptions, MSG_ID_MEMORY, bare, body_err, format_msg_id, parse_msg_id_with};
use crate::database::Network;
use crate::irc::Message;

const MESSAGE_RING_BUFFER_CAP: usize = 4096;

pub(super) fn format_memory_msg_id(net_id: i64, entity: &str, seq: u64) -> String {
    format_msg_id(net_id, entity, MSG_ID_MEMORY, |w| w.uint(seq))
}

pub(super) fn parse_memory_msg_id(s: &str) -> Result<u64> {
    let (_, _, body) = parse_msg_id_with(s, MSG_ID_MEMORY)?;
    bare::Reader(&body).uint().map_err(body_err)
}

/// The latest messages of a target, up to a capacity. Messages are numbered
/// with increasing sequence numbers, starting at 1. Storage grows with the
/// number of messages, rather than being allocated upfront.
struct RingBuffer {
    buf: VecDeque<Message>,
    cap: usize,
    /// The sequence number of the next message
    cur: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> RingBuffer {
        RingBuffer {
            buf: VecDeque::new(),
            cap: capacity,
            cur: 1,
        }
    }

    fn append(&mut self, msg: Message) -> u64 {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(msg);
        let seq = self.cur;
        self.cur += 1;
        seq
    }

    /// The sequence number of the last message, 0 if there is none.
    fn last_seq(&self) -> u64 {
        self.cur - 1
    }

    /// Loads up to limit of the latest messages after the message with the
    /// sequence number seq (excluded).
    fn load_latest_seq(&self, seq: u64, limit: usize) -> Result<Vec<Message>> {
        if seq >= self.cur {
            bail!(
                "loading messages from sequence number ({seq}) greater than the last ({})",
                self.last_seq()
            );
        }
        let n = ((self.cur - seq - 1) as usize)
            .min(self.buf.len())
            .min(limit);
        Ok(self.buf.range(self.buf.len() - n..).cloned().collect())
    }
}

/// An in-memory store, keeping the latest 4K messages per target. Only
/// PRIVMSG and NOTICE messages are stored.
#[derive(Default)]
pub struct MemoryStore {
    buffers: HashMap<(i64, String), RingBuffer>,
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }

    pub(super) fn close(&mut self) {
        self.buffers.clear();
    }

    pub(super) fn last_msg_id(&self, network: &Network, entity: &str) -> String {
        // The ID of the last message: messages loaded after it are the ones
        // appended since
        let seq = self
            .buffers
            .get(&(network.id, entity.to_owned()))
            .map(|rb| rb.last_seq())
            .unwrap_or(0);
        format_memory_msg_id(network.id, entity, seq)
    }

    pub(super) fn append(&mut self, network: &Network, entity: &str, msg: &Message) -> String {
        match msg.command.as_str() {
            // Only append these messages, because load_latest_id shouldn't
            // return other kinds of message.
            "PRIVMSG" | "NOTICE" => {}
            _ => return String::new(),
        }
        let rb = self
            .buffers
            .entry((network.id, entity.to_owned()))
            .or_insert_with(|| RingBuffer::new(MESSAGE_RING_BUFFER_CAP));
        let seq = rb.append(msg.clone());
        format_memory_msg_id(network.id, entity, seq)
    }

    pub(super) fn load_latest_id(
        &self,
        id: &str,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        if options.events {
            bail!("events are unsupported for memory message store");
        }
        let seq = parse_memory_msg_id(id)?;
        match self
            .buffers
            .get(&(options.network.id, options.entity.to_owned()))
        {
            Some(rb) => rb.load_latest_seq(seq, options.limit),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;

    #[test]
    fn ring() {
        let net = Network {
            id: 1,
            ..Network::new("x")
        };
        let mut s = MemoryStore::new();
        let id0 = s.last_msg_id(&net, "#a");
        assert_eq!(s.append(&net, "#a", &msg!("JOIN", "#a")), "");
        for i in 0..10 {
            s.append(&net, "#a", &msg!("PRIVMSG", "#a", i.to_string()));
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
        assert!(s.load_latest_id(&id, &opts).unwrap().is_empty());
        // Messages appended after taking the last ID are loaded
        s.append(&net, "#a", &msg!("PRIVMSG", "#a", "10"));
        let l = s.load_latest_id(&id, &opts).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].params[1], "10");

        let mut rb = RingBuffer::new(4);
        for i in 0..10 {
            rb.append(msg!("PRIVMSG", "#a", i.to_string()));
        }
        let l = rb.load_latest_seq(1, 100).unwrap();
        assert_eq!(
            l.iter().map(|m| m.params[1].as_str()).collect::<Vec<_>>(),
            ["6", "7", "8", "9"]
        );
        assert!(rb.load_latest_seq(100, 1).is_err());
    }
}
