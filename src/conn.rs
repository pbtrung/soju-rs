//! Generic IRC connections: line-based streams (TCP, TLS, Unix sockets)
//! and WebSockets, with a dedicated writer task per connection.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite, BufReader, BufWriter};
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;

use crate::irc::Message;
use crate::logger::Logger;
use crate::rate::RateLimiter;
use crate::{debugf, logf};

mod reader;
mod transport;

pub use transport::Reader;
use transport::{Writer, format_line, redact_line_for_log};

pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum length of an incoming line or WebSocket message. IRCv3 lines are
/// at most 8191 bytes of tags plus 512 bytes: leave room for servers with a
/// larger LINELEN.
pub const MAX_LINE_LENGTH: usize = 32768;

/// Maximum size of the messages queued on a connection and not written yet.
/// A peer which doesn't read fast enough is disconnected once it's reached,
/// instead of letting the queue grow without bounds.
pub const MAX_QUEUED_BYTES: usize = 16 << 20;

/// Accounted size of a queued line on top of its length, for the channel
/// node and string header.
const QUEUED_LINE_OVERHEAD: usize = 64;

/// Maximum number of messages read from a connection and still being
/// processed (see ConnReader::read_message_limited). Once it's reached, the
/// connection isn't read anymore: the peer is slowed down by TCP flow
/// control instead of growing the event queue.
pub const MAX_INPUT_IN_FLIGHT: usize = 64;

/// Keepalives bypass ordinary input backpressure but still have a bounded
/// backlog, so PING/PONG floods cannot fill the user's event queue.
const MAX_KEEPALIVE_IN_FLIGHT: usize = 4;

/// Held while a message returned by ConnReader::read_message_limited is
/// being processed. Keepalives use their own pool of permits.
pub type InputPermit = Option<tokio::sync::OwnedSemaphorePermit>;

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

pub type BoxIo = Box<dyn Io>;

/// Static information about a connection.
#[derive(Clone, Debug, Default)]
pub struct ConnInfo {
    /// Remote address, as printed in logs and used for the downstream
    /// hostname ("ip:port" for TCP)
    pub remote_addr: String,
    /// Remote IP for TCP connections (possibly from a trusted proxy)
    pub remote_ip: Option<IpAddr>,
    /// DER-encoded TLS client certificate
    pub peer_cert: Option<Vec<u8>>,
}

enum Outgoing {
    /// A line formatted by format_line
    Line(String),
    Shutdown,
}

struct Shared {
    tx: mpsc::UnboundedSender<Outgoing>,
    /// The number of messages queued and not written yet
    queued: AtomicUsize,
    /// The accounted size of these messages, see MAX_QUEUED_BYTES
    queued_bytes: AtomicUsize,
    closed: AtomicBool,
    cancel: CancellationToken,
    logger: Logger,
}

/// A virtual connection multiplexed on a connection (soju.im/multiplex):
/// its messages carry a tag, and closing it leaves the connection open.
struct View {
    tag_key: String,
    tag_value: String,
    closed: AtomicBool,
    /// Called once the view is closed
    on_close: Box<dyn Fn() + Send + Sync>,
}

/// A handle to a connection's writer. Cloning is cheap.
#[derive(Clone)]
pub struct Conn {
    shared: Arc<Shared>,
    view: Option<Arc<View>>,
}

/// Rate limiting for outgoing messages: one message every `delay`, with a
/// burst.
#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    pub delay: Duration,
    pub burst: u32,
}

/// Shared flag telling the writer whether to rate limit (upstreams turn it
/// off when the server advertises soju.im/SAFERATE).
pub type RateLimitFlag = Arc<AtomicBool>;

impl Conn {
    /// Splits a connection into a reader and a writer handle. The writer
    /// task runs until the connection is closed.
    pub fn new_stream(
        io: BoxIo,
        logger: Logger,
        rate: Option<(RateLimit, RateLimitFlag)>,
    ) -> (Conn, ConnReader) {
        let (r, w) = tokio::io::split(io);
        Self::spawn(
            Reader::Lines(BufReader::with_capacity(8192, r)),
            Writer::Stream(BufWriter::new(w)),
            logger,
            rate,
        )
    }

    pub fn new_websocket(ws: WebSocketStream<BoxIo>, logger: Logger) -> (Conn, ConnReader) {
        let (w, r) = ws.split();
        Self::spawn(Reader::Ws(r), Writer::Ws(w), logger, None)
    }

