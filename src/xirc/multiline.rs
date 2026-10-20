//! draft/multiline support.
//!
//! A multiline message is handled internally as a single logical
//! PRIVMSG/NOTICE whose text contains '\n' line separators. It is converted
//! from and to a draft/multiline batch at the connection boundaries.

use crate::irc::{Message, Tags};

use super::MAX_MESSAGE_LENGTH;

pub const MULTILINE_BATCH_TYPE: &str = "draft/multiline";
pub const MULTILINE_CONCAT_TAG: &str = "draft/multiline-concat";

/// Upper bound for the size of a multiline message, regardless of the
/// limits advertised by the server.
const MAX_MULTILINE_BYTES: usize = 64 * 1024;
const MAX_MULTILINE_LINES: usize = 1024;

/// The smallest line length used when splitting text.
const MIN_LINE_LENGTH: usize = 64;

/// The limits advertised in a draft/multiline capability value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MultilineLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
}

/// soju's own upper bounds.
impl Default for MultilineLimits {
    fn default() -> MultilineLimits {
        MultilineLimits {
            max_bytes: MAX_MULTILINE_BYTES,
            max_lines: MAX_MULTILINE_LINES,
        }
    }
}

impl MultilineLimits {
    /// Parses a capability value such as "max-bytes=4096,max-lines=24".
    /// Missing or invalid limits default to soju's own upper bounds.
    pub fn parse(v: &str) -> MultilineLimits {
        let mut limits = MultilineLimits::default();
        for kv in v.split(',') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            let Ok(n) = v.parse::<usize>() else { continue };
            match k {
                "max-bytes" => limits.max_bytes = n.min(MAX_MULTILINE_BYTES),
                "max-lines" => limits.max_lines = n.min(MAX_MULTILINE_LINES),
                _ => {}
            }
        }
        limits
    }

    /// Formats the limits as a capability value.
    pub fn cap_value(&self) -> String {
        format!("max-bytes={},max-lines={}", self.max_bytes, self.max_lines)
    }
}

/// Returns the maximum length of the text of a PRIVMSG/NOTICE line, so that
/// the full line fits in an IRC message with a prefix of prefix_len bytes.
pub fn max_text_length(prefix_len: usize, command: &str, target: &str) -> usize {
    // ":<prefix> <command> <target> :<text>\r\n"
    let overhead = 1 + prefix_len + 1 + command.len() + 1 + target.len() + 2 + 2;
    MAX_MESSAGE_LENGTH
        .saturating_sub(overhead)
        .max(MIN_LINE_LENGTH)
}

/// How to send a message to a peer, see prepare_text_message.
pub enum TextMessage {
    /// The message as is
    Single(Message),
    /// A draft/multiline batch: BATCH +, the lines, BATCH -
    Batch(Vec<Message>),
    /// One message per line, without draft/multiline support
    Lines(Vec<Message>),
}

/// Prepares a message for a peer: a PRIVMSG/NOTICE spanning several lines,
/// or too long for a single line, is sent as a draft/multiline batch if the
/// peer supports it (multiline), or else line by line. The length of the
/// prefix the peer will see and the batch reference are only computed when
/// needed.
pub fn prepare_text_message(
    msg: Message,
    multiline: bool,
    prefix_len: impl FnOnce() -> usize,
    batch_ref: impl FnOnce() -> String,
) -> TextMessage {
    if !matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE") || msg.params.len() < 2 {
        return TextMessage::Single(msg);
    }
    let breaks = msg.params[1].contains('\n');
    if !breaks && !multiline {
        return TextMessage::Single(msg);
    }
    let max = max_text_length(prefix_len(), &msg.command, &msg.params[0]);
    if multiline && (breaks || msg.params[1].len() > max) {
        TextMessage::Batch(generate_multiline_batch(&msg, &batch_ref(), max))
    } else if breaks {
        TextMessage::Lines(generate_multiline_fallback(&msg, max))
    } else {
        TextMessage::Single(msg)
    }
}

/// Splits the text of a multiline message into lines of at most max bytes.
/// Lines which had to be split are continued with lines flagged for
/// concatenation.
pub fn split_multiline(text: &str, max: usize) -> Vec<(&str, bool)> {
    let max = max.max(MIN_LINE_LENGTH);
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let mut rest = line;
        let mut concat = false;
        loop {
            if rest.len() <= max {
                // Concatenated lines can't be blank
                if !concat || !rest.is_empty() {
                    lines.push((rest, concat));
                }
                break;
            }
            let mut i = max;
            while !rest.is_char_boundary(i) {
                i -= 1;
            }
            lines.push((&rest[..i], concat));
            rest = &rest[i..];
            concat = true;
        }
    }
    lines
}

