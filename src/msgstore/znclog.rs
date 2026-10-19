//! ZNC log line format, used by the fs message store.

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike, Utc};

use crate::database::{Network, User, get_nick};
use crate::irc::{Message, Prefix, Tags};
use crate::xirc;

const TIMESTAMP_PREFIX_LEN: usize = "[01:02:03] ".len();

/// Builds a local time from a date and a time of day. Ambiguous or
/// non-existent local times (DST transitions) resolve to the earliest valid
/// instant.
pub fn local_time(date: NaiveDate, hour: u32, min: u32, sec: u32) -> DateTime<Local> {
    let naive = date
        .and_hms_opt(hour, min, sec)
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap());
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => t,
        chrono::LocalResult::Ambiguous(a, _) => a,
        chrono::LocalResult::None => {
            // Skipped by a DST transition: move forward an hour
            let naive = naive + chrono::Duration::hours(1);
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap_or_else(|| Local.from_utc_datetime(&naive))
        }
    }
}

fn parse_timestamp(line: &str) -> Option<(u32, u32, u32)> {
    let b = line.as_bytes();
    if b.len() < TIMESTAMP_PREFIX_LEN
        || b[0] != b'['
        || b[3] != b':'
        || b[6] != b':'
        || b[9] != b']'
        || b[10] != b' '
    {
        return None;
    }
    let num = |i: usize| -> Option<u32> {
        if b[i].is_ascii_digit() && b[i + 1].is_ascii_digit() {
            Some(((b[i] - b'0') * 10 + (b[i + 1] - b'0')) as u32)
        } else {
            None
        }
    };
    Some((num(1)?, num(4)?, num(7)?))
}

fn trim_parens(s: &str) -> &str {
    let s = s.strip_prefix('(').unwrap_or(s);
    s.strip_suffix(')').unwrap_or(s)
}

/// Parses a log line. Returns None for lines which can't be turned into a
/// message (or are events when events is false).
pub fn unmarshal_line(
    line: &str,
    user: &User,
    network: &Network,
    entity: &str,
    ref_date: NaiveDate,
    events: bool,
) -> Result<Option<(Message, DateTime<Local>)>> {
    let Some((hour, minute, second)) = parse_timestamp(line) else {
        bail!("malformed timestamp prefix");
    };
    let line = &line[TIMESTAMP_PREFIX_LEN..];

    let cmd: &str;
    let prefix: Prefix;
    let params: Vec<String>;
    if let (true, Some(rest)) = (events, line.strip_prefix("*** ")) {
        let Some((kind, rest)) = rest.split_once(' ') else {
            return Ok(None);
        };
        match kind {
            "Joins:" | "Parts:" | "Quits:" => {
                let args: Vec<&str> = rest.splitn(3, ' ').collect();
                if args.len() < 2 {
                    return Ok(None);
                }
                let nick = args[0];
                let mask = trim_parens(args[1]);
                let Some((u, h)) = mask.split_once('@') else {
                    return Ok(None);
                };
                prefix = Prefix::full(nick, u, h);
                let reason = args.get(2).map(|r| trim_parens(r)).unwrap_or("");
                match kind {
                    "Joins:" => {
                        cmd = "JOIN";
                        params = vec![entity.to_owned()];
                    }
                    "Parts:" => {
                        cmd = "PART";
                        params = if reason.is_empty() {
                            vec![entity.to_owned()]
                        } else {
                            vec![entity.to_owned(), reason.to_owned()]
                        };
                    }
                    _ => {
                        cmd = "QUIT";
                        params = if reason.is_empty() {
                            vec![]
                        } else {
                            vec![reason.to_owned()]
                        };
                    }
                }
            }
            nick => {
                if let Some(r) = rest.strip_prefix("is now known as ") {
                    cmd = "NICK";
                    prefix = Prefix::new(nick);
                    params = vec![r.to_owned()];
                } else if let Some(r) = rest.strip_prefix("was kicked by ") {
                    let Some((by, reason)) = r.split_once(' ') else {
                        return Ok(None);
                    };
                    cmd = "KICK";
                    prefix = Prefix::new(by);
                    let reason = trim_parens(reason);
                    let mut p = vec![entity.to_owned(), nick.to_owned()];
                    if !reason.is_empty() {
                        p.push(reason.to_owned());
                    }
                    params = p;
                } else if let Some(r) = rest.strip_prefix("changes topic to ") {
                    cmd = "TOPIC";
                    prefix = Prefix::new(nick);
                    let topic = r.strip_prefix('\'').unwrap_or(r);
                    let topic = topic.strip_suffix('\'').unwrap_or(topic);
                    params = vec![entity.to_owned(), topic.to_owned()];
                } else if let Some(r) = rest.strip_prefix("sets mode: ") {
                    cmd = "MODE";
                    prefix = Prefix::new(nick);
                    let mut p = vec![entity.to_owned()];
                    p.extend(r.split(' ').map(str::to_owned));
                    params = p;
                } else {
                    return Ok(None);
                }
            }
        }
    } else {
        let (sender, text);
        if let Some(rest) = line.strip_prefix('<') {
            cmd = "PRIVMSG";
            let Some((s, t)) = rest.split_once("> ") else {
                return Ok(None);
            };
            sender = s;
            text = t.to_owned();
        } else if let Some(rest) = line.strip_prefix('-') {
            cmd = "NOTICE";
            let Some((s, t)) = rest.split_once("- ") else {
                return Ok(None);
            };
            sender = s;
            text = t.to_owned();
        } else if let Some(rest) = line.strip_prefix("* ") {
            cmd = "PRIVMSG";
            let Some((s, t)) = rest.split_once(' ') else {
                return Ok(None);
            };
            sender = s;
            text = format!("\x01ACTION {t}\x01");
        } else {
            return Ok(None);
        }

        prefix = Prefix::new(sender);
        let target = if entity == sender {
            // This is a direct message from a user to us. We don't store our
            // own nickname in the logs, so grab it from the network
            // settings. Not very accurate since this may not match our nick
            // at the time the message was received, but we can't do a lot
            // better.
            get_nick(user, Some(network))
        } else {
            entity
        };
        params = vec![target.to_owned(), text];
    }

    let t = local_time(ref_date, hour, minute, second);
    let msg = Message {
        tags: Tags::from_iter([("time", xirc::format_server_time(t.with_timezone(&Utc)))]),
        prefix: Some(prefix),
        command: cmd.to_owned(),
        params,
    };
    Ok(Some((msg, t)))
}

