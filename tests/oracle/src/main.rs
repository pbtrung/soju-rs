//! Request: "<op>\t<hex input>\n". Response: "<hex output>\n". Every op
//! formats its result as text; fuzz/oracle_ops.c formats the C result the
//! same way.

use std::io::{BufRead, Write};

use base64::Engine;
use soju::irc::Message;
use soju::xirc;

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Splits at the first NUL byte.
fn two(input: &str) -> (&str, &str) {
    input.split_once('\0').unwrap_or((input, ""))
}

fn scfg_dump(b: &[soju::scfg::Directive], out: &mut String, depth: usize) {
    for d in b {
        out.push_str(&format!("{}{} {:?} @{}", " ".repeat(depth), d.name, d.params, d.lineno));
        match &d.children {
            Some(c) => {
                out.push_str(" {\n");
                scfg_dump(c, out, depth + 1);
                out.push_str(&format!("{}}}\n", " ".repeat(depth)));
            }
            None => out.push('\n'),
        }
    }
}

fn run(op: &str, raw: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(raw);
    let s: &str = &lossy;
    match op {
        "irc" => match Message::parse(s) {
            Ok(m) => format!("ok:{m}"),
            Err(e) => format!("err:{e}"),
        },
        "time" => match xirc::parse_server_time(s) {
            Some(t) => format!(
                "some:{}:{}.{}",
                xirc::format_server_time(t),
                t.timestamp(),
                t.timestamp_subsec_nanos()
            ),
            None => "none".into(),
        },
        "lossy" => lossy.into_owned(),
        "lower" => s.to_lowercase(),
        "upper" => s.to_uppercase(),
        "debug" => format!("{s:?}"),
        "scfg" => match soju::scfg::parse(s) {
            Ok(b) => {
                let mut out = String::from("ok:\n");
                scfg_dump(&b, &mut out, 0);
                out
            }
            Err(e) => format!("err:{e}"),
        },
        "config" => match soju::config::parse(s) {
            Ok(c) => {
                let b = &c.basic;
                let mut out = String::from("ok:\n");
                out.push_str(&format!("listen {:?}\n", c.listen));
                out.push_str(&format!("title {:?}\n", b.title));
                out.push_str(&format!("store {:?} {:?}\n", b.msg_store.driver, b.msg_store.source));
                out.push_str(&format!("origins {:?}\n", b.http_origins));
                out.push_str(&format!("proxy {:?} {}\n", b.accept_proxy_ips.0.iter().map(|n| n.to_string()).collect::<Vec<_>>(), b.accept_proxy_unix));
                out.push_str(&format!("maxnet {}\n", b.max_user_networks));
                out.push_str(&format!("upips {:?}\n", b.upstream_user_ips.iter().map(|n| n.to_string()).collect::<Vec<_>>()));
                out.push_str(&format!("durations {} {}\n", b.disable_inactive_users_delay.as_nanos(), b.message_expiry.as_nanos()));
                out.push_str(&format!("limit {} {} {}\n", b.chat_history_limit, b.enable_users_on_auth, b.client_cert_auth));
                out.push_str(&format!("tls {:?}\n", c.tls.as_ref().map(|t| (&t.cert_path, &t.key_path))));
                out.push_str(&format!("misc {:?} {:?} {:?} {:?} {}\n", c.icon, c.motd_path, c.db.driver, c.db.source, c.file_upload));
                out.push_str(&format!("auth {:?}\n", c.auth.iter().map(|a| (format!("{:?}", a.driver), &a.source)).collect::<Vec<_>>()));
                if let Some(t) = &c.turso {
                    out.push_str(&format!("turso {:?} {:?} {:?} {:?} {:?}\n", t.organization, t.group, t.controller, t.group_token, t.api_token));
                }
                out
            }
            Err(e) => format!("err:{e}"),
        },
        "casemap" => format!(
            "{}\n{}\n{}",
            xirc::casemap_ascii(s),
            xirc::casemap_rfc1459(s),
            xirc::casemap_rfc1459_strict(s)
        ),
        "pathmatch" => {
            let (p, n) = two(s);
            match soju::util::path_match(p, n) {
                Ok(b) => b.to_string(),
                Err(_) => "err".into(),
            }
        }
        "split_words" => match soju::ircutil::split_words(s) {
            Ok(w) => format!("ok:{w:?}"),
            Err(e) => format!("err:{e}"),
        },
        "url" => match soju::database::parse_url(s) {
            Ok(u) => format!("ok:{:?}|{:?}|{:?}|{}|{:?}|{:?}", u.scheme, u.host, u.path, u.has_user, u.query, u.fragment),
            Err(e) => format!("err:{e}"),
        },
        "hostport" => match soju::database::split_host_port(s) {
            Ok((h, p)) => format!("ok:{h:?} {p:?}"),
            Err(e) => format!("err:{e}"),
        },
        "highlight" => {
            let (t, n) = two(s);
            if n.is_empty() {
                "skip".into()
            } else {
                soju::ircutil::is_highlight(t, n).to_string()
            }
        }
        "listen" => match soju::listen::parse_listen_uri(s) {
            Ok(l) => format!("ok:{:?} {:?} {}", l.kind, l.addr, l.tls),
            Err(e) => format!("err:{e}"),
        },
        "msgid" => match soju::msgstore::parse_msg_id(s) {
            Ok((n, t)) => format!("ok:{n} {t:?}"),
            Err(e) => format!("err:{e}"),
        },
        "strip" => soju::database::strip_ansi(s),
        "b64std" => match base64::engine::general_purpose::STANDARD.decode(raw) {
            Ok(b) => format!("ok:{}", hex(&b)),
            Err(e) => format!("err:{e}"),
        },
        "b64url" => match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw) {
            Ok(b) => format!("ok:{}", hex(&b)),
            Err(e) => format!("err:{e}"),
        },
        "ipaddr" => match s.parse::<std::net::IpAddr>() {
            Ok(a) => format!("ok:{a} {}", a.to_canonical()),
            Err(_) => "err".into(),
        },
        "sockaddr" => match s.parse::<std::net::SocketAddr>() {
            Ok(a) => format!("ok:{} {}", a.ip(), a.port()),
            Err(_) => "err".into(),
        },
        "ipnet" => match s.parse::<ipnet::IpNet>() {
            Ok(n) => format!("ok:{}", n.trunc()),
            Err(_) => "err".into(),
        },
        "mlimits" => xirc::MultilineLimits::parse(s).cap_value(),
        "whox" => {
            let (f, t) = xirc::parse_whox_options(s);
            format!("{f:?} {t:?}")
        }
        _ => panic!("unknown op {op}"),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout());
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let (op, h) = line.split_once('\t').unwrap_or((&line, ""));
        let res = run(op, &unhex(h));
        writeln!(out, "{}", hex(res.as_bytes())).unwrap();
        out.flush().unwrap();
    }
}