/// Converts a multiline message into a draft/multiline batch. The tags of
/// the message are carried by the BATCH command.
pub fn generate_multiline_batch(msg: &Message, batch_ref: &str, max: usize) -> Vec<Message> {
    let target = msg.params[0].clone();
    let mut l = vec![
        Message::new(
            "BATCH",
            vec![
                format!("+{batch_ref}"),
                MULTILINE_BATCH_TYPE.to_owned(),
                target.clone(),
            ],
        )
        .with_tags(msg.tags.clone()),
    ];
    l[0].prefix = msg.prefix.clone();
    for (line, concat) in split_multiline(&msg.params[1], max) {
        let mut tags = Tags::new();
        tags.set("batch", batch_ref);
        if concat {
            tags.set(MULTILINE_CONCAT_TAG, "");
        }
        let mut m = Message::new(msg.command.clone(), vec![target.clone(), line.to_owned()])
            .with_tags(tags);
        m.prefix = msg.prefix.clone();
        l.push(m);
    }
    l.push(Message::new("BATCH", vec![format!("-{batch_ref}")]));
    l
}

/// Converts a multiline message into separate messages, for clients and
/// servers which don't support draft/multiline. Only the first message
/// carries the message ID. Blank lines are dropped.
pub fn generate_multiline_fallback(msg: &Message, max: usize) -> Vec<Message> {
    let mut l = Vec::new();
    for (line, _) in split_multiline(&msg.params[1], max) {
        if line.is_empty() {
            continue;
        }
        let mut m = msg.clone();
        m.params[1] = line.to_owned();
        if !l.is_empty() {
            m.tags.remove("msgid");
        }
        l.push(m);
    }
    if l.is_empty() {
        l.push(msg.clone());
        l[0].params[1].clear();
    }
    l
}

/// Errors when assembling a multiline batch, named after the FAIL codes of
/// the specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultilineError {
    MaxBytes,
    MaxLines,
    InvalidTarget,
    Invalid,
}

impl MultilineError {
    pub fn code(&self) -> &'static str {
        match self {
            MultilineError::MaxBytes => "MULTILINE_MAX_BYTES",
            MultilineError::MaxLines => "MULTILINE_MAX_LINES",
            MultilineError::InvalidTarget => "MULTILINE_INVALID_TARGET",
            MultilineError::Invalid => "MULTILINE_INVALID",
        }
    }
}

/// A draft/multiline batch being received.
#[derive(Clone, Debug)]
pub struct MultilineBatch {
    /// The BATCH command which opened the batch.
    pub opener: Message,
    pub target: String,
    command: String,
    text: String,
    bytes: usize,
    lines: usize,
    pub error: Option<MultilineError>,
}

impl MultilineBatch {
    /// Starts a batch from its "BATCH +<ref> draft/multiline <target>"
    /// opener.
    pub fn new(opener: Message) -> MultilineBatch {
        let target = opener.param(2).to_owned();
        let error = if target.is_empty() {
            Some(MultilineError::Invalid)
        } else {
            None
        };
        MultilineBatch {
            opener,
            target,
            command: String::new(),
            text: String::new(),
            bytes: 0,
            lines: 0,
            error,
        }
    }

