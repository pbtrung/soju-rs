//! Stream and WebSocket framing, formatting and size limits.

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, ReadHalf, WriteHalf};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message as WsMessage, protocol::frame::coding::CloseCode},
};

use super::{BoxIo, MAX_LINE_LENGTH};
use crate::irc::Message;

/// Release unusually large buffers after parsing a line.
const MAX_RETAINED_LINE_BUFFER: usize = 1024;

pub enum Reader {
    Lines(BufReader<ReadHalf<BoxIo>>),
    Ws(futures_util::stream::SplitStream<WebSocketStream<BoxIo>>),
}

pub(super) enum Writer {
    /// Buffered: lines are flushed once no more are queued
    Stream(BufWriter<WriteHalf<BoxIo>>),
    Ws(futures_util::stream::SplitSink<WebSocketStream<BoxIo>, WsMessage>),
}

/// Formats a message as a line, without the CRLF terminator. Queuing the
/// line rather than the message keeps a single allocation per queued
/// message, e.g. when a client fetches a lot of history at once.
pub(super) fn format_line(msg: &Message) -> String {
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

pub(super) fn redact_line_for_log(line: &str) -> String {
    match Message::parse(line) {
        Ok(msg) => crate::ircutil::redact_for_log(&msg),
        Err(_) => "<invalid message>".to_owned(),
    }
}

impl Writer {
    /// Writes a line formatted by format_line. With flush unset, it may
    /// stay buffered until the next flushed write.
    pub(super) async fn write(&mut self, line: String, flush: bool) -> Result<()> {
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

    pub(super) async fn close(&mut self) {
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
pub(super) fn parse_line(line: &str) -> Result<Option<Message>> {
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
    pub(super) async fn read_raw(&mut self, pending: &mut Vec<u8>) -> Result<Option<Message>> {
        match self {
            Reader::Lines(r) => loop {
                // Keep bytes across cancelled reads: read_line_limited has
                // already consumed them from the buffered stream.
                let n = read_line_limited(r, pending).await?;
                if n == 0 {
                    return Ok(None);
                }
                if !pending.ends_with(b"\n") {
                    // EOF in the middle of a line
                    pending.clear();
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(pending);
                let parsed = parse_line(&line);
                // Parsing owns the message fields. Clear even on error, so
                // a subsequent read cannot replay the failed line.
                pending.clear();
                if pending.capacity() > MAX_RETAINED_LINE_BUFFER {
                    *pending = Vec::new();
                }
                // It's valid for a message to be empty, ignore these
                if let Some(msg) = parsed? {
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
                if text.len() > MAX_LINE_LENGTH {
                    return Err(anyhow!("line too long"));
                }
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
