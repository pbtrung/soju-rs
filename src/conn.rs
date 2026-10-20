//! Generic IRC connections: line-based streams (TCP, TLS, Unix sockets)
//! and WebSockets, with a dedicated writer task per connection.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

use crate::irc::Message;
use crate::logger::Logger;
use crate::rate::RateLimiter;
use crate::{debugf, logf};

pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum length of an incoming line on stream connections.
const MAX_LINE_LENGTH: usize = 1 << 20;

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
    pub local_addr: String,
    /// DER-encoded TLS client certificate
    pub peer_cert: Option<Vec<u8>>,
}

pub enum Reader {
    Lines(BufReader<ReadHalf<BoxIo>>),
    Ws(futures_util::stream::SplitStream<WebSocketStream<BoxIo>>),
}

enum Writer {
    Stream(WriteHalf<BoxIo>),
    Ws(futures_util::stream::SplitSink<WebSocketStream<BoxIo>, WsMessage>),
}

impl Writer {
    async fn write(&mut self, msg: &Message) -> Result<()> {
        match self {
            Writer::Stream(w) => {
                let line = format!("{msg}\r\n");
                w.write_all(line.as_bytes()).await?;
                w.flush().await?;
            }
            Writer::Ws(w) => {
                w.send(WsMessage::text(msg.to_string())).await?;
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
                let text = match frame? {
                    WsMessage::Text(t) => t.to_string(),
                    WsMessage::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    WsMessage::Close(frame) => {
                        return match frame.map(|f| f.code) {
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
        buf.extend_from_slice(&available[..used]);
        r.consume(used);
        if done {
            return Ok(buf.len());
        }
        if buf.len() > MAX_LINE_LENGTH {
            return Err(anyhow!("line too long"));
        }
    }
}

enum Outgoing {
    Msg(Message),
    Shutdown,
}

struct Shared {
    tx: mpsc::UnboundedSender<Outgoing>,
    closed: AtomicBool,
    cancel: CancellationToken,
    logger: Logger,
}

/// A handle to a connection's writer. Cloning is cheap.
#[derive(Clone)]
pub struct Conn {
    shared: Arc<Shared>,
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
            Writer::Stream(w),
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
            closed: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            logger: logger.clone(),
        });
        let conn = Conn { shared };

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
                let msg = match out {
                    Some(Outgoing::Msg(m)) => m,
                    Some(Outgoing::Shutdown) | None => break,
                };
                if let (Some(l), Some((_, flag))) = (limiter.as_mut(), rate.as_ref())
                    && flag.load(Ordering::Relaxed)
                {
                    tokio::select! {
                        _ = l.wait() => {}
                        _ = cancel.cancelled() => break,
                    }
                }
                debugf!(c.shared.logger, "sent: {msg}");
                match tokio::time::timeout(WRITE_TIMEOUT, writer.write(&msg)).await {
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
            cancel: conn.shared.cancel.clone(),
            logger,
        };
        (conn, reader)
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Closes the connection immediately. Safe to call several times.
    pub fn close(&self) {
        if !self.shared.closed.swap(true, Ordering::SeqCst) {
            self.shared.cancel.cancel();
        }
    }

    /// Queues an outgoing message. Messages sent after the connection is
    /// closed are silently dropped.
    pub fn send_message(&self, msg: Message) {
        if self.is_closed() {
            return;
        }
        let _ = self.shared.tx.send(Outgoing::Msg(msg));
    }

    /// Gracefully closes the connection, flushing pending messages first.
    pub fn shutdown(&self) {
        if self.is_closed() {
            return;
        }
        let _ = self.shared.tx.send(Outgoing::Shutdown);
    }
}

/// The reading side of a connection.
pub struct ConnReader {
    reader: Reader,
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