    fn spawn(
        reader: Reader,
        mut writer: Writer,
        logger: Logger,
        rate: Option<(RateLimit, RateLimitFlag)>,
    ) -> (Conn, ConnReader) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            tx,
            queued: AtomicUsize::new(0),
            queued_bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            logger: logger.clone(),
        });
        let conn = Conn { shared, view: None };

        let c = conn.clone();
        tokio::spawn(async move {
            let cancel = c.shared.cancel.clone();
            let mut limiter = rate
                .as_ref()
                .map(|(r, _)| RateLimiter::new(r.delay, r.burst));
            loop {
                let out = tokio::select! {
                    out = rx.recv() => out,
                    _ = cancel.cancelled() => None,
                };
                let line = match out {
                    Some(Outgoing::Line(l)) => l,
                    Some(Outgoing::Shutdown) | None => break,
                };
                c.shared.queued.fetch_sub(1, Ordering::Relaxed);
                c.shared
                    .queued_bytes
                    .fetch_sub(line.len() + QUEUED_LINE_OVERHEAD, Ordering::Relaxed);
                let limited = rate
                    .as_ref()
                    .is_some_and(|(_, f)| f.load(Ordering::Relaxed));
                if let Some(l) = limiter.as_mut()
                    && limited
                {
                    tokio::select! {
                        _ = l.wait() => {}
                        _ = cancel.cancelled() => break,
                    }
                }
                debugf!(c.shared.logger, "sent: {}", redact_line_for_log(&line));
                // Batch lines in a write when more are ready, e.g. backlog.
                // Rate limited lines are sent as soon as allowed.
                let flush = limited || rx.is_empty();
                match tokio::time::timeout(WRITE_TIMEOUT, writer.write(line, flush)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        logf!(c.shared.logger, "failed to write message: {e}");
                        break;
                    }
                    Err(_) => {
                        logf!(c.shared.logger, "failed to write message: i/o timeout");
                        break;
                    }
                }
            }
            c.close();
            let _ = tokio::time::timeout(WRITE_TIMEOUT, writer.close()).await;
            debugf!(c.shared.logger, "connection closed");
        });

        debugf!(logger, "new connection");
        let reader = ConnReader {
            reader,
            input: Arc::new(Semaphore::new(MAX_INPUT_IN_FLIGHT)),
            keepalive_input: Arc::new(Semaphore::new(MAX_KEEPALIVE_IN_FLIGHT)),
            line_buf: Vec::new(),
            pending: None,
            cancel: conn.shared.cancel.clone(),
            logger,
        };
        (conn, reader)
    }

    /// A virtual connection on this one: its messages are sent with the
    /// tag, and closing it calls on_close instead of closing this
    /// connection. Unless closed with close_quietly, the peer gets an ERROR
    /// with the tag first.
    pub fn view(
        &self,
        tag_key: &str,
        tag_value: &str,
        on_close: impl Fn() + Send + Sync + 'static,
    ) -> Conn {
        Conn {
            shared: self.shared.clone(),
            view: Some(Arc::new(View {
                tag_key: tag_key.to_owned(),
                tag_value: tag_value.to_owned(),
                closed: AtomicBool::new(false),
                on_close: Box::new(on_close),
            })),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
            || self
                .view
                .as_ref()
                .is_some_and(|v| v.closed.load(Ordering::SeqCst))
    }

    /// Closes the connection immediately. Safe to call several times.
    pub fn close(&self) {
        if let Some(v) = &self.view {
            self.close_view(v, true);
        } else {
            self.close_shared();
        }
    }

    fn close_shared(&self) {
        if !self.shared.closed.swap(true, Ordering::SeqCst) {
            self.shared.cancel.cancel();
        }
    }

    /// Like close, but a view is closed without telling the peer, e.g.
    /// because it asked for it.
    pub fn close_quietly(&self) {
        match &self.view {
            Some(v) => self.close_view(v, false),
            None => self.close(),
        }
    }

    fn close_view(&self, v: &View, notify: bool) {
        if self.is_closed() {
            v.closed.store(true, Ordering::SeqCst);
            return;
        }
        if notify {
            self.send_message(crate::msg!("ERROR", "Closing link"));
        }
        if !v.closed.swap(true, Ordering::SeqCst) {
            (v.on_close)();
        }
    }

    /// Queues an outgoing message. Messages sent after the connection is
    /// closed are silently dropped. The connection is closed if more than
    /// MAX_QUEUED_BYTES would be queued.
    pub fn send_message(&self, msg: Message) {
        if self.is_closed() {
            return;
        }
        let line = match &self.view {
            Some(v) => format_line(&msg.with_tag(&v.tag_key, &v.tag_value)),
            None => format_line(&msg),
        };
        let size = line.len() + QUEUED_LINE_OVERHEAD;
        let queued = self.shared.queued_bytes.fetch_add(size, Ordering::Relaxed);
        if queued + size > MAX_QUEUED_BYTES {
            self.shared.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            logf!(
                self.shared.logger,
                "closing connection: too many messages waiting to be sent"
            );
            // The budget belongs to the physical connection. Closing a view
            // would queue an ERROR on the same full queue and recurse.
            self.close_shared();
            return;
        }
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        let _ = self.shared.tx.send(Outgoing::Line(line));
    }

    /// The number of messages queued and not written yet, e.g. because the
    /// peer doesn't read fast enough.
    pub fn queued(&self) -> usize {
        self.shared.queued.load(Ordering::Relaxed)
    }

    /// The accounted size of the messages waiting to be sent.
    pub fn queued_bytes(&self) -> usize {
        self.shared.queued_bytes.load(Ordering::Relaxed)
    }

    /// Gracefully closes the connection, flushing pending messages first.
    pub fn shutdown(&self) {
        if self.view.is_some() {
            // Messages of a view are queued on the connection: they're
            // still sent once it's closed
            self.close();
            return;
        }
        if self.is_closed() {
            return;
        }
        let _ = self.shared.tx.send(Outgoing::Shutdown);
    }
}

/// The reading side of a connection.
pub struct ConnReader {
    reader: Reader,
    input: Arc<Semaphore>,
    keepalive_input: Arc<Semaphore>,
    /// A partial stream line retained across cancellation of read_message.
    line_buf: Vec<u8>,
    /// A message read while waiting for an input permit
    pending: Option<Message>,
    cancel: CancellationToken,
    logger: Logger,
}

/// Splits host:port and returns the host, or the whole string on failure.
pub fn host_of(addr: &str) -> String {
    match crate::database::split_host_port(addr) {
        Ok((h, _)) => h,
        Err(_) => addr.to_owned(),
    }
}

#[cfg(test)]
mod tests;
