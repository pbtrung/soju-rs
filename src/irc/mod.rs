//! IRC message parsing and formatting, compatible with gopkg.in/irc.v4.

pub mod numerics;

use std::collections::hash_map::Entry;
use std::fmt;

/// Characters which are escaped in tag values.
fn escape_tag_value(v: &str, out: &mut impl fmt::Write) -> fmt::Result {
    for c in v.chars() {
        match c {
            ';' => out.write_str("\\:")?,
            ' ' => out.write_str("\\s")?,
            '\\' => out.write_str("\\\\")?,
            '\r' => out.write_str("\\r")?,
            '\n' => out.write_str("\\n")?,
            c => out.write_char(c)?,
        }
    }
    Ok(())
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

    /// Parses a tag string (without the leading '@'). A duplicate key keeps
    /// its first position and its last value.
    pub fn parse(s: &str) -> Tags {
        let split = |tag: &'_ str| match tag.split_once('=') {
            Some((k, v)) => (k.to_owned(), parse_tag_value(v)),
            None => (tag.to_owned(), String::new()),
        };
        let mut tags = Tags::new();
        if s.bytes().filter(|&b| b == b';').count() < 16 {
            for tag in s.split(';') {
                let (k, v) = split(tag);
                tags.set(k, v);
            }
            return tags;
        }
        // Looking up previous keys linearly would make parsing quadratic in
        // the number of tags, which a client controls
        let mut index = std::collections::HashMap::<String, usize>::new();
        for tag in s.split(';') {
            let (k, v) = split(tag);
            match index.entry(k) {
                Entry::Occupied(e) => tags.0[*e.get()].1 = v,
                Entry::Vacant(e) => {
                    tags.0.push((e.key().clone(), v));
                    e.insert(tags.0.len() - 1);
                }
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
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(";")?;
            }
            f.write_str(k)?;
            if !v.is_empty() {
                f.write_str("=")?;
                escape_tag_value(v, f)?;
            }
        }
        Ok(())
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

    /// The length of the formatted prefix, without formatting it.
    pub fn formatted_len(&self) -> usize {
        let mut n = self.name.len();
        if !self.user.is_empty() {
            n += 1 + self.user.len();
        }
        if !self.host.is_empty() {
            n += 1 + self.host.len();
        }
        n
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
        if !self.tags.is_empty() {
            write!(f, "@{} ", self.tags)?;
        }
        if let Some(prefix) = &self.prefix
            && !prefix.name.is_empty()
        {
            write!(f, ":{prefix} ")?;
        }
        f.write_str(&self.command)?;
        if let Some((trailing, args)) = self.params.split_last() {
            for arg in args {
                f.write_str(" ")?;
                f.write_str(arg)?;
            }
            if trailing.is_empty() || trailing.contains(' ') || trailing.starts_with(':') {
                f.write_str(" :")?;
            } else {
                f.write_str(" ")?;
            }
            f.write_str(trailing)?;
        }
        Ok(())
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
    fn parse_many_tags() {
        // Same semantics with and without the key index
        for n in [4, 100] {
            let s: Vec<String> = (0..n).map(|i| format!("k{}={i}", i % (n / 2))).collect();
            let tags = Tags::parse(&s.join(";"));
            let mut want = Tags::new();
            for i in 0..n {
                want.set(format!("k{}", i % (n / 2)), i.to_string());
            }
            assert_eq!(tags, want);
            assert_eq!(tags.len(), n / 2);
            assert_eq!(tags.get("k0"), Some((n / 2).to_string().as_str()));
        }
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
