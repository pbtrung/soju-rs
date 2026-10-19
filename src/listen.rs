//! Listeners: IRC (TCP, TLS, Unix), HTTP/WebSocket, ident, Prometheus and
//! the admin socket.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::conn::{BoxIo, Conn, ConnInfo};
use crate::database::{join_host_port, parse_url, split_host_port};
use crate::server::Server;
use crate::{logf, proxyproto};

/// TCP keep-alive interval for downstream TCP connections.
const DOWNSTREAM_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(3600);

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

/// Shared TLS state for listeners.
#[derive(Clone)]
pub struct TlsState {
    pub certs: Arc<crate::tls::CertStore>,
}

/// An accepted stream with its addresses.
struct Accepted {
    io: BoxIo,
    remote: Option<SocketAddr>,
    unix: bool,
}

enum BoundListener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

impl BoundListener {
    async fn accept(&self) -> std::io::Result<Accepted> {
        match self {
            BoundListener::Tcp(l) => {
                let (s, addr) = l.accept().await?;
                let _ = set_keepalive(&s);
                Ok(Accepted {
                    io: Box::new(s),
                    remote: Some(addr),
                    unix: false,
                })
            }
            #[cfg(unix)]
            BoundListener::Unix(l) => {
                let (s, _) = l.accept().await?;
                Ok(Accepted {
                    io: Box::new(s),
                    remote: None,
                    unix: true,
                })
            }
        }
    }
}

fn set_keepalive(s: &tokio::net::TcpStream) -> std::io::Result<()> {
    let sock = socket2::SockRef::from(s);
    sock.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(DOWNSTREAM_KEEP_ALIVE))
}

