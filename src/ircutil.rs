//! IRC helpers used by the bouncer logic (port of upstream's irc.go).

use chrono::{DateTime, Utc};
use indexmap::IndexMap;

use crate::irc::{Message, Tags, numerics::*};
use crate::xirc::{self, CaseMappingMap, Membership, MembershipSet};

/// Formats a message for logs, hiding the parameters of commands carrying
/// credentials.
pub fn redact_for_log(msg: &Message) -> String {
    match msg.command.as_str() {
        "PASS" | "AUTHENTICATE" | "OPER" => format!("{} <redacted>", msg.command),
        _ => msg.to_string(),
    }
}

/// A set of user modes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserModes(pub String);

impl UserModes {
    pub fn has(&self, c: u8) -> bool {
        self.0.as_bytes().contains(&c)
    }

    pub fn add(&mut self, c: u8) {
        // Mode letters are ASCII: other bytes would be stored as multi-byte
        // chars, which del() can't remove by byte position
        if c.is_ascii() && !self.has(c) {
            self.0.push(c as char);
        }
    }

    pub fn del(&mut self, c: u8) {
        if let Some(i) = self.0.bytes().position(|b| b == c) {
            self.0.remove(i);
        }
    }

    pub fn apply(&mut self, s: &str) -> Result<(), String> {
        let mut plus_minus = 0u8;
        for c in s.bytes() {
            match c {
                b'+' | b'-' => plus_minus = c,
                _ => match plus_minus {
                    b'+' => self.add(c),
                    b'-' => self.del(c),
                    _ => {
                        return Err(format!("malformed modestring {s:?}: missing plus/minus"));
                    }
                },
            }
        }
        Ok(())
    }
}

/// Standard channel mode types, as explained in
/// https://modern.ircdocs.horse/#mode-message
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelModeType {
    /// Modes that add or remove an address to or from a list
    A,
    /// Modes that change a setting on a channel, and must always have a
    /// parameter
    B,
    /// Modes that change a setting on a channel, and must have a parameter
    /// when being set, and no parameter when being unset
    C,
    /// Modes that change a setting on a channel, and must not have a
    /// parameter
    D,
}

pub fn std_channel_modes() -> IndexMap<u8, ChannelModeType> {
    use ChannelModeType::*;
    [
        (b'b', A), // ban list
        (b'e', A), // ban exception list
        (b'I', A), // invite exception list
        (b'k', B), // channel key
        (b'l', C), // channel user limit
        (b'i', D), // channel is invite-only
        (b'm', D), // channel is moderated
        (b'n', D), // channel has no external messages
        (b's', D), // channel is secret
        (b't', D), // channel has protected topic
    ]
    .into_iter()
    .collect()
}

pub type ChannelModes = IndexMap<u8, String>;

pub const STD_CHANNEL_TYPES: &str = "#&+!";

pub fn std_memberships() -> Vec<Membership> {
    [
        (b'q', b'~'), // founder
        (b'a', b'&'), // protected
        (b'o', b'@'), // operator
        (b'h', b'%'), // halfop
        (b'v', b'+'), // voice
    ]
    .into_iter()
    .map(|(mode, prefix)| Membership { mode, prefix })
    .collect()
}

