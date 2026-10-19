//! IRC message parsing and formatting, compatible with gopkg.in/irc.v4.

pub mod numerics;

use std::fmt;

/// Characters which are escaped in tag values.
fn escape_tag_value(v: &str, out: &mut String) {
    for c in v.chars() {
        match c {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Parses an encoded tag value.
pub fn parse_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // A trailing backslash is dropped
            let Some(c2) = chars.next() else { break };
            out.push(match c2 {
                ':' => ';',
                's' => ' ',
                '\\' => '\\',
                'r' => '\r',
                'n' => '\n',
                c2 => c2,
            });
        } else {
            out.push(c);
        }
    }
    out
}

/// IRCv3 message tags. Insertion order is preserved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags(Vec<(String, String)>);

impl Tags {
    pub fn new() -> Tags {
        Tags(Vec::new())
    }

    /// Parses a tag string (without the leading '@').
    pub fn parse(s: &str) -> Tags {
        let mut tags = Tags::new();
        for tag in s.split(';') {
            match tag.split_once('=') {
                Some((k, v)) => tags.set(k, parse_tag_value(v)),
                None => tags.set(tag, ""),
            }
        }
        tags
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }

    /// Returns the value of a tag, or an empty string if it's missing.
    pub fn get_or_empty(&self, k: &str) -> &str {
        self.get(k).unwrap_or("")
    }

    pub fn contains(&self, k: &str) -> bool {
        self.0.iter().any(|(key, _)| key == k)
    }

    pub fn set(&mut self, k: impl Into<String>, v: impl Into<String>) {
        let k = k.into();
        let v = v.into();
        if let Some(entry) = self.0.iter_mut().find(|(key, _)| *key == k) {
            entry.1 = v;
        } else {
            self.0.push((k, v));
        }
    }

    pub fn remove(&mut self, k: &str) -> Option<String> {
        let i = self.0.iter().position(|(key, _)| key == k)?;
        Some(self.0.remove(i).1)
    }

    pub fn retain(&mut self, mut f: impl FnMut(&str, &str) -> bool) {
        self.0.retain(|(k, v)| f(k, v));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl fmt::Display for Tags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                s.push(';');
            }
            s.push_str(k);
            if !v.is_empty() {
                s.push('=');
                escape_tag_value(v, &mut s);
            }
        }
        f.write_str(&s)
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Tags {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Tags {
        let mut tags = Tags::new();
        for (k, v) in iter {
            tags.set(k, v);
        }
        tags
    }
}

/// The source of a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefix {
    pub name: String,
    pub user: String,
    pub host: String,
}

impl Prefix {
    pub fn new(name: impl Into<String>) -> Prefix {
        Prefix {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn full(
        name: impl Into<String>,
        user: impl Into<String>,
        host: impl Into<String>,
    ) -> Prefix {
        Prefix {
            name: name.into(),
            user: user.into(),
            host: host.into(),
        }
    }

    pub fn parse(s: &str) -> Prefix {
        let (mut name, host) = match s.split_once('@') {
            Some((n, h)) => (n, h),
            None => (s, ""),
        };
        let mut user = "";
        if let Some((n, u)) = name.split_once('!') {
            name = n;
            user = u;
        }
        Prefix::full(name, user, host)
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.user.is_empty() {
            write!(f, "!{}", self.user)?;
        }
        if !self.host.is_empty() {
            write!(f, "@{}", self.host)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    ZeroLength,
    MissingDataAfterPrefix,
    MissingDataAfterTags,
    MissingCommand,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParseError::ZeroLength => "irc: cannot parse zero-length message",
            ParseError::MissingDataAfterPrefix => "irc: no message data after prefix",
            ParseError::MissingDataAfterTags => "irc: no message data after tags",
            ParseError::MissingCommand => "irc: missing message command",
        })
    }
}

impl std::error::Error for ParseError {}

/// An IRC message.
///
/// A message without a source has an empty prefix name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub tags: Tags,
    pub prefix: Option<Prefix>,
    pub command: String,
    pub params: Vec<String>,
}

impl Message {
    pub fn new(command: impl Into<String>, params: Vec<String>) -> Message {
        Message {
            command: command.into(),
            params,
            ..Default::default()
        }
    }

    pub fn with_prefix(mut self, prefix: Prefix) -> Message {
        self.prefix = Some(prefix);
        self
    }

    pub fn with_tags(mut self, tags: Tags) -> Message {
        self.tags = tags;
        self
    }

    pub fn with_tag(mut self, k: impl Into<String>, v: impl Into<String>) -> Message {
        self.tags.set(k, v);
        self
    }

    /// Parses a raw IRC line. Trailing CR/LF characters are ignored.
    pub fn parse(line: &str) -> Result<Message, ParseError> {
        let mut line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(ParseError::ZeroLength);
        }

        let mut msg = Message::default();

        if let Some(rest) = line.strip_prefix('@') {
            let (tags, rest) = rest
                .split_once(' ')
                .ok_or(ParseError::MissingDataAfterTags)?;
            msg.tags = Tags::parse(tags);
            line = rest;
            if line.is_empty() {
                return Err(ParseError::MissingCommand);
            }
        }