async fn bind(addr: &Address) -> Result<BoundListener> {
    match addr {
        Address::Tcp(a) => {
            let a = a
                .strip_prefix(':')
                .map(|p| format!("[::]:{p}"))
                .unwrap_or_else(|| a.clone());
            Ok(BoundListener::Tcp(tokio::net::TcpListener::bind(&a).await?))
        }
        #[cfg(unix)]
        Address::Unix(path) => {
            // Remove stale socket files left by a previous instance
            if let Ok(md) = std::fs::symlink_metadata(path) {
                use std::os::unix::fs::FileTypeExt;
                if md.file_type().is_socket() {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(BoundListener::Unix(tokio::net::UnixListener::bind(path)?))
        }
        #[cfg(not(unix))]
        Address::Unix(_) => bail!("Unix sockets are not supported on this platform"),
    }
}

fn fmt_addr(a: SocketAddr) -> String {
    join_host_port(&a.ip().to_canonical().to_string(), &a.port().to_string())
}

/// Binds a listener and spawns its accept loop. Returns the bound local
/// address for TCP listeners.
pub async fn spawn_listener(
    srv: Arc<Server>,
    l: Listener,
    tls: Option<TlsState>,
) -> Result<Option<SocketAddr>> {
    let bound = bind(&l.addr)
        .await
        .map_err(|e| anyhow!("failed to start listener on {:?}: {e}", l.label))?;
    #[cfg(unix)]
    if let Address::Unix(path) = &l.addr {
        use std::os::unix::fs::PermissionsExt;
        let mode = if l.kind == Kind::Admin { 0o600 } else { 0o775 };
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            if l.kind == Kind::Admin {
                bail!("failed to chmod Unix admin socket: {e}");
            }
            logf!(srv.logger, "failed to chmod Unix socket: {e}");
        }
    }
    let local = match &bound {
        BoundListener::Tcp(t) => t.local_addr().ok(),
        #[cfg(unix)]
        BoundListener::Unix(_) => None,
    };

    let tls_config = match (&tls, l.tls) {
        (Some(t), true) => {
            let alpn: &[&str] = if l.kind == Kind::Http {
                &["http/1.1"]
            } else {
                &["irc"]
            };
            Some(crate::tls::server_config(
                t.certs.clone(),
                srv.client_cert_auth.clone(),
                alpn,
            )?)
        }
        (None, true) => bail!(
            "failed to listen on {:?}: missing TLS configuration",
            l.label
        ),
        _ => None,
    };
    let acceptor = tls_config.map(tokio_rustls::TlsAcceptor::from);

    let logger = srv.logger.with_prefix(format!("listener {:?}: ", l.label));
    let stop = srv.stopped();
    tokio::spawn(async move {
        let mut delay = std::time::Duration::ZERO;
        loop {
            let accepted = tokio::select! {
                a = bound.accept() => a,
                _ = stop.cancelled() => return,
            };
            let a = match accepted {
                Ok(a) => {
                    delay = std::time::Duration::ZERO;
                    a
                }
                Err(e) => {
                    // Temporary errors (e.g. too many open files): retry
                    // with an increasing delay
                    delay = if delay.is_zero() {
                        std::time::Duration::from_millis(5)
                    } else {
                        (delay * 2).min(std::time::Duration::from_secs(1))
                    };
                    logf!(logger, "accept error (retrying in {delay:?}): {e}");
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            let srv = srv.clone();
            let acceptor = acceptor.clone();
            let kind = l.kind;
            tokio::spawn(async move {
                handle_accepted(srv, kind, a, acceptor).await;
            });
        }
    });
    Ok(local)
}

/// Whether PROXY headers / forwarding headers from a peer are trusted.
fn accept_proxy(srv: &Server, remote: Option<SocketAddr>, unix: bool) -> bool {
    let cfg = srv.config();
    match remote {
        Some(a) => cfg.basic.accept_proxy_ips.contains(a.ip()),
        None => unix && cfg.basic.accept_proxy_unix,
    }
}

async fn handle_accepted(
    srv: Arc<Server>,
    kind: Kind,
    a: Accepted,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
) {
    let mut peer_cert = None;
    let io: BoxIo = match acceptor {
        Some(acceptor) => {
            let res = tokio::time::timeout(
                crate::server::DOWNSTREAM_REGISTER_TIMEOUT,
                acceptor.accept(a.io),
            )
            .await;
            match res {
                Ok(Ok(tls)) => {
                    peer_cert = tls
                        .get_ref()
                        .1
                        .peer_certificates()
                        .and_then(|c| c.first())
                        .map(|c| c.as_ref().to_vec());
                    Box::new(tls)
                }
                _ => return,
            }
        }
        None => a.io,
    };

    if kind == Kind::Http || kind == Kind::Prometheus {
        serve_http(srv, kind, io, a.remote, a.unix, peer_cert).await;
        return;
    }

    // Read an optional PROXY header. Headers from untrusted peers are
    // consumed but ignored.
    let res = tokio::time::timeout(
        crate::server::DOWNSTREAM_REGISTER_TIMEOUT,
        proxyproto::read_header(io),
    )
    .await;
    let (header, io) = match res {
        Ok(Ok(v)) => v,
        _ => return,
    };
    let trusted = accept_proxy(&srv, a.remote, a.unix);
    let mut remote = a.remote;
    if trusted && let Some(h) = &header {
        if h.source.is_some() {
            remote = h.source;
        }
        peer_cert = h.client_cert.clone();
    }
    let io: BoxIo = Box::new(io);

    let remote_addr = match remote {
        Some(r) => fmt_addr(r),
        None => "@".to_owned(),
    };
    let info = ConnInfo {
        remote_addr: remote_addr.clone(),
        remote_ip: remote.map(|r| r.ip().to_canonical()),
        local_addr: String::new(),
        peer_cert,
    };

    match kind {
        Kind::Irc => {
            crate::metrics_guard(&srv, async {
                let logger = srv
                    .logger
                    .with_prefix(format!("downstream {remote_addr:?}: "));
                let (conn, reader) = Conn::new_stream(io, logger.clone(), None);
                crate::downstream::serve(srv.clone(), conn, reader, info, logger).await;
            })
            .await;
        }
        Kind::Admin => {
            let logger = srv.logger.with_prefix(format!("admin {remote_addr:?}: "));
            let (conn, reader) = Conn::new_stream(io, logger.clone(), None);
            srv.clone().serve_admin(conn, reader, logger).await;
        }
        Kind::Ident => {
            let Some(identd) = srv.identd() else { return };
            let host = remote
                .map(|r| r.ip().to_canonical().to_string())
                .unwrap_or_default();
            identd.handle(io, &host).await;
        }
        Kind::Http | Kind::Prometheus => unreachable!(),
    }
}

/// Parses a Forwarded header (RFC 7239), or the X-Forwarded-* headers.
fn parse_forwarded(headers: &hyper::HeaderMap) -> Option<String> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let Some(forwarded) = get("Forwarded") else {
        return get("X-Forwarded-For").filter(|v| !v.is_empty());
    };
    // Like Go's mime.ParseMediaType("hack; " + forwarded): a single element
    // of semicolon-separated key=value pairs
    let mut result = None;
    for part in forwarded.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=')?;
        let k = k.trim().to_lowercase();
        let v = v.trim();
        if !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
        {
            return None;
        }
        let v = if let Some(q) = v.strip_prefix('"') {
            q.strip_suffix('"')?.replace("\\\"", "\"")
        } else {
            if v.contains([',', ' ', '"']) {
                return None;
            }
            v.to_owned()
        };
        if k == "for" {
            result = Some(v);
        }
    }
    result.filter(|v| !v.is_empty())
}

fn parse_client_cert(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix(':')?.strip_suffix(':')?;
    BASE64.decode(s).ok()
}

/// Checks the Origin header of a WebSocket request, like
/// github.com/coder/websocket.
fn authenticate_origin(req: &Request<Incoming>, patterns: &[String]) -> Result<()> {
    let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(());
    };
    if origin.is_empty() {
        return Ok(());
    }
    let u =
        parse_url(origin).map_err(|e| anyhow!("failed to parse Origin header {origin:?}: {e}"))?;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host.eq_ignore_ascii_case(&u.host) {
        return Ok(());
    }
    for pattern in patterns {
        let target = if pattern.contains("://") {
            format!("{}://{}", u.scheme, u.host)
        } else {
            u.host.clone()
        };
        if crate::util::path_match(&pattern.to_lowercase(), &target.to_lowercase())
            .map_err(|e| anyhow!("failed to parse path pattern {pattern:?}: {e}"))?
        {
            return Ok(());
        }
    }
    if u.host.is_empty() {
        bail!("request Origin {origin:?} is not a valid URL with a host");
    }
    bail!(
        "request Origin {:?} is not authorized for Host {host:?}",
        u.host
    )
}

