//! sojuctl sends a BouncerServ command to a running soju instance via its
//! admin socket.

use std::process::ExitCode;

use soju::config;
use soju::irc::Message;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const USAGE: &str = "usage: sojuctl [-config path] <command>\n";

/// Quotes a word like Go's strconv.Quote.
fn quote(word: &str) -> String {
    let mut s = String::from("\"");
    for c in word.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            '\x07' => s.push_str("\\a"),
            '\x08' => s.push_str("\\b"),
            '\x0c' => s.push_str("\\f"),
            '\x0b' => s.push_str("\\v"),
            c if c.is_control() => {
                let n = c as u32;
                if n < 0x10000 {
                    s.push_str(&format!("\\u{n:04x}"));
                } else {
                    s.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

fn quote_words(words: &[String]) -> String {
    words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}

/// The admin socket path, parsed like the server parses listen directives.
fn admin_socket_path(cfg: &config::Server) -> Option<String> {
    cfg.listen.iter().find_map(
        |listen| match soju::listen::parse_listen_uri(listen).ok()? {
            soju::listen::Listener {
                kind: soju::listen::Kind::Admin,
                addr: soju::listen::Address::Unix(path),
                ..
            } => Some(path),
            _ => None,
        },
    )
}

#[cfg(unix)]
async fn run(cfg: &config::Server, words: &[String]) -> Result<(), String> {
    let Some(path) = admin_socket_path(cfg) else {
        return Err("no listen unix+admin directive found in config".to_owned());
    };
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .map_err(|e| format!("dial {path}: {e}"))?;
    let (r, mut w) = stream.into_split();
    let m = Message::new("BOUNCERSERV", vec![quote_words(words)]);
    w.write_all(format!("{m}\r\n").as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut r = BufReader::new(r);
    loop {
        let mut line = String::new();
        let n = r
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("read: EOF".to_owned());
        }
        let Ok(m) = Message::parse(&line) else {
            continue;
        };
        match m.command.as_str() {
            "PRIVMSG" => println!("{}", m.trailing()),
            "BOUNCERSERV" if m.param(0) == "OK" => return Ok(()),
            _ => return Err(m.trailing().to_owned()),
        }
    }
}

#[cfg(not(unix))]
async fn run(_cfg: &config::Server, _words: &[String]) -> Result<(), String> {
    Err("Unix sockets are not supported on this platform".to_owned())
}

fn main() -> ExitCode {
    let mut config_path = config::DEFAULT_PATH.to_owned();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    while let Some(a) = args.first().cloned() {
        if a == "--" {
            args.remove(0);
            break;
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        args.remove(0);
        let name = a.trim_start_matches('-');
        match name.split_once('=') {
            Some(("config", v)) => config_path = v.to_owned(),
            None if name == "config" => {
                if args.is_empty() {
                    eprintln!("flag needs an argument: -config");
                    eprint!("{USAGE}");
                    return ExitCode::from(2);
                }
                config_path = args.remove(0);
            }
            None if name == "h" || name == "help" => {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
            _ => {
                eprintln!("flag provided but not defined: -{name}");
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let cfg = if config_path.is_empty() {
        config::Server::defaults()
    } else {
        match config::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to load config file: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to start the async runtime");
    match rt.block_on(run(&cfg, &args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let words: Vec<String> = ["user", "create", "-password", "a \"b\"\\"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            quote_words(&words),
            r#""user" "create" "-password" "a \"b\"\\""#
        );
        let parsed = soju::ircutil::split_words(&quote_words(&words)).unwrap();
        assert_eq!(parsed, words);
    }
}
