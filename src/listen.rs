//! Listeners: IRC (TCP, TLS, Unix), HTTP/WebSocket, ident, Prometheus and
//! the admin socket.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::conn::{BoxIo, Conn, ConnInfo};
use crate::database::join_host_port;
use crate::server::Server;
use crate::{logf, proxyproto};

mod address;
mod forwarded;
mod http;

pub use address::{Address, Kind, Listener, parse_listen_uri};
use http::serve_http;

/// TCP keep-alive interval for downstream TCP connections.
const DOWNSTREAM_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(3600);

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
            if let Some(port) = a.strip_prefix(':') {
                // All interfaces, IPv4 only if IPv6 is unavailable
                return match tokio::net::TcpListener::bind(format!("[::]:{port}")).await {
                    Ok(l) => Ok(BoundListener::Tcp(l)),
                    Err(e) if is_af_unsupported(&e) => Ok(BoundListener::Tcp(
                        tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?,
                    )),
                    Err(e) => Err(e.into()),
                };
            }
            // Like upstream, prefer the first IPv4 address of a hostname,
            // e.g. 127.0.0.1 for localhost rather than ::1
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(a.as_str()).await?.collect();
            let Some(addr) = addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()) else {
                bail!("no addresses found for {a:?}");
            };
            Ok(BoundListener::Tcp(
                tokio::net::TcpListener::bind(addr).await?,
            ))
        }
        #[cfg(unix)]
        Address::Unix(path) => {
            // Remove stale socket files left by a previous instance
            if let Ok(md) = std::fs::symlink_metadata(path) {
                use std::os::unix::fs::FileTypeExt;
                if md.file_type().is_socket() {
                    match tokio::net::UnixStream::connect(path).await {
                        Ok(_) => bail!("Unix socket {path:?} is already in use"),
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                            std::fs::remove_file(path)?;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Ok(BoundListener::Unix(tokio::net::UnixListener::bind(path)?))
        }
        #[cfg(not(unix))]
        Address::Unix(_) => bail!("Unix sockets are not supported on this platform"),
    }
}

/// Whether an error means the address family isn't supported, e.g. IPv6
/// disabled in the kernel.
fn is_af_unsupported(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(libc::EAFNOSUPPORT);
    #[cfg(not(unix))]
    false
}

/// Formats an address as "host:port", with IPv4-mapped IPv6 addresses as
/// IPv4.
pub fn fmt_addr(a: SocketAddr) -> String {
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
        serve_http(srv, kind, io, a.remote, peer_cert).await;
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
        if let Some(cert) = &h.client_cert {
            peer_cert = Some(cert.clone());
        }
    }
    let io: BoxIo = Box::new(io);

    let remote_addr = match remote {
        Some(r) => fmt_addr(r),
        None => "@".to_owned(),
    };
    let info = ConnInfo {
        remote_addr: remote_addr.clone(),
        remote_ip: remote.map(|r| r.ip().to_canonical()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_binding_preserves_live_sockets_and_regular_files() {
        let path = std::env::temp_dir().join(format!(
            "soju-listener-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let address = Address::Unix(path.to_str().unwrap().to_owned());
        let original = tokio::net::UnixListener::bind(&path).unwrap();
        assert!(bind(&address).await.is_err());
        // The original socket remains reachable and has not been replaced.
        tokio::net::UnixStream::connect(&path).await.unwrap();
        drop(original);
        // A stale socket left after the listener closes can be reclaimed.
        let replacement = bind(&address).await.unwrap();
        drop(replacement);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "keep").unwrap();
        assert!(bind(&address).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
        std::fs::remove_file(&path).unwrap();
    }
}
