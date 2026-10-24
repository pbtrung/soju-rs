//! Generic IRC connections: line-based streams (TCP, TLS, Unix sockets)
//! and WebSockets, with a dedicated writer task per connection.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, ReadHalf,
    WriteHalf,
};
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

use crate::irc::Message;
use crate::logger::Logger;
use crate::rate::RateLimiter;
use crate::{debugf, logf};

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

/// Held while a message returned by ConnReader::read_message_limited is
/// being processed.
pub type InputPermit = tokio::sync::OwnedSemaphorePermit;

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

pub enum Reader {
    Lines(BufReader<ReadHalf<BoxIo>>),
    Ws(futures_util::stream::SplitStream<WebSocketStream<BoxIo>>),
}

enum Writer {
    /// Buffered: lines are flushed once no more are queued
    Stream(BufWriter<WriteHalf<BoxIo>>),
    Ws(futures_util::stream::SplitSink<WebSocketStream<BoxIo>, WsMessage>),
}

/// Formats a message as a line, without the CRLF terminator. Queuing the
/// line rather than the message keeps a single allocation per queued
/// message, e.g. when a client fetches a lot of history at once.
fn format_line(msg: &Message) -> String {
    let mut line = msg.to_string();
    if line.contains(['\r', '\n']) {
        // Multiline messages must be split before being sent: never let a
        // line break inject another message. Replace line breaks in the
        // message rather than in the line, so that a last parameter which
        // gains spaces is still formatted as a trailing one.
        let clean = |s: &mut String| {
            if s.contains(['\r', '\n']) {
                *s = s.replace(['\r', '\n'], " ");
            }
        };
        let mut msg = msg.clone();
        clean(&mut msg.command);
        msg.params.iter_mut().for_each(clean);
        if let Some(p) = &mut msg.prefix {
            clean(&mut p.name);
            clean(&mut p.user);
            clean(&mut p.host);
        }
        line = msg.to_string();
        // Tag values are escaped, but not tag keys
        if line.contains(['\r', '\n']) {
            line = line.replace(['\r', '\n'], " ");
        }
    }
    // Formatting grows the buffer as it goes: don't keep the spare capacity
    // while the line is queued
    line.shrink_to_fit();
    line
}

impl Writer {
    /// Writes a line formatted by format_line. With flush unset, it may
    /// stay buffered until the next flushed write.
    async fn write(&mut self, line: String, flush: bool) -> Result<()> {
        match self {
            Writer::Stream(w) => {
                w.write_all(line.as_bytes()).await?;
                w.write_all(b"\r\n").await?;
                if flush {
                    w.flush().await?;
                }
            }
            Writer::Ws(w) => {
                w.feed(WsMessage::text(line)).await?;
                if flush {
                    w.flush().await?;
                }
            }
        }
        Ok(())
    }

    async fn close(&mut self) {
        match self {
            Writer::Stream(w) => {
                let _ = w.shutdown().await;
            }
            Writer::Ws(w) => {
                let _ = w
                    .send(WsMessage::Close(Some(
                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: CloseCode::Normal,
                            reason: "".into(),
                        },
                    )))
                    .await;
                let _ = w.close().await;
            }
        }
    }
}

/// Parses a line, using only the part before the first CR or LF so that a
/// WebSocket frame can't smuggle several IRC lines.
fn parse_line(line: &str) -> Result<Option<Message>> {
    let line = match line.find(['\r', '\n']) {
        Some(i) => &line[..i],
        None => line,
    };
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(Message::parse(line)?))
}

impl Reader {
    async fn read_raw(&mut self) -> Result<Option<Message>> {
        match self {
            Reader::Lines(r) => loop {
                let mut buf = Vec::new();
                let n = read_line_limited(r, &mut buf).await?;
                if n == 0 {
                    return Ok(None);
                }
                if !buf.ends_with(b"\n") {
                    // EOF in the middle of a line
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(&buf);
                // It's valid for a message to be empty, ignore these
                if let Some(msg) = parse_line(&line)? {
                    return Ok(Some(msg));
                }
            },
            Reader::Ws(r) => loop {
                let Some(frame) = r.next().await else {
                    return Ok(None);
                };
                let frame = frame?;
                let text = match &frame {
                    WsMessage::Text(t) => std::borrow::Cow::Borrowed(t.as_str()),
                    WsMessage::Binary(b) => String::from_utf8_lossy(b),
                    WsMessage::Close(frame) => {
                        return match frame.as_ref().map(|f| f.code) {
                            None | Some(CloseCode::Normal) | Some(CloseCode::Away) => Ok(None),
                            Some(code) => Err(anyhow!("websocket closed with status {code}")),
                        };
                    }
                    _ => continue,
                };
                match parse_line(&text)? {
                    Some(msg) => return Ok(Some(msg)),
                    None => return Err(anyhow!("irc: cannot parse zero-length message")),
                }
            },
        }
    }
}

async fn read_line_limited(r: &mut BufReader<ReadHalf<BoxIo>>, buf: &mut Vec<u8>) -> Result<usize> {
    loop {
        let available = r.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len());
        }
        let (done, used) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (true, i + 1),
            None => (false, available.len()),
        };
        if buf.len() + used > MAX_LINE_LENGTH {
            return Err(anyhow!("line too long"));
        }
        buf.extend_from_slice(&available[..used]);
        r.consume(used);
        if done {
            return Ok(buf.len());
        }
    }
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
                debugf!(c.shared.logger, "sent: {line}");
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
        } else if !self.shared.closed.swap(true, Ordering::SeqCst) {
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
            self.close();
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
    cancel: CancellationToken,
    logger: Logger,
}

