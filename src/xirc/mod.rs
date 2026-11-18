//! Extended IRC library: helpers shared by upstream and downstream code.

mod casemapping;
mod genmsg;
mod multiline;
mod whox;

pub use casemapping::*;
pub use genmsg::*;
pub use multiline::*;
pub use whox::*;

use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};
use jiff::Timestamp;

pub const MAX_MESSAGE_LENGTH: usize = 512;
pub const MAX_MESSAGE_PARAMS: usize = 15;
pub const MAX_SASL_LENGTH: usize = 400;

/// Formats a time with the `IRCv3` server-time layout.
#[must_use]
pub fn format_server_time(t: Timestamp) -> String {
    jiff::fmt::temporal::DateTimePrinter::new()
        .precision(Some(3))
        .timestamp_to_string(&t)
}

/// Parses a time with the `IRCv3` server-time layout
/// ("2006-01-02T15:04:05.000Z" in Go notation).
///
/// The fractional seconds may have one through nine digits or be omitted.
/// This is more lenient than upstream, whose layout requires exactly three
/// digits.
/// Leap seconds are constrained to second 59, following Jiff's Unix timescale.
/// Dates outside Jiff's representable timestamp range are rejected.
#[must_use]
pub fn parse_server_time(s: &str) -> Option<Timestamp> {
    let s = s.strip_suffix('Z')?;
    let (base, frac) = match s.split_once('.') {
        Some((b, f)) => (b, Some(f)),
        None => (s, None),
    };
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
    let mut parsed = jiff::fmt::strtime::parse("%Y-%m-%dT%H:%M:%S", base).ok()?;
    parsed.set_offset(Some(jiff::tz::Offset::UTC));
    let t = parsed.to_timestamp().ok()?;
    t.checked_add(jiff::SignedDuration::from_nanos(i64::from(nanos)))
        .ok()
}

/// A set of capabilities, with the available ones and the enabled ones.
#[derive(Clone, Debug, Default)]
pub struct CapRegistry {
    pub available: IndexMap<String, String>,
    pub enabled: IndexSet<String>,
}

impl CapRegistry {
    #[must_use]
    pub fn new() -> CapRegistry {
        CapRegistry::default()
    }

    #[must_use]
    pub fn is_available(&self, name: &str) -> bool {
        self.available.contains_key(name)
    }

    #[must_use]
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
    ///
    /// # Errors
    ///
    /// Returns an error if the input violates the supported IRC syntax or message constraints.
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

    #[must_use]
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
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The surrounding length check or bounded iteration establishes this local collection index."
                )]
                if l[i] == avail {
                    if avail == new {
                        // we already have this membership
                        return;
                    }
                    i += 1;
                    continue;
                }
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

#[must_use]
pub fn decode_isupport_value(v: &str) -> String {
    replace_all(v, &[("\\x20", " "), ("\\x5C", "\\")])
}

#[must_use]
pub fn encode_isupport_value(v: &str) -> String {
    replace_all(v, &[(" ", "\\x20"), ("\\", "\\x5C")])
}

/// Replaces substrings left-to-right, like Go's strings.Replacer: at each
/// position, the first matching pattern wins and the output is not
/// rescanned.
#[must_use]
pub fn replace_all(s: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < s.len() {
        for (old, new) in pairs {
            {
                #![expect(
                    clippy::string_slice,
                    reason = "The surrounding length check or bounded iteration establishes this local collection index."
                )]
                if !old.is_empty() && s[i..].starts_with(old) {
                    out.push_str(new);
                    i += old.len();
                    continue 'outer;
                }
            }
        }
        let Some(c) = s.get(i..).and_then(|s| s.chars().next()) else {
            break;
        };
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

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn server_time() {
        let t = "2024-01-02T03:04:05Z".parse::<Timestamp>().unwrap()
            + jiff::SignedDuration::from_millis(678);
        assert_eq!(format_server_time(t), "2024-01-02T03:04:05.678Z");
        assert_eq!(parse_server_time("2024-01-02T03:04:05.678Z"), Some(t));
        assert_eq!(
            parse_server_time("2024-01-02T03:04:05Z"),
            Some("2024-01-02T03:04:05Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(
            parse_server_time("2024-01-02T03:04:05.6Z"),
            Some(
                "2024-01-02T03:04:05Z".parse::<Timestamp>().unwrap()
                    + jiff::SignedDuration::from_millis(600)
            )
        );
        assert_eq!(parse_server_time("2024-01-02T03:04:05.678"), None);
        assert_eq!(parse_server_time("garbage"), None);
    }

    #[test]
    #[expect(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    #[expect(
        clippy::string_slice,
        reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
    )]
    fn server_time_compatibility() {
        for (input, output) in [
            ("0000-01-01T00:00:00Z", "0000-01-01T00:00:00.000Z"),
            ("9999-12-30T22:00:00.999999999Z", "9999-12-30T22:00:00.999Z"),
            ("1969-12-31T23:59:59.999999999Z", "1969-12-31T23:59:59.999Z"),
            ("2000-02-29T00:00:00Z", "2000-02-29T00:00:00.000Z"),
            ("2024- 1-02T03:04:05Z", "2024-01-02T03:04:05.000Z"),
            ("2024-01-02T 3:04:05Z", "2024-01-02T03:04:05.000Z"),
            ("2016-12-31T23:59:60.123Z", "2016-12-31T23:59:59.123Z"),
        ] {
            assert_eq!(
                format_server_time(parse_server_time(input).expect(input)),
                output,
                "{input}"
            );
        }
        for fraction in [
            "1",
            "12",
            "123",
            "1234",
            "12345",
            "123456",
            "1234567",
            "12345678",
            "123456789",
        ] {
            let input = format!("2024-01-02T03:04:05.{fraction}Z");
            let t = parse_server_time(&input).unwrap();
            let start = parse_server_time("2024-01-02T03:04:05Z").unwrap();
            assert_eq!(
                (t - start).as_nanos(),
                format!("{fraction:0<9}").parse::<i128>().unwrap()
            );
            assert_eq!(
                format_server_time(t),
                format!(
                    "2024-01-02T03:04:05.{:0<3}Z",
                    &fraction[..fraction.len().min(3)]
                )
            );
            assert!(t > parse_server_time("2024-01-02T03:04:05Z").unwrap());
        }
        for input in [
            "1900-02-29T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:61Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00.1234567890Z",
            "2024-01-01T00:00:00.１２Z",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00z",
            "10000-01-01T00:00:00Z",
            "9999-12-30T22:00:01Z",
            "9999-12-31T23:59:59.999999999Z",
            "garbage",
        ] {
            assert!(parse_server_time(input).is_none(), "{input}");
        }
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
}