fn response(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = status;
    r
}

async fn serve_http(
    srv: Arc<Server>,
    kind: Kind,
    io: BoxIo,
    remote: Option<SocketAddr>,
    unix: bool,
    peer_cert: Option<Vec<u8>>,
) {
    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let srv = srv.clone();
        let peer_cert = peer_cert.clone();
        async move {
            let resp = if kind == Kind::Prometheus {
                let mut r = response(StatusCode::OK, srv.render_metrics());
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
                );
                r
            } else {
                handle_http_request(srv, req, remote, unix, peer_cert).await
            };
            Ok::<_, Infallible>(resp)
        }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades()
        .await;
}

async fn handle_http_request(
    srv: Arc<Server>,
    req: Request<Incoming>,
    remote: Option<SocketAddr>,
    unix: bool,
    peer_cert: Option<Vec<u8>>,
) -> Response<Full<Bytes>> {
    match req.uri().path() {
        "/socket" => handle_websocket(srv, req, remote, unix, peer_cert),
        "/icon" => {
            let cfg = srv.config();
            if cfg.icon_path.is_empty() {
                return response(StatusCode::NOT_FOUND, "404 page not found\n");
            }
            match tokio::fs::read(&cfg.icon_path).await {
                Ok(data) => {
                    let mut r = response(StatusCode::OK, data);
                    if let Some(ct) = content_type_for(&cfg.icon_path) {
                        r.headers_mut()
                            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
                    }
                    r
                }
                Err(_) => response(StatusCode::NOT_FOUND, "404 page not found\n"),
            }
        }
        _ => response(StatusCode::NOT_FOUND, "404 page not found\n"),
    }
}