impl ConnReader {
    /// Reads an incoming message. Ok(None) is returned when there are no
    /// more messages (EOF or the connection was closed).
    pub async fn read_message(&mut self) -> Result<Option<Message>> {
        let res = tokio::select! {
            res = self.reader.read_raw() => res,
            _ = self.cancel.cancelled() => return Ok(None),
        };
        if let Ok(Some(msg)) = &res {
            debugf!(self.logger, "received: {msg}");
        }
        res
    }

    /// Like read_message, but first waits until less than
    /// MAX_INPUT_IN_FLIGHT messages previously returned by this function
    /// still have their permit alive. The permit is meant to be dropped once
    /// the message is processed.
    pub async fn read_message_limited(&mut self) -> Result<Option<(Message, InputPermit)>> {
        let permit = tokio::select! {
            p = self.input.clone().acquire_owned() => p.expect("semaphore is never closed"),
            _ = self.cancel.cancelled() => return Ok(None),
        };
        Ok(self.read_message().await?.map(|msg| (msg, permit)))
    }
}

/// Splits host:port and returns the host, or the whole string on failure.
pub fn host_of(addr: &str) -> String {
    match crate::database::split_host_port(addr) {
        Ok((h, _)) => h,
        Err(_) => addr.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_roundtrip() {
        let (a, b) = tokio::io::duplex(4096);
        let logger = Logger::new(false);
        let (ca, mut ra) = Conn::new_stream(Box::new(a), logger.clone(), None);
        let (cb, mut rb) = Conn::new_stream(Box::new(b), logger, None);
        ca.send_message(crate::msg!("PRIVMSG", "#a", "hello world"));
        ca.send_message(crate::msg!("PING", "x"));
        let m = rb.read_message().await.unwrap().unwrap();
        assert_eq!(m.to_string(), "PRIVMSG #a :hello world");
        assert_eq!(rb.read_message().await.unwrap().unwrap().command, "PING");
        ca.shutdown();
        assert!(rb.read_message().await.unwrap().is_none());
        cb.close();
        assert!(rb.read_message().await.unwrap().is_none());
        drop(ra.read_message().await);
    }

    #[tokio::test]
    async fn rate_limited() {
        tokio::time::pause();
        let (a, b) = tokio::io::duplex(4096);
        let logger = Logger::new(false);
        let flag: RateLimitFlag = Arc::new(AtomicBool::new(true));
        let rate = RateLimit {
            delay: Duration::from_secs(2),
            burst: 2,
        };
        let (ca, _ra) = Conn::new_stream(Box::new(a), logger.clone(), Some((rate, flag)));
        let (_cb, mut rb) = Conn::new_stream(Box::new(b), logger, None);
        let start = tokio::time::Instant::now();
        for i in 0..3 {
            ca.send_message(crate::msg!("PING", i.to_string()));
        }
        for _ in 0..3 {
            rb.read_message().await.unwrap().unwrap();
        }
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    fn tagged_message() -> Message {
        crate::msg!("PRIVMSG", "#a", "hello world")
            .with_prefix(crate::irc::Prefix::full("nick", "user", "host"))
            .with_tag("time", "2020-01-01T00:00:00.000Z")
            .with_tag("msgid", "a b;c")
    }

    #[test]
    fn queued_line() {
        let msg = tagged_message();
        let line = format_line(&msg);
        assert_eq!(line, msg.to_string());
        assert_eq!(line.capacity(), line.len());
        // Line breaks can't inject another message
        let line = format_line(&crate::msg!("PRIVMSG", "#a", "x\r\nQUIT"));
        assert_eq!(line, "PRIVMSG #a :x  QUIT");
        let msg = Message::parse(&line).unwrap();
        assert_eq!(msg.params, ["#a", "x  QUIT"]);
        let line = format_line(&crate::msg!("PRIVMSG", "#a\n", "x").with_tag("a\nb", "c\n"));
        assert_eq!(line, "@a b=c\\n PRIVMSG #a  x");
    }

    #[tokio::test]
    async fn stream_wire_format() {
        use tokio::io::AsyncReadExt;
        let (a, mut b) = tokio::io::duplex(4096);
        let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
        let msg = tagged_message();
        c.send_message(msg.clone());
        c.send_message(crate::msg!("PING", "x"));
        c.shutdown();
        let mut wire = Vec::new();
        b.read_to_end(&mut wire).await.unwrap();
        assert_eq!(
            String::from_utf8(wire).unwrap(),
            format!("{msg}\r\nPING x\r\n")
        );
    }

    #[tokio::test]
    async fn websocket_wire_format() {
        let (a, b) = tokio::io::duplex(4096);
        let ws = |io: tokio::io::DuplexStream, role| {
            WebSocketStream::from_raw_socket(Box::new(io) as BoxIo, role, None)
        };
        let (server, mut client) = tokio::join!(
            ws(a, tokio_tungstenite::tungstenite::protocol::Role::Server),
            ws(b, tokio_tungstenite::tungstenite::protocol::Role::Client)
        );
        let (c, _r) = Conn::new_websocket(server, Logger::new(false));
        let msg = tagged_message();
        c.send_message(msg.clone());
        // One text frame per message, without CRLF
        match client.next().await.unwrap().unwrap() {
            WsMessage::Text(t) => assert_eq!(t.as_str(), msg.to_string()),
            m => panic!("unexpected frame: {m:?}"),
        }
    }

    #[tokio::test]
    async fn queue_budget() {
        // The peer never reads
        let (a, _b) = tokio::io::duplex(4096);
        let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
        let text = "x".repeat(400);
        let n = MAX_QUEUED_BYTES / (text.len() + QUEUED_LINE_OVERHEAD) + 1;
        for _ in 0..n {
            c.send_message(crate::msg!("PRIVMSG", "#a", text.clone()));
        }
        assert!(c.is_closed());
        assert!(c.shared.queued_bytes.load(Ordering::Relaxed) <= MAX_QUEUED_BYTES);
    }

    #[tokio::test]
    async fn input_in_flight() {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (ca, _ra) = Conn::new_stream(Box::new(a), Logger::new(false), None);
        let (_cb, mut rb) = Conn::new_stream(Box::new(b), Logger::new(false), None);
        for i in 0..MAX_INPUT_IN_FLIGHT + 1 {
            ca.send_message(crate::msg!("PING", i.to_string()));
        }
        let mut permits = Vec::new();
        for _ in 0..MAX_INPUT_IN_FLIGHT {
            permits.push(rb.read_message_limited().await.unwrap().unwrap());
        }
        let next = tokio::time::timeout(Duration::from_millis(50), rb.read_message_limited());
        assert!(next.await.is_err());
        permits.pop();
        let (msg, _) = rb.read_message_limited().await.unwrap().unwrap();
        assert_eq!(msg.params, [MAX_INPUT_IN_FLIGHT.to_string()]);
    }

    #[tokio::test]
    async fn view() {
        let (a, b) = tokio::io::duplex(4096);
        let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
        let (_cb, mut rb) = Conn::new_stream(Box::new(b), Logger::new(false), None);
        let closed = Arc::new(AtomicUsize::new(0));
        let n = closed.clone();
        let v = c.view("soju.im/network", "1", move || {
            n.fetch_add(1, Ordering::SeqCst);
        });
        v.send_message(crate::msg!("PING", "x").with_tag("label", "a"));
        let m = rb.read_message().await.unwrap().unwrap();
        assert_eq!(m.tags.get("soju.im/network"), Some("1"));
        assert_eq!(m.tags.get("label"), Some("a"));

        // Closing the view tells the peer, and leaves the connection open
        v.close();
        v.close();
        assert!(v.is_closed() && !c.is_closed());
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        let m = rb.read_message().await.unwrap().unwrap();
        assert_eq!(m.command, "ERROR");
        assert_eq!(m.tags.get("soju.im/network"), Some("1"));
        v.send_message(crate::msg!("PING", "dropped"));
        c.send_message(crate::msg!("PING", "y"));
        let m = rb.read_message().await.unwrap().unwrap();
        assert_eq!(m.params, ["y"]);
        assert!(m.tags.get("soju.im/network").is_none());

        // Closed quietly: no ERROR
        let q = c.view("soju.im/network", "2", || {});
        q.close_quietly();
        c.send_message(crate::msg!("PING", "z"));
        assert_eq!(rb.read_message().await.unwrap().unwrap().params, ["z"]);

        // Closing the connection closes its views
        let w = c.view("soju.im/network", "3", || {});
        c.close();
        assert!(w.is_closed());
    }

    #[test]
    fn line_parsing() {
        assert!(parse_line("").unwrap().is_none());
        assert_eq!(
            parse_line("PRIVMSG #a :x\r\nQUIT")
                .unwrap()
                .unwrap()
                .to_string(),
            "PRIVMSG #a x"
        );
    }
}
