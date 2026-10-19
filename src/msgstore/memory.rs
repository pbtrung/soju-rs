use std::collections::HashMap;

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

struct RingBuffer {
    buf: Vec<Option<Message>>,
    cur: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> RingBuffer {
        RingBuffer {
            buf: vec![None; capacity],
            cur: 1,
        }
    }

    fn cap(&self) -> u64 {
        self.buf.len() as u64
    }

    fn append(&mut self, msg: Message) -> u64 {
        let seq = self.cur;
        let i = (seq % self.cap()) as usize;
        self.buf[i] = Some(msg);
        self.cur += 1;
        seq
    }

    fn load_latest_seq(&self, seq: u64, limit: usize) -> Result<Vec<Message>> {
        if seq > self.cur {
            bail!(
                "loading messages from sequence number ({seq}) greater than current ({})",
                self.cur
            );
        } else if seq == self.cur {
            return Ok(Vec::new());
        }

        // The query excludes the message with the sequence number seq
        let mut diff = self.cur - seq - 1;
        if diff > self.cap() {
            // We dropped diff - cap entries
            diff = self.cap();
        }
        if diff as usize > limit {
            diff = limit as u64;
        }

        Ok((0..diff)
            .filter_map(|i| {
                let j = ((self.cur - diff + i) % self.cap()) as usize;
                self.buf[j].clone()
            })
            .collect())
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
        let seq = self
            .buffers
            .get(&(network.id, entity.to_owned()))
            .map(|rb| rb.cur)
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
