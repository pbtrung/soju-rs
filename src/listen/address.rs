//! Listener kinds, addresses and configuration URI parsing.

use crate::database::split_host_port;
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// IRC over a stream (TCP, TLS, Unix socket)
    Irc,
    /// HTTP with /socket (WebSocket) and /icon
    Http,
    Ident,
    Prometheus,
    Admin,
}

/// Where to listen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    Tcp(String),
    Unix(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub label: String,
    pub kind: Kind,
    pub addr: Address,
    pub tls: bool,
}

fn with_default_port(addr: &str, port: &str) -> String {
    if split_host_port(addr).is_ok() {
        addr.to_owned()
    } else {
        format!("{addr}:{port}")
    }
}

/// Parses a listen URI.
pub fn parse_listen_uri(listen: &str) -> Result<Listener> {
    let uri = if listen.contains(":/") {
        listen.to_owned()
    } else {
        // This is a raw domain name, make it an URL with an empty scheme
        format!("//{listen}")
    };
    let (scheme, rest) = match uri.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r.to_owned()),
        None => (String::new(), uri.trim_start_matches("//").to_owned()),
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_owned(), rest[i..].to_owned()),
        None => (rest.clone(), String::new()),
    };
    let hostname = || -> String {
        match split_host_port(&host) {
            Ok((h, _)) => h,
            Err(_) => host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
        }
    };
    let mk = |kind, addr, tls| Listener {
        label: listen.to_owned(),
        kind,
        addr,
        tls,
    };
    let unix_path = format!("{host}{path}");

    Ok(match scheme.as_str() {
        "ircs" | "" => mk(
            Kind::Irc,
            Address::Tcp(with_default_port(&host, "6697")),
            true,
        ),
        "irc" | "irc+insecure" => {
            if scheme == "irc" && hostname() != "localhost" {
                bail!("Plain-text IRC listening host must be localhost unless marked as insecure");
            }
            mk(
                Kind::Irc,
                Address::Tcp(with_default_port(&host, "6667")),
                false,
            )
        }
        "unix" => mk(Kind::Irc, Address::Unix(unix_path), false),
        "unix+admin" => {
            let path = if unix_path.is_empty() {
                crate::config::DEFAULT_UNIX_ADMIN_PATH.to_owned()
            } else {
                unix_path
            };
            mk(Kind::Admin, Address::Unix(path), false)
        }
        "wss" | "https" => mk(
            Kind::Http,
            Address::Tcp(with_default_port(&host, "443")),
            true,
        ),
        "ws" | "ws+insecure" | "http" | "http+insecure" => {
            if (scheme == "ws" || scheme == "http") && hostname() != "localhost" {
                bail!(
                    "Plain-text {} listening host must be localhost unless marked as insecure",
                    if scheme == "ws" { "WebSocket" } else { "HTTP" }
                );
            }
            mk(
                Kind::Http,
                Address::Tcp(with_default_port(&host, "80")),
                false,
            )
        }
        "ws+unix" | "http+unix" => mk(Kind::Http, Address::Unix(unix_path), false),
        "ident" => mk(
            Kind::Ident,
            Address::Tcp(with_default_port(&host, "113")),
            false,
        ),
        "http+prometheus" | "http+insecure+prometheus" => {
            if split_host_port(&host).is_err() {
                bail!("invalid host in URI {listen:?}: missing port in address");
            }
            if scheme == "http+prometheus" && hostname() != "localhost" {
                bail!("Prometheus listening host must be localhost unless marked as insecure");
            }
            mk(Kind::Prometheus, Address::Tcp(host.clone()), false)
        }
        "http+pprof" => bail!("failed to listen on {listen:?}: pprof is not supported"),
        _ => bail!("failed to listen on {listen:?}: unsupported scheme"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uris() {
        let l = parse_listen_uri("ircs://").unwrap();
        assert_eq!(
            (l.kind, l.addr, l.tls),
            (Kind::Irc, Address::Tcp(":6697".into()), true)
        );
        let l = parse_listen_uri(":6697").unwrap();
        assert_eq!(l.addr, Address::Tcp(":6697".into()));
        let l = parse_listen_uri("irc+insecure://0.0.0.0:6667").unwrap();
        assert_eq!((l.kind, l.tls), (Kind::Irc, false));
        assert!(parse_listen_uri("irc://0.0.0.0").is_err());
        assert!(parse_listen_uri("irc://localhost").is_ok());
        let l = parse_listen_uri("unix:///run/soju/irc.sock").unwrap();
        assert_eq!(l.addr, Address::Unix("/run/soju/irc.sock".into()));
        let l = parse_listen_uri("unix+admin://").unwrap();
        assert_eq!(
            l.addr,
            Address::Unix(crate::config::DEFAULT_UNIX_ADMIN_PATH.into())
        );
        let l = parse_listen_uri("ws+insecure://:8080").unwrap();
        assert_eq!(
            (l.kind, l.addr, l.tls),
            (Kind::Http, Address::Tcp(":8080".into()), false)
        );
        let l = parse_listen_uri("wss://").unwrap();
        assert_eq!(l.addr, Address::Tcp(":443".into()));
        assert!(parse_listen_uri("ws://example.org").is_err());
        assert!(parse_listen_uri("http+prometheus://localhost:9090").is_ok());
        assert!(parse_listen_uri("http+prometheus://0.0.0.0:9090").is_err());
        assert!(parse_listen_uri("http+prometheus://localhost").is_err());
        assert!(parse_listen_uri("gopher://").is_err());
        let l = parse_listen_uri("ident://").unwrap();
        assert_eq!(l.addr, Address::Tcp(":113".into()));
    }
}
