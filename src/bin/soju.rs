//! soju, a user-friendly IRC bouncer.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use soju::config;
use soju::database::Database;
use soju::listen::{self, Kind, TlsState};
use soju::logger::Logger;
use soju::server::{self, Server};
use soju::{logf, tls};

const USAGE: &str = "Usage of soju:
  -config string
    \tpath to configuration file
  -debug
    \tenable debug logging
  -listen value
    \tlistening address
";

struct Args {
    config_path: String,
    debug: bool,
    listen: Vec<String>,
}

/// Parses flags like Go's flag package: -flag value, -flag=value and
/// --flag forms.
fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config_path: config::DEFAULT_PATH.to_owned(),
        debug: false,
        listen: Vec::new(),
    };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        if arg == "--" {
            break;
        }
        let name = arg.trim_start_matches('-');
        let (name, value) = match name.split_once('=') {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (name.to_owned(), None),
        };
        let mut take_value = || -> Result<String, String> {
            match &value {
                Some(v) => Ok(v.clone()),
                None => it
                    .next()
                    .ok_or_else(|| format!("flag needs an argument: -{name}")),
            }
        };
        match name.as_str() {
            "config" => args.config_path = take_value()?,
            "listen" => args.listen.push(take_value()?),
            "debug" => {
                args.debug = match value.as_deref() {
                    None | Some("1" | "t" | "T" | "TRUE" | "true" | "True") => true,
                    Some("0" | "f" | "F" | "FALSE" | "false" | "False") => false,
                    Some(v) => {
                        return Err(format!(
                            "invalid boolean value {v:?} for -debug: parse error"
                        ));
                    }
                };
            }
            "h" | "help" => return Err(String::new()),
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }
    Ok(args)
}

fn load_config(path: &str) -> Result<config::Server> {
    if path.is_empty() {
        return Ok(config::Server::defaults());
    }
    config::load(path).map_err(|e| anyhow!("failed to load config file: {e}"))
}

fn fatal(logger: &Logger, msg: impl std::fmt::Display) -> ExitCode {
    logf!(logger, "{msg}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the async runtime");
    rt.block_on(run(args))
}

async fn run(args: Args) -> ExitCode {
    let logger = Logger::new(args.debug);
    logf!(logger, "starting soju-rs v{}", soju::VERSION);

    let mut raw = match load_config(&args.config_path) {
        Ok(c) => c,
        Err(e) => return fatal(&logger, e),
    };
    let server_cfg = match server::load_server_config(&raw).await {
        Ok(c) => c,
        Err(e) => return fatal(&logger, e),
    };
    if raw.file_upload {
        logf!(
            logger,
            "warning: file uploads are not supported, ignoring the file-upload directive"
        );
    }

    let certs = Arc::new(tls::CertStore::default());
    if let Some(t) = &raw.tls {
        match tls::load_certified_key(&t.cert_path, &t.key_path) {
            Ok(k) => certs.set(k),
            Err(e) => {
                return fatal(
                    &logger,
                    format!("failed to load TLS certificate and key: {e}"),
                );
            }
        }
    }

    raw.listen.extend(args.listen.iter().cloned());
    if raw.listen.is_empty() {
        raw.listen.push(":6697".to_owned());
    }

    let db = match Database::open(&raw.db.source) {
        Ok(db) => db,
        Err(e) => return fatal(&logger, format!("failed to open database: {e}")),
    };

    let srv = Server::new(db, logger.clone());
    srv.set_config(server_cfg);

    let tls_state = raw.tls.as_ref().map(|_| TlsState {
        certs: certs.clone(),
    });
    for l in &raw.listen {
        let listener = match listen::parse_listen_uri(l) {
            Ok(l) => l,
            Err(e) => return fatal(&logger, format!("failed to parse listen URI {l:?}: {e}")),
        };
        if listener.kind == Kind::Ident && srv.identd().is_none() {
            srv.set_identd(Arc::new(soju::identd::Identd::new()));
        }
        if let Err(e) = listen::spawn_listener(srv.clone(), listener, tls_state.clone()).await {
            return fatal(&logger, e);
        }
        logf!(logger, "server listening on {l:?}");
    }

    if let Err(e) = srv.start() {
        return fatal(&logger, e);
    }

    wait_for_signals(&srv, &args, &certs).await;
    srv.shutdown().await;
    ExitCode::SUCCESS
}

#[cfg(unix)]
async fn wait_for_signals(srv: &Arc<Server>, args: &Args, certs: &Arc<tls::CertStore>) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");
    let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    loop {
        tokio::select! {
            _ = hup.recv() => {
                logf!(srv.logger, "reloading configuration");
                match reload(args, certs).await {
                    Ok(cfg) => srv.set_config(cfg),
                    Err(e) => logf!(srv.logger, "failed to reloading configuration: {e}"),
                }
            }
            _ = term.recv() => return,
            _ = int.recv() => return,
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signals(_srv: &Arc<Server>, _args: &Args, _certs: &Arc<tls::CertStore>) {
    let _ = tokio::signal::ctrl_c().await;
}

/// Reloads the configuration file, the TLS certificate and the MOTD.
async fn reload(args: &Args, certs: &Arc<tls::CertStore>) -> Result<server::ServerConfig> {
    let raw = load_config(&args.config_path)?;
    let cfg = server::load_server_config(&raw).await?;
    if let Some(t) = &raw.tls {
        let key = tls::load_certified_key(&t.cert_path, &t.key_path)
            .map_err(|e| anyhow!("failed to load TLS certificate and key: {e}"))?;
        certs.set(key);
    }
    Ok(cfg)
}
