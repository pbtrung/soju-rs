//! Extended IRC library: helpers shared by upstream and downstream code.

mod casemapping;
mod genmsg;
mod whox;

pub use casemapping::*;
pub use genmsg::*;
pub use whox::*;

use std::collections::HashMap;

use chrono::{DateTime, NaiveDateTime, Utc};
use indexmap::{IndexMap, IndexSet};

use crate::irc::Message;

pub const MAX_MESSAGE_LENGTH: usize = 512;
pub const MAX_MESSAGE_PARAMS: usize = 15;
pub const MAX_SASL_LENGTH: usize = 400;

/// Formats a time with the IRCv3 server-time layout.
pub fn format_server_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Parses a time with the IRCv3 server-time layout
/// ("2006-01-02T15:04:05.000Z" in Go notation).
///
/// The fractional seconds may have any number of digits or be omitted.
/// This is more lenient than upstream, whose layout requires exactly three
/// digits.
pub fn parse_server_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.strip_suffix('Z')?;
    let (base, frac) = match s.split_once('.') {
        Some((b, f)) => (b, Some(f)),
        None => (s, None),
    };
    let naive = NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H:%M:%S").ok()?;
    if base.len() != 19 {
        return None;
    }
    let mut nanos: u32 = 0;
    if let Some(frac) = frac {
        if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let padded = format!("{frac:0<9}");
        nanos = padded.parse().ok()?;
    }
    let t = naive.and_utc() + chrono::Duration::nanoseconds(nanos as i64);
    Some(t)
}

/// Parses a CTCP message. CTCP is defined in
/// https://tools.ietf.org/html/draft-oakley-irc-ctcp-02
pub fn parse_ctcp_message(msg: &Message) -> Option<(String, String)> {
    if (msg.command != "PRIVMSG" && msg.command != "NOTICE") || msg.params.len() < 2 {
        return None;
    }
    let text = &msg.params[1];
    if !text.starts_with('\x01') {
        return None;
    }
    let text = text.trim_matches('\x01');
    let (cmd, params) = match text.split_once(' ') {
        Some((c, p)) => (c, p),
        None => (text, ""),
    };
    Some((cmd.to_uppercase(), params.to_owned()))
}

/// A set of capabilities, with the available ones and the enabled ones.
#[derive(Clone, Debug, Default)]
pub struct CapRegistry {
    pub available: IndexMap<String, String>,
    pub enabled: IndexSet<String>,
}

impl CapRegistry {
    pub fn new() -> CapRegistry {
        CapRegistry::default()
    }

    pub fn is_available(&self, name: &str) -> bool {
        self.available.contains_key(name)
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    pub fn del(&mut self, name: &str) {
        self.available.shift_remove(name);
        self.enabled.shift_remove(name);
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if enabled {
            self.enabled.insert(name.to_owned());
        } else {
            self.enabled.shift_remove(name);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelStatus {
    Public,
    Secret,
    Private,
}

impl ChannelStatus {
    pub fn parse(s: &str) -> Result<ChannelStatus, String> {
        if s.len() > 1 {
            return Err(format!(
                "invalid channel status {s:?}: more than one character"
            ));
        }
        match s {
            "=" => Ok(ChannelStatus::Public),
            "@" => Ok(ChannelStatus::Secret),
            "*" => Ok(ChannelStatus::Private),
            _ => Err(format!("invalid channel status {s:?}: unknown status")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ChannelStatus::Public => "=",
            ChannelStatus::Secret => "@",
            ChannelStatus::Private => "*",
        }
    }
}

/// A channel member rank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Membership {
    pub mode: u8,
    pub prefix: u8,
}

/// A set of memberships sorted by descending rank.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MembershipSet(pub Vec<Membership>);

impl MembershipSet {
    pub fn add(&mut self, available: &[Membership], new: Membership) {
        let l = &mut self.0;
        let mut i = 0;
        for &avail in available {
            if i >= l.len() {
                break;
            }
            if l[i] == avail {
                if avail == new {
                    // we already have this membership
                    return;
                }
                i += 1;
                continue;
            }
            if avail == new {
                break;
            }
        }
        l.insert(i, new);
    }

    pub fn remove(&mut self, m: Membership) {
        if let Some(i) = self.0.iter().position(|x| *x == m) {
            self.0.remove(i);
        }
    }
}

pub fn decode_isupport_value(v: &str) -> String {
    replace_all(v, &[("\\x20", " "), ("\\x5C", "\\")])
}

pub fn encode_isupport_value(v: &str) -> String {
    replace_all(v, &[(" ", "\\x20"), ("\\", "\\x5C")])
}

/// Replaces substrings left-to-right, like Go's strings.Replacer: at each
/// position, the first matching pattern wins and the output is not
/// rescanned.
pub fn replace_all(s: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < s.len() {
        for (old, new) in pairs {
            if !old.is_empty() && s[i..].starts_with(old) {
                out.push_str(new);
                i += old.len();
                continue 'outer;
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// ISUPPORT tokens: a missing value is represented by None.
pub type Isupport = HashMap<String, Option<String>>;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn server_time() {
        let t = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()
            + chrono::Duration::milliseconds(678);
        assert_eq!(format_server_time(t), "2024-01-02T03:04:05.678Z");
        assert_eq!(parse_server_time("2024-01-02T03:04:05.678Z"), Some(t));
        assert_eq!(
            parse_server_time("2024-01-02T03:04:05Z"),
            Some(Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap())
        );
        assert_eq!(
            parse_server_time("2024-01-02T03:04:05.6Z"),
            Some(
                Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()
                    + chrono::Duration::milliseconds(600)
            )
        );
        assert_eq!(parse_server_time("2024-01-02T03:04:05.678"), None);
        assert_eq!(parse_server_time("garbage"), None);
    }

    #[test]
    fn membership_set() {
        let avail = [
            Membership {
                mode: b'o',
                prefix: b'@',
            },
            Membership {
                mode: b'h',
                prefix: b'%',
            },
            Membership {
                mode: b'v',
                prefix: b'+',
            },
        ];
        let mut ms = MembershipSet::default();
        ms.add(&avail, avail[2]);
        ms.add(&avail, avail[0]);
        ms.add(&avail, avail[0]);
        ms.add(&avail, avail[1]);
        assert_eq!(ms.0, avail.to_vec());
        ms.remove(avail[1]);
        assert_eq!(ms.0, vec![avail[0], avail[2]]);
    }

    #[test]
    fn isupport_value() {
        assert_eq!(decode_isupport_value("a\\x20b\\x5Cc"), "a b\\c");
        assert_eq!(encode_isupport_value("a b\\c"), "a\\x20b\\x5Cc");
    }

    #[test]
    fn ctcp() {
        let m = crate::msg!("PRIVMSG", "#a", "\x01ACTION waves\x01");
        assert_eq!(
            parse_ctcp_message(&m),
            Some(("ACTION".into(), "waves".into()))
        );
        let m = crate::msg!("PRIVMSG", "#a", "\x01version");
        assert_eq!(parse_ctcp_message(&m), Some(("VERSION".into(), "".into())));
    }
}