        if let Some(rest) = line.strip_prefix(':') {
            let (prefix, rest) = rest
                .split_once(' ')
                .ok_or(ParseError::MissingDataAfterPrefix)?;
            msg.prefix = Some(Prefix::parse(prefix));
            line = rest;
        }

        let (middle, trailing) = match line.split_once(" :") {
            Some((m, t)) => (m, Some(t)),
            None => (line, None),
        };
        let mut params: Vec<String> = middle
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if params.is_empty() {
            return Err(ParseError::MissingCommand);
        }
        if let Some(t) = trailing {
            params.push(t.to_owned());
        }

        msg.command = params.remove(0).to_uppercase();
        msg.params = params;
        Ok(msg)
    }

    /// Returns the name of the message source, or an empty string.
    pub fn prefix_name(&self) -> &str {
        self.prefix.as_ref().map(|p| p.name.as_str()).unwrap_or("")
    }

    /// Returns the i'th parameter or an empty string.
    pub fn param(&self, i: usize) -> &str {
        self.params.get(i).map(String::as_str).unwrap_or("")
    }

    /// Returns the last parameter or an empty string.
    pub fn trailing(&self) -> &str {
        self.params.last().map(String::as_str).unwrap_or("")
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::with_capacity(64);
        if !self.tags.is_empty() {
            s.push('@');
            s.push_str(&self.tags.to_string());
            s.push(' ');
        }
        if let Some(prefix) = &self.prefix
            && !prefix.name.is_empty()
        {
            s.push(':');
            s.push_str(&prefix.to_string());
            s.push(' ');
        }
        s.push_str(&self.command);
        if let Some((trailing, args)) = self.params.split_last() {
            for arg in args {
                s.push(' ');
                s.push_str(arg);
            }
            if trailing.is_empty() || trailing.contains(' ') || trailing.starts_with(':') {
                s.push_str(" :");
            } else {
                s.push(' ');
            }
            s.push_str(trailing);
        }
        f.write_str(&s)
    }
}

/// Builds a parameter list from string-like values.
#[macro_export]
macro_rules! params {
    ($($p:expr),* $(,)?) => {
        vec![$(::std::string::String::from($p)),*]
    };
}

/// Builds a message: `msg!("PRIVMSG", target, text)`.
#[macro_export]
macro_rules! msg {
    ($cmd:expr $(, $p:expr)* $(,)?) => {
        $crate::irc::Message::new($cmd, $crate::params![$($p),*])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let m = Message::parse(":nick!user@host PRIVMSG #chan :hello world\r\n").unwrap();
        assert_eq!(m.prefix, Some(Prefix::full("nick", "user", "host")));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#chan", "hello world"]);
        assert_eq!(m.to_string(), ":nick!user@host PRIVMSG #chan :hello world");
    }

    #[test]
    fn parse_tags() {
        let m = Message::parse("@a=b\\sc;d;e=\\:\\\\x\\ :srv cmd  x  y").unwrap();
        assert_eq!(m.tags.get("a"), Some("b c"));
        assert_eq!(m.tags.get("d"), Some(""));
        assert_eq!(m.tags.get("e"), Some(";\\x"));
        assert_eq!(m.command, "CMD");
        assert_eq!(m.params, vec!["x", "y"]);
        assert_eq!(m.to_string(), "@a=b\\sc;d;e=\\:\\\\x :srv CMD x y");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Message::parse(""), Err(ParseError::ZeroLength));
        assert_eq!(Message::parse("\r\n"), Err(ParseError::ZeroLength));
        assert_eq!(Message::parse("@a"), Err(ParseError::MissingDataAfterTags));
        assert_eq!(
            Message::parse(":a"),
            Err(ParseError::MissingDataAfterPrefix)
        );
        assert_eq!(Message::parse(":a  "), Err(ParseError::MissingCommand));
        assert_eq!(Message::parse("@a "), Err(ParseError::MissingCommand));
    }

    #[test]
    fn format_trailing() {
        assert_eq!(msg!("CMD", "a", "").to_string(), "CMD a :");
        assert_eq!(msg!("CMD", ":x").to_string(), "CMD ::x");
        assert_eq!(msg!("CMD").to_string(), "CMD");
        let m = Message::parse("CMD :").unwrap();
        assert_eq!(m.params, vec![""]);
        let m = Message::parse("PING").unwrap();
        assert!(m.params.is_empty());
    }

    #[test]
    fn empty_prefix_not_formatted() {
        let m = msg!("PING", "x").with_prefix(Prefix::new(""));
        assert_eq!(m.to_string(), "PING x");
    }

    #[test]
    fn prefix_parse() {
        assert_eq!(Prefix::parse("a"), Prefix::new("a"));
        assert_eq!(Prefix::parse("a@h"), Prefix::full("a", "", "h"));
        assert_eq!(Prefix::parse("a!u"), Prefix::full("a", "u", ""));
        assert_eq!(Prefix::parse("a!u@h!x"), Prefix::full("a", "u", "h!x"));
    }
}