fn content_type_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn handle_websocket(
    srv: Arc<Server>,
    mut req: Request<Incoming>,
    remote: Option<SocketAddr>,
    unix: bool,
    peer_cert: Option<Vec<u8>>,
) -> Response<Full<Bytes>> {
    let headers = req.headers();
    let has_token = |name: header::HeaderName, token: &str| {
        headers.get_all(name).iter().any(|v| {
            v.to_str()
                .map(|s| s.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
                .unwrap_or(false)
        })
    };
    if !has_token(header::CONNECTION, "upgrade") {
        return response(
            StatusCode::UPGRADE_REQUIRED,
            "WebSocket protocol violation: Connection header \"\" does not contain Upgrade\n",
        );
    }
    if !has_token(header::UPGRADE, "websocket") {
        return response(
            StatusCode::UPGRADE_REQUIRED,
            "WebSocket protocol violation: Upgrade header does not contain websocket\n",
        );
    }
    if req.method() != hyper::Method::GET {
        return response(
            StatusCode::METHOD_NOT_ALLOWED,
            "WebSocket protocol violation: handshake request method is not GET\n",
        );
    }
    if headers
        .get(header::SEC_WEBSOCKET_VERSION)
        .and_then(|v| v.to_str().ok())
        != Some("13")
    {
        return response(
            StatusCode::BAD_REQUEST,
            "unsupported WebSocket protocol version (only 13 is supported)\n",
        );
    }
    let Some(key) = headers
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|k| k.as_bytes().to_vec())
    else {
        return response(
            StatusCode::BAD_REQUEST,
            "WebSocket protocol violation: missing Sec-WebSocket-Key\n",
        );
    };

    let cfg = srv.config();
    if let Err(e) = authenticate_origin(&req, &cfg.basic.http_origins) {
        logf!(srv.logger, "failed to serve HTTP connection: {e}");
        return response(StatusCode::FORBIDDEN, format!("{e}\n"));
    }

    // Subprotocol negotiation: "text.ircv3.net" (non-compliant, fight me)
    let subprotocol = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find(|p| p.eq_ignore_ascii_case("text.ircv3.net"))
        .map(str::to_owned);

    // Only trust the Forwarded header field if this is a trusted proxy IP,
    // to prevent users from spoofing the remote address
    let accept_proxy = remote.is_some_and(|r| cfg.basic.accept_proxy_ips.contains(r.ip()));
    let _ = unix;
    let mut remote_addr = match remote {
        Some(r) => fmt_addr(r),
        None => "@".to_owned(),
    };
    if accept_proxy && let Some(f) = parse_forwarded(headers) {
        remote_addr = f;
    }
    let mut cert = peer_cert;
    if accept_proxy
        && let Some(cc) = headers.get("Client-Cert").and_then(|v| v.to_str().ok())
        && !cc.is_empty()
    {
        cert = parse_client_cert(cc);
    }

    let accept = derive_accept_key(&key);
    let upgrade = hyper::upgrade::on(&mut req);
    let info = ConnInfo {
        remote_addr: remote_addr.clone(),
        remote_ip: None,
        local_addr: String::new(),
        peer_cert: cert,
    };
    tokio::spawn(async move {
        let upgraded = match upgrade.await {
            Ok(u) => u,
            Err(e) => {
                logf!(srv.logger, "failed to serve HTTP connection: {e}");
                return;
            }
        };
        let io: BoxIo = Box::new(TokioIo::new(upgraded));
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        crate::metrics_guard(&srv, async {
            let logger = srv
                .logger
                .with_prefix(format!("downstream {remote_addr:?}: "));
            let (conn, reader) = Conn::new_websocket(ws, logger.clone());
            crate::downstream::serve(srv.clone(), conn, reader, info, logger).await;
        })
        .await;
    });

    let mut r = response(StatusCode::SWITCHING_PROTOCOLS, Bytes::new());
    let h = r.headers_mut();
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(
        header::SEC_WEBSOCKET_ACCEPT,
        HeaderValue::from_str(&accept).unwrap(),
    );
    if let Some(p) = subprotocol
        && let Ok(v) = HeaderValue::from_str(&p)
    {
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, v);
    }
    r
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

    #[test]
    fn forwarded() {
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "Forwarded",
            HeaderValue::from_static("for=\"[2001:db8::1]:1234\";proto=https"),
        );
        assert_eq!(parse_forwarded(&h).as_deref(), Some("[2001:db8::1]:1234"));
        let mut h = hyper::HeaderMap::new();
        h.insert("X-Forwarded-For", HeaderValue::from_static("1.2.3.4"));
        assert_eq!(parse_forwarded(&h).as_deref(), Some("1.2.3.4"));
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "Forwarded",
            HeaderValue::from_static("for=1.1.1.1, for=2.2.2.2"),
        );
        assert_eq!(parse_forwarded(&h), None);
    }

    #[test]
    fn client_cert_header() {
        assert_eq!(parse_client_cert(":AAEC:"), Some(vec![0, 1, 2]));
        assert_eq!(parse_client_cert("AAEC"), None);
    }
}