/// Formats a message as a log line. Returns an empty string if the message
/// isn't logged.
pub fn marshal_line(msg: &Message, t: DateTime<Local>) -> String {
    let s = format_message(msg);
    if s.is_empty() {
        return s;
    }
    format!(
        "[{:02}:{:02}:{:02}] {}",
        t.hour(),
        t.minute(),
        t.second(),
        s
    )
}

/// Formats a message log line. It assumes a well-formed IRC message.
fn format_message(msg: &Message) -> String {
    let p = msg.prefix.clone().unwrap_or_default();
    let param = |i: usize| msg.params.get(i).map(String::as_str).unwrap_or("");
    match msg.command.to_uppercase().as_str() {
        "NICK" => format!("*** {} is now known as {}", p.name, param(0)),
        "JOIN" => format!("*** Joins: {} ({}@{})", p.name, p.user, p.host),
        "PART" => format!(
            "*** Parts: {} ({}@{}) ({})",
            p.name,
            p.user,
            p.host,
            param(1)
        ),
        "KICK" => format!("*** {} was kicked by {} ({})", param(1), p.name, param(2)),
        "QUIT" => format!(
            "*** Quits: {} ({}@{}) ({})",
            p.name,
            p.user,
            p.host,
            param(0)
        ),
        "TOPIC" => format!("*** {} changes topic to '{}'", p.name, param(1)),
        "MODE" => format!(
            "*** {} sets mode: {}",
            p.name,
            msg.params.get(1..).map(|s| s.join(" ")).unwrap_or_default()
        ),
        "NOTICE" => format!("-{}- {}", p.name, param(1)),
        "PRIVMSG" => match xirc::parse_ctcp_message(msg) {
            Some((cmd, params)) if cmd == "ACTION" => format!("* {} {}", p.name, params),
            _ => format!("<{}> {}", p.name, param(1)),
        },
        _ => String::new(),
    }
}

/// Returns the local date of a time.
pub fn local_date(t: DateTime<Utc>) -> NaiveDate {
    let l = t.with_timezone(&Local);
    NaiveDate::from_ymd_opt(l.year(), l.month(), l.day()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;

    fn roundtrip(m: Message, events: bool) -> Option<Message> {
        let date = NaiveDate::from_ymd_opt(2024, 5, 6).unwrap();
        let t = local_time(date, 12, 34, 56);
        let line = marshal_line(&m, t);
        let user = User::new("me");
        let net = Network::new("irc.example.org");
        let (got, gt) = unmarshal_line(&line, &user, &net, "#chan", date, events)
            .unwrap()
            .map(|(m, t)| (Some(m), Some(t)))
            .unwrap_or((None, None));
        if let Some(gt) = gt {
            assert_eq!(gt, t);
        }
        got.map(|mut g| {
            g.tags.clear();
            g
        })
    }

    #[test]
    fn lines() {
        let p = Prefix::full("bob", "u", "h");
        let m = msg!("PRIVMSG", "#chan", "hello there").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), false), Some(m));
        let m = msg!("NOTICE", "#chan", "hi").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), false), Some(m));
        let m = msg!("PRIVMSG", "#chan", "\x01ACTION waves\x01").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), false), Some(m));
        let m = msg!("JOIN", "#chan").with_prefix(p.clone());
        assert_eq!(roundtrip(m.clone(), true), Some(m.clone()));
        assert_eq!(roundtrip(m, false), None);
        let m = msg!("PART", "#chan", "bye now").with_prefix(p.clone());
        assert_eq!(roundtrip(m.clone(), true), Some(m));
        let m = msg!("QUIT", "gone").with_prefix(p.clone());
        assert_eq!(roundtrip(m.clone(), true), Some(m));
        let m = msg!("KICK", "#chan", "carol", "spam").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), true), Some(m));
        let m = msg!("NICK", "robert").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), true), Some(m));
        let m = msg!("TOPIC", "#chan", "new topic").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), true), Some(m));
        let m = msg!("MODE", "#chan", "+o", "carol").with_prefix(Prefix::new("bob"));
        assert_eq!(roundtrip(m.clone(), true), Some(m));
        assert_eq!(marshal_line(&msg!("TAGMSG", "#chan"), Local::now()), "");
    }

    #[test]
    fn direct_message() {
        let date = NaiveDate::from_ymd_opt(2024, 5, 6).unwrap();
        let mut user = User::new("me");
        user.nick = "menick".into();
        let net = Network::new("irc.example.org");
        let (m, _) = unmarshal_line("[01:02:03] <bob> hi", &user, &net, "bob", date, false)
            .unwrap()
            .unwrap();
        assert_eq!(m.params, vec!["menick", "hi"]);
        assert!(unmarshal_line("garbage", &user, &net, "bob", date, false).is_err());
        assert!(
            unmarshal_line("[01:02:03] ???", &user, &net, "bob", date, false)
                .unwrap()
                .is_none()
        );
    }
}
