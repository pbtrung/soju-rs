//! Two message buffers: one filling and one writing on an independent stream.

use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::database::{
    Database, MessageBuffer, MessageInsert, MessageOptions, MessageRow, MessageTargetLast,
    MessageWriter,
};
use crate::irc::Message;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

struct Flight {
    rows: Arc<Vec<MessageInsert>>,
    reply: Option<mpsc::Receiver<Result<usize>>>,
    retry_at: Instant,
    failures: u32,
}

struct Buffers {
    writer: Arc<MessageWriter>,
    allocator: Arc<MessageWriter>,
    index: MessageBuffer,
    ids: RangeInclusive<i64>,
    next_ids: Option<RangeInclusive<i64>>,
    reserving: Option<mpsc::Receiver<Result<RangeInclusive<i64>>>>,
    reserve_retry: Instant,
    current: Vec<MessageInsert>,
    current_bytes: usize,
    since: Instant,
    flight: Option<Flight>,
}

pub(super) struct BatchStore {
    db: Arc<Database>,
    user_id: i64,
    limit: AtomicUsize,
    buffers: Mutex<Option<Buffers>>,
}

impl BatchStore {
    pub fn new(db: Arc<Database>, user_id: i64, limit: usize) -> Self {
        Self {
            db,
            user_id,
            limit: AtomicUsize::new(limit.clamp(1, crate::config::MAX_CHAT_HISTORY_LIMIT)),
            buffers: Mutex::new(None),
        }
    }

    fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(
            limit.clamp(1, crate::config::MAX_CHAT_HISTORY_LIMIT),
            Ordering::Relaxed,
        );
    }

    pub fn prepare(&self) -> Result<()> {
        let mut buffers = self.buffers.lock().unwrap();
        if buffers.is_none() {
            let writer = Arc::new(self.db.message_writer(self.user_id)?);
            // Amortize ID reservations. IDs are never reused across crashes;
            // gaps in history cursors are intentional and harmless.
            let ids = writer.reserve(self.limit() * 1024)?;
            *buffers = Some(Buffers {
                writer,
                allocator: Arc::new(self.db.message_writer(self.user_id)?),
                index: MessageBuffer::new()?,
                ids,
                next_ids: None,
                reserving: None,
                reserve_retry: Instant::now(),
                current: Vec::new(),
                current_bytes: 0,
                since: Instant::now(),
                flight: None,
            });
            self.reserve_ahead(buffers.as_mut().unwrap())?;
        }
        Ok(())
    }

    fn reserve_ahead(&self, b: &mut Buffers) -> Result<()> {
        if let Some(rx) = &b.reserving {
            match rx.try_recv() {
                Ok(result) => {
                    b.reserving = None;
                    b.reserve_retry = Instant::now() + Duration::from_secs(5);
                    b.next_ids = Some(result?);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    b.reserving = None;
                    b.reserve_retry = Instant::now() + Duration::from_secs(5);
                    return Err(anyhow!("Turso message ID allocator stopped"));
                }
            }
        }
        if b.next_ids.is_none() && b.reserving.is_none() && Instant::now() >= b.reserve_retry {
            let writer = b.allocator.clone();
            let count = self.limit() * 1024;
            let (tx, rx) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("turso-message-ids".into())
                .spawn(move || {
                    let _ = tx.send(writer.reserve(count));
                })?;
            b.reserving = Some(rx);
        }
        Ok(())
    }

    fn start(flight: &mut Flight, writer: &Arc<MessageWriter>) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        let writer = writer.clone();
        let rows = flight.rows.clone();
        std::thread::Builder::new()
            .name("turso-messages".into())
            .spawn(move || {
                let _ = tx.send(writer.write(&rows));
            })?;
        flight.reply = Some(rx);
        Ok(())
    }

    fn reap(b: &mut Buffers, wait: bool) -> Result<()> {
        let Some(flight) = b.flight.as_mut() else {
            return Ok(());
        };
        let Some(reply) = flight.reply.as_ref() else {
            if Instant::now() >= flight.retry_at || wait {
                Self::start(flight, &b.writer)?;
            }
            return Ok(());
        };
        let result = if wait {
            Some(
                reply
                    .recv()
                    .map_err(|_| anyhow!("Turso message writer stopped"))?,
            )
        } else {
            match reply.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err(anyhow!("Turso message writer stopped")))
                }
            }
        };
        if let Some(result) = result {
            flight.reply = None;
            match result {
                Ok(rejected) => {
                    b.index.remove(&flight.rows)?;
                    b.flight = None;
                    if rejected > 0 {
                        return Err(anyhow!(
                            "Turso rejected {rejected} buffered messages; they were not stored"
                        ));
                    }
                }
                Err(err) => {
                    flight.failures += 1;
                    flight.retry_at =
                        Instant::now() + Duration::from_secs(1 << flight.failures.min(5));
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn dispatch(b: &mut Buffers, limit: usize) -> Result<()> {
        if b.flight.is_none() && !b.current.is_empty() {
            b.flight = Some(Flight {
                rows: Arc::new(b.current.drain(..limit.min(b.current.len())).collect()),
                reply: None,
                retry_at: Instant::now(),
                failures: 0,
            });
            b.current_bytes = b.current.iter().map(MessageInsert::bytes).sum();
            Self::start(b.flight.as_mut().unwrap(), &b.writer)?;
        }
        Ok(())
    }

    pub fn append(&self, network: i64, target: &str, msg: &Message) -> Result<String> {
        self.prepare()?;
        let mut lock = self.buffers.lock().unwrap();
        let b = lock.as_mut().unwrap();
        // The event loop applies input backpressure before the next event
        // when both buffers fill. One event may fan out to several targets;
        // finish that event without waiting for an HTTP response.
        if b.ids.is_empty() {
            // Reservations are prefetched independently, never behind the
            // commit connection's mutex or a synchronous HTTP call here.
            b.ids = b
                .next_ids
                .take()
                .ok_or_else(|| anyhow!("Turso message ID reservation pending"))?;
        }
        let id = b.ids.next().unwrap();
        let row = MessageInsert::new(id, network, target, msg)?;
        b.index.insert(&row)?;
        if b.current.is_empty() {
            b.since = Instant::now();
        }
        b.current_bytes += row.bytes();
        b.current.push(row);
        if b.current.len() >= self.limit() || b.current_bytes >= 8 * 1024 * 1024 {
            // Do not wait for an in-flight HTTP response to forward this line.
            Self::dispatch(b, self.limit())?;
        }
        Ok(super::db::format_db_msg_id(network, target, id))
    }

    pub fn poll(&self) -> Result<()> {
        let mut lock = self.buffers.lock().unwrap();
        if let Some(b) = lock.as_mut() {
            self.reserve_ahead(b)?;
            Self::reap(b, false)?;
            if b.current.len() >= self.limit() || b.since.elapsed() >= FLUSH_INTERVAL {
                Self::dispatch(b, self.limit())?;
            }
        }
        Ok(())
    }

    pub fn has_capacity(&self) -> bool {
        let lock = self.buffers.lock().unwrap();
        lock.as_ref().is_none_or(|b| {
            b.current.len() < self.limit()
                && b.current_bytes < 8 * 1024 * 1024
                && (!b.ids.is_empty() || b.next_ids.is_some())
        })
    }

    pub fn flush(&self) -> Result<()> {
        let mut lock = self.buffers.lock().unwrap();
        if let Some(b) = lock.as_mut() {
            let mut failures = 0;
            loop {
                if let Err(e) = Self::reap(b, true) {
                    failures += 1;
                    if failures >= 3 {
                        let pending =
                            b.current.len() + b.flight.as_ref().map_or(0, |f| f.rows.len());
                        return Err(anyhow!(
                            "failed to flush {pending} buffered messages after 3 attempts: {e}"
                        ));
                    }
                }
                Self::dispatch(b, self.limit())?;
                if b.flight.is_none() && b.current.is_empty() {
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn last_id(&self, network: i64, target: &str) -> Result<i64> {
        let lock = self.buffers.lock().unwrap();
        match lock.as_ref() {
            Some(b) => b.index.last_id(network, target),
            None => Ok(0),
        }
    }

    pub fn list(
        &self,
        network: i64,
        target: &str,
        opts: &MessageOptions,
    ) -> Result<Vec<MessageRow>> {
        let lock = self.buffers.lock().unwrap();
        match lock.as_ref() {
            Some(b) => b.index.list(network, target, opts),
            None => Ok(Vec::new()),
        }
    }

    pub fn targets(&self, network: i64, opts: &MessageOptions) -> Result<Vec<MessageTargetLast>> {
        let lock = self.buffers.lock().unwrap();
        match lock.as_ref() {
            Some(b) => b.index.targets(network, opts),
            None => Ok(Vec::new()),
        }
    }
}