    /// Appends a message of the batch. Once an error has occurred, further
    /// messages are ignored.
    pub fn push(&mut self, msg: &Message, limits: &MultilineLimits) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = self.push_inner(msg, limits) {
            self.error = Some(e);
            self.text.clear();
        }
    }

    fn push_inner(
        &mut self,
        msg: &Message,
        limits: &MultilineLimits,
    ) -> Result<(), MultilineError> {
        if !matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE")
            || (!self.command.is_empty() && msg.command != self.command)
            || msg.params.len() < 2
        {
            return Err(MultilineError::Invalid);
        }
        if msg.params[0] != self.target {
            return Err(MultilineError::InvalidTarget);
        }
        let line = &msg.params[1];
        let concat = msg.tags.contains(MULTILINE_CONCAT_TAG);
        if concat && line.is_empty() {
            return Err(MultilineError::Invalid);
        }
        self.lines += 1;
        if self.lines > limits.max_lines {
            return Err(MultilineError::MaxLines);
        }
        self.bytes += line.len();
        if self.bytes > limits.max_bytes {
            return Err(MultilineError::MaxBytes);
        }
        if self.lines > 1 && !concat {
            self.text.push('\n');
        }
        self.text.push_str(line);
        self.command = msg.command.clone();
        Ok(())
    }

    /// Completes the batch, returning the logical message. Its tags are the
    /// ones of the BATCH opener.
    pub fn finish(self) -> Result<Message, MultilineError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        // Trailing blank lines carry no meaning, and wouldn't survive being
        // stored as a raw IRC line
        let len = self.text.trim_end_matches('\n').len();
        let mut text = self.text;
        text.truncate(len);
        if text.is_empty() {
            return Err(MultilineError::Invalid);
        }
        let mut msg =
            Message::new(self.command, vec![self.target, text]).with_tags(self.opener.tags);
        msg.prefix = self.opener.prefix;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;

    const LIMITS: MultilineLimits = MultilineLimits {
        max_bytes: 4096,
        max_lines: 24,
    };

    #[test]
    fn limits() {
        assert_eq!(
            MultilineLimits::parse("max-bytes=4096,max-lines=24"),
            LIMITS
        );
        let l = MultilineLimits::parse("max-bytes=1000000");
        assert_eq!(l.max_bytes, MAX_MULTILINE_BYTES);
        assert_eq!(l.max_lines, MAX_MULTILINE_LINES);
    }

    #[test]
    fn split() {
        assert_eq!(
            split_multiline("a\n\nb", 100),
            [("a", false), ("", false), ("b", false)]
        );
        let long = "é".repeat(50);
        let l = split_multiline(&long, 65);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].0.len(), 64);
        assert!(!l[0].1 && l[1].1);
        assert_eq!(format!("{}{}", l[0].0, l[1].0), long);
    }

    #[test]
    fn assemble() {
        let opener = Message::parse("@label=x;+reply=1 :a BATCH +r draft/multiline #c").unwrap();
        let mut b = MultilineBatch::new(opener);
        b.push(&msg!("PRIVMSG", "#c", "hello"), &LIMITS);
        b.push(
            &msg!("PRIVMSG", "#c", " world").with_tag(MULTILINE_CONCAT_TAG, ""),
            &LIMITS,
        );
        b.push(&msg!("PRIVMSG", "#c", ""), &LIMITS);
        b.push(&msg!("PRIVMSG", "#c", "bye"), &LIMITS);
        let m = b.finish().unwrap();
        assert_eq!(m.params, ["#c", "hello world\n\nbye"]);
        assert_eq!(m.tags.get("label"), Some("x"));
        assert_eq!(m.prefix_name(), "a");

        let batch = generate_multiline_batch(&m, "1", 100);
        let lines: Vec<String> = batch.iter().map(|m| m.to_string()).collect();
        assert_eq!(
            lines,
            [
                "@label=x;+reply=1 :a BATCH +1 draft/multiline #c",
                "@batch=1 :a PRIVMSG #c :hello world",
                "@batch=1 :a PRIVMSG #c :",
                "@batch=1 :a PRIVMSG #c bye",
                "BATCH -1",
            ]
        );

        let m = m.with_tag("msgid", "id");
        let l = generate_multiline_fallback(&m, 100);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].tags.get("msgid"), Some("id"));
        assert_eq!(l[1].tags.get("msgid"), None);
        assert_eq!(l[1].params, ["#c", "bye"]);
    }

    #[test]
    fn errors() {
        let opener = msg!("BATCH", "+r", "draft/multiline", "#c");
        let mut b = MultilineBatch::new(opener.clone());
        b.push(&msg!("PRIVMSG", "#d", "hello"), &LIMITS);
        assert_eq!(b.finish().unwrap_err(), MultilineError::InvalidTarget);

        let mut b = MultilineBatch::new(opener.clone());
        b.push(&msg!("PRIVMSG", "#c", "a"), &LIMITS);
        b.push(&msg!("NOTICE", "#c", "b"), &LIMITS);
        assert_eq!(b.finish().unwrap_err(), MultilineError::Invalid);

        let mut b = MultilineBatch::new(opener.clone());
        for _ in 0..25 {
            b.push(&msg!("PRIVMSG", "#c", "a"), &LIMITS);
        }
        assert_eq!(b.finish().unwrap_err(), MultilineError::MaxLines);

        let mut b = MultilineBatch::new(opener.clone());
        b.push(&msg!("PRIVMSG", "#c", "a".repeat(4097)), &LIMITS);
        assert_eq!(b.finish().unwrap_err(), MultilineError::MaxBytes);

        let b = MultilineBatch::new(opener);
        assert_eq!(b.finish().unwrap_err(), MultilineError::Invalid);
    }
}