/// Parses a mode string and mode arguments from a MODE message, and applies
/// the corresponding channel mode and user membership changes on that
/// channel.
///
/// If modes is None, channel modes are not updated.
pub fn apply_channel_modes(
    mut modes: Option<&mut ChannelModes>,
    members: &mut CaseMappingMap<MembershipSet>,
    available_memberships: &[Membership],
    available_channel_modes: &IndexMap<u8, ChannelModeType>,
    mode_str: &str,
    arguments: &[String],
) -> Result<(), String> {
    let mut next_argument = 0;
    let mut plus_minus = 0u8;
    'outer: for mode in mode_str.bytes() {
        if mode == b'+' || mode == b'-' {
            plus_minus = mode;
            continue;
        }
        if plus_minus != b'+' && plus_minus != b'-' {
            return Err(format!(
                "malformed modestring {mode_str:?}: missing plus/minus"
            ));
        }

        for membership in available_memberships {
            if membership.mode == mode {
                if next_argument >= arguments.len() {
                    return Err(format!(
                        "malformed modestring {:?}: missing mode argument for {}{}",
                        mode_str, plus_minus as char, mode as char
                    ));
                }
                let member = &arguments[next_argument];
                if let Some(m) = members.get_mut(member) {
                    if plus_minus == b'+' {
                        m.add(available_memberships, *membership);
                    } else {
                        // TODO: for upstreams without multi-prefix, query the user modes again
                        m.remove(*membership);
                    }
                }
                next_argument += 1;
                continue 'outer;
            }
        }

        let Some(&mt) = available_channel_modes.get(&mode) else {
            continue;
        };
        match mt {
            ChannelModeType::A => next_argument += 1,
            ChannelModeType::B => {
                if plus_minus == b'+' {
                    // some sensitive arguments (such as channel keys) can be
                    // omitted for privacy (this will only happen for
                    // RPL_CHANNELMODEIS, never for MODE messages)
                    let argument = arguments.get(next_argument).cloned().unwrap_or_default();
                    if let Some(modes) = modes.as_deref_mut() {
                        modes.insert(mode, argument);
                    }
                } else if let Some(modes) = modes.as_deref_mut() {
                    modes.shift_remove(&mode);
                }
                next_argument += 1;
            }
            ChannelModeType::C if plus_minus == b'+' => {
                let argument = arguments.get(next_argument).cloned().unwrap_or_default();
                if let Some(modes) = modes.as_deref_mut() {
                    modes.insert(mode, argument);
                }
                next_argument += 1;
            }
            ChannelModeType::C | ChannelModeType::D => {
                if let Some(modes) = modes.as_deref_mut() {
                    if plus_minus == b'+' {
                        modes.insert(mode, String::new());
                    } else {
                        modes.shift_remove(&mode);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Formats channel modes as a mode string and parameters.
pub fn format_channel_modes(cm: &ChannelModes) -> (String, Vec<String>) {
    let mut with_values = String::new();
    let mut without_values = String::new();
    let mut parameters = Vec::new();
    for (&mode, value) in cm {
        if !value.is_empty() {
            with_values.push(mode as char);
            parameters.push(value.clone());
        } else {
            without_values.push(mode as char);
        }
    }
    (format!("+{with_values}{without_values}"), parameters)
}

pub fn format_member_prefix(ms: &MembershipSet, multi_prefix: bool) -> String {
    if !multi_prefix {
        return ms
            .0
            .first()
            .map(|m| (m.prefix as char).to_string())
            .unwrap_or_default();
    }
    ms.0.iter().map(|m| m.prefix as char).collect()
}

/// Removes channel membership prefixes from flags.
pub fn strip_member_prefixes(flags: &str, memberships: &[Membership]) -> String {
    flags
        .chars()
        .filter(|&c| !memberships.iter().any(|m| c as u32 == m.prefix as u32))
        .collect()
}

pub fn copy_client_tags(tags: &Tags) -> Tags {
    tags.iter().filter(|(k, _)| k.starts_with('+')).collect()
}

fn is_word_boundary(r: Option<char>) -> bool {
    match r {
        // inspired from weechat.look.highlight_regex
        Some('-' | '_' | '|') => false,
        Some(r) => !r.is_alphabetic() && !r.is_numeric(),
        None => true,
    }
}

fn is_uri_prefix(text: &str) -> bool {
    let mut text = text;
    if let Some((i, _)) = text.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
        text = &text[i..];
    }
    let Some(i) = text.find("://") else {
        return false;
    };
    // See RFC 3986 section 3
    match text[..i].chars().next_back() {
        Some('+' | '-' | '.') => true,
        Some(r) => r.is_ascii_alphanumeric(),
        None => false,
    }
}

pub fn is_highlight(text: &str, nick: &str) -> bool {
    assert!(!nick.is_empty(), "is_highlight called with empty nick");
    let mut text = text;
    loop {
        let Some(i) = text.find(nick) else {
            return false;
        };
        let left = text[..i].chars().next_back();
        let right = text[i + nick.len()..].chars().next();
        if is_word_boundary(left) && is_word_boundary(right) && !is_uri_prefix(&text[..i]) {
            return true;
        }
        text = &text[i + nick.len()..];
    }
}

/// Parses a CHATHISTORY parameter as a bound.
pub fn parse_chathistory_bound(param: &str) -> Option<DateTime<Utc>> {
    let (kind, value) = param.split_once('=')?;
    match kind {
        "timestamp" => xirc::parse_server_time(value),
        _ => None,
    }
}

pub fn is_numeric(cmd: &str) -> bool {
    cmd.len() == 3 && cmd.bytes().all(|b| b.is_ascii_digit())
}

pub fn generate_away_reply(away: bool) -> Message {
    if away {
        crate::msg!(RPL_NOWAWAY, "*", "You have been marked as being away")
    } else {
        crate::msg!(RPL_UNAWAY, "*", "You are no longer marked as being away")
    }
}

/// Splits a service command line into words, following POSIX shell quoting
/// rules (port of upstream's splitWords).
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut last_word = String::new();
    let mut escape = false;
    let mut prev = ' ';
    let mut word_delim = ' ';

    for r in s.chars() {
        if escape {
            // last char was a backslash, write the byte as-is.
            last_word.push(r);
            escape = false;
        } else if r == '\\' {
            escape = true;
        } else if word_delim == ' ' && r.is_whitespace() {
            // end of last word
            if !prev.is_whitespace() {
                words.push(std::mem::take(&mut last_word));
            }
        } else if r == word_delim {
            // word_delim is either " or ', switch back to space-delimited
            // words.
            word_delim = ' ';
        } else if r == '"' || r == '\'' {
            if word_delim == ' ' {
                // start of (double-)quoted word
                word_delim = r;
            } else {
                // either word_delim is " and r is ' or vice-versa
                last_word.push(r);
            }
        } else {
            last_word.push(r);
        }
        prev = r;
    }

    if !prev.is_whitespace() {
        words.push(last_word);
    }
    if word_delim != ' ' {
        return Err("unterminated quoted string".to_owned());
    }
    if escape {
        return Err("unterminated backslash sequence".to_owned());
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_modes_non_ascii() {
        let mut m = UserModes::default();
        m.apply("+iw\u{e9}").unwrap();
        // Non-ASCII mode bytes are ignored, rather than panicking on removal
        m.apply("-\u{e9}w").unwrap();
        assert_eq!(m.0, "i");
    }

    #[test]
    fn highlight() {
        let nick = "SojuUser";
        let cases = [
            ("noContains", "hi there Soju User!", false),
            ("standalone", "SojuUser", true),
            ("middle", "hi there SojuUser!", true),
            ("start", "SojuUser: how are you doing?", true),
            ("end", "maybe ask SojuUser", true),
            ("inWord", "but OtherSojuUserSan is a different nick", false),
            (
                "startWord",
                "and OtherSojuUser is another different nick",
                false,
            ),
            ("endWord", "and SojuUserSan is yet a different nick", false),
            (
                "underscore",
                "and SojuUser_san has nothing to do with me",
                false,
            ),
            (
                "zeroWidthSpace",
                "writing S\u{200B}ojuUser shouldn't trigger a highlight",
                false,
            ),
            ("url", "https://SojuUser.example", false),
            (
                "startURL",
                "https://SojuUser.example is a nice website",
                false,
            ),
            (
                "endURL",
                "check out my website: https://SojuUser.example",
                false,
            ),
            (
                "parenthesizedURL",
                "see my website (https://SojuUser.example)",
                false,
            ),
            (
                "afterURL",
                "see https://SojuUser.example (cc SojuUser)",
                true,
            ),
        ];
        for (name, text, hl) in cases {
            assert_eq!(is_highlight(text, nick), hl, "{name}");
        }
    }

    fn assert_split(input: &str, expected: &[&str]) {
        assert_eq!(split_words(input).unwrap(), expected, "{input:?}");
    }

    #[test]
    fn split() {
        assert_split(
            "  ch 'up' #soju    'relay'-det\"ache\"d  message  ",
            &["ch", "up", "#soju", "relay-detached", "message"],
        );
        assert_split(
            "net update \\\"free\\\"node -pass 'political \"stance\" desu!' -realname '' -nick lee",
            &[
                "net",
                "update",
                "\"free\"node",
                "-pass",
                "political \"stance\" desu!",
                "-realname",
                "",
                "-nick",
                "lee",
            ],
        );
        assert_split("Omedeto,\\ Yui! ''", &["Omedeto, Yui!", ""]);
        assert!(split_words("end of 'file").is_err());
        assert!(split_words("end of backquote \\").is_err());
    }

    #[test]
    fn user_modes() {
        let mut m = UserModes::default();
        m.apply("+iw-i+o").unwrap();
        assert_eq!(m.0, "wo");
        assert!(m.apply("x").is_err());
    }

    #[test]
    fn channel_modes() {
        let avail = std_memberships();
        let chmodes = std_channel_modes();
        let mut members = CaseMappingMap::new(xirc::CASEMAPPING_RFC1459);
        members.set("Alice", MembershipSet::default());
        let mut modes = ChannelModes::new();
        let args: Vec<String> = ["alice", "*!*@x", "key", "10"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        apply_channel_modes(
            Some(&mut modes),
            &mut members,
            &avail,
            &chmodes,
            "+obkln",
            &args,
        )
        .unwrap();
        assert_eq!(members.get("ALICE").unwrap().0, vec![avail[2]]);
        let (s, p) = format_channel_modes(&modes);
        assert_eq!(s, "+kln");
        assert_eq!(p, vec!["key", "10"]);
        apply_channel_modes(
            Some(&mut modes),
            &mut members,
            &avail,
            &chmodes,
            "-lk",
            &["x".into()],
        )
        .unwrap();
        assert_eq!(format_channel_modes(&modes).0, "+n");
        assert!(apply_channel_modes(None, &mut members, &avail, &chmodes, "+o", &[]).is_err());
    }

    #[test]
    fn member_prefix() {
        let avail = std_memberships();
        let ms = MembershipSet(vec![avail[2], avail[4]]);
        assert_eq!(format_member_prefix(&ms, true), "@+");
        assert_eq!(format_member_prefix(&ms, false), "@");
        assert_eq!(strip_member_prefixes("H@+", &avail), "H");
    }
}
