//! sojuctl sends a BouncerServ command to a running soju instance via its
//! admin socket.

use std::process::ExitCode;

use soju::config;
use soju::irc::Message;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const USAGE: &str = "usage: sojuctl [-config path] <command>\n       sojuctl -config path turso create-admin -username name -password password\n";

/// Explicit offline bootstrap. The regular commands always use the running
/// server, so they cannot leave its in-memory user directory out of sync.
fn create_turso_admin(cfg: &config::Server, args: &[String]) -> anyhow::Result<()> {
    check_words(args).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        cfg.turso.is_some(),
        "turso create-admin requires message-store turso"
    );
    let flags = soju::flags::FlagSet::new()
        .string("username")
        .string("password");
    let parsed = flags.parse(args)?;
    anyhow::ensure!(parsed.args.is_empty(), "unexpected positional arguments");
    let username = parsed.string_or("username", "");
    let password = parsed.string_or("password", "");
    anyhow::ensure!(!username.is_empty(), "flag -username is required");
    anyhow::ensure!(!password.is_empty(), "flag -password is required");
    let mut user = soju::database::User::new(&username);
    user.set_password(&password)?;
    user.admin = true;
    let db = soju::database::Database::open_config(cfg)?;
    db.store_user(&mut user)?;
    println!(
        "created admin {username:?} and their Turso database; start or restart soju to load the user"
    );
    Ok(())
}

/// Quotes a word for split_words, which only understands backslash escapes
/// of the next character: other characters are kept as-is. CR, LF and NUL
/// can't be sent in an IRC line, see check_words.
fn quote(word: &str) -> String {
    let mut s = String::from("\"");
    for c in word.chars() {
        if c == '"' || c == '\\' {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

/// Rejects words which can't be sent to the server unchanged.
fn check_words(words: &[String]) -> Result<(), String> {
    if words.iter().any(|w| w.contains(['\r', '\n', '\0'])) {
        return Err("arguments can't contain CR, LF or NUL characters".to_owned());
    }
    Ok(())
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
    check_words(words)?;
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
    let result = if args.first().map(String::as_str) == Some("turso")
        && args.get(1).map(String::as_str) == Some("create-admin")
    {
        create_turso_admin(&cfg, &args[2..]).map_err(|e| format!("{e:#}"))
    } else {
        rt.block_on(run(&cfg, &args))
    };
    match result {
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
    fn turso_bootstrap_validates_before_connecting() {
        let mut cfg = config::Server::defaults();
        assert!(
            create_turso_admin(&cfg, &[])
                .unwrap_err()
                .to_string()
                .contains("message-store turso")
        );
        cfg.turso = Some(config::Turso {
            organization: "example".into(),
            group: "bouncer".into(),
            controller: "soju-controller".into(),
            api_token: "api-secret".into(),
            group_token: "sql-secret".into(),
        });
        assert!(
            create_turso_admin(&cfg, &[])
                .unwrap_err()
                .to_string()
                .contains("-username")
        );
        let args = vec!["-username".to_owned(), "alice".to_owned()];
        assert!(
            create_turso_admin(&cfg, &args)
                .unwrap_err()
                .to_string()
                .contains("-password")
        );
        let args = vec![
            "-username".to_owned(),
            "alice\r\nJOIN #channel".to_owned(),
            "-password".to_owned(),
            "secret".to_owned(),
        ];
        assert!(
            create_turso_admin(&cfg, &args)
                .unwrap_err()
                .to_string()
                .contains("CR, LF or NUL")
        );
    }

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

    #[test]
    fn quoting_roundtrip() {
        for word in [
            "a\tb",
            "\x07\x1b[1m",
            "'single' \"double\"",
            "back\\slash\\",
            "",
            "é ✓",
        ] {
            let words = vec![word.to_owned()];
            assert!(check_words(&words).is_ok());
            let line = quote_words(&words);
            // The line survives IRC framing
            let m = Message::new("BOUNCERSERV", vec![line.clone()]);
            assert_eq!(Message::parse(&m.to_string()).unwrap().params[0], line);
            assert_eq!(
                soju::ircutil::split_words(&line).unwrap(),
                words,
                "{word:?}"
            );
        }
        assert!(check_words(&["a\nb".to_owned()]).is_err());
    }
}
