//! HTTP endpoints and WebSocket upgrades.

use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use http_body_util::Full;
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header::{self, HeaderValue},
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::derive_accept_key,
        protocol::{Role, WebSocketConfig},
    },
};

use super::forwarded::{parse_client_cert, parse_forwarded};
use super::{Kind, fmt_addr};
use crate::conn::{BoxIo, Conn, ConnInfo};
use crate::database::parse_url;
use crate::logf;
use crate::server::Server;

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

/// How long a client may take to send the headers of a request, including
/// the first one.
const HTTP_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn serve_http(
    srv: Arc<Server>,
    kind: Kind,
    io: BoxIo,
    remote: Option<SocketAddr>,
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
                handle_http_request(srv, req, remote, peer_cert).await
            };
            Ok::<_, Infallible>(resp)
        }
    });
    // Without a timer, hyper doesn't enforce its header read timeout: idle
    // connections would be kept forever
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HTTP_HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades()
        .await;
}

async fn handle_http_request(
    srv: Arc<Server>,
    req: Request<Incoming>,
    remote: Option<SocketAddr>,
    peer_cert: Option<Vec<u8>>,
) -> Response<Full<Bytes>> {
    match req.uri().path() {
        "/socket" => handle_websocket(srv, req, remote, peer_cert),
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

    if BASE64
        .decode(&key)
        .ok()
        .is_none_or(|decoded| decoded.len() != 16)
    {
        return response(
            StatusCode::BAD_REQUEST,
            "WebSocket protocol violation: invalid Sec-WebSocket-Key\n",
        );
    }

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
    // to prevent users from spoofing the remote address. Unix sockets have
    // no remote address: proxy headers are ignored there.
    let accept_proxy = remote.is_some_and(|r| cfg.basic.accept_proxy_ips.contains(r.ip()));
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
        // Like upstream, whose WebSocket connections don't have a TCP
        // address: device certificates record no IP for them
        remote_ip: None,
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
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, Some(websocket_config())).await;
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

/// Upstream's WebSocket library limits messages to 32 KiB by default:
/// bigger ones close the connection.
const WEBSOCKET_READ_LIMIT: usize = crate::conn::MAX_LINE_LENGTH;

/// The WebSocket read buffer is allocated upfront for each connection, the
/// write buffer grows up to this size before being written out. IRC lines
/// are small: the defaults (128 KiB each) would mostly hold nothing. Larger
/// frames still fit, the buffers grow as needed.
const WEBSOCKET_READ_BUFFER_SIZE: usize = 4096;
const WEBSOCKET_WRITE_BUFFER_SIZE: usize = 8192;

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(WEBSOCKET_READ_BUFFER_SIZE)
        .write_buffer_size(WEBSOCKET_WRITE_BUFFER_SIZE)
        .max_message_size(Some(WEBSOCKET_READ_LIMIT))
        .max_frame_size(Some(WEBSOCKET_READ_LIMIT))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Lines bigger than the WebSocket buffers still go through, both ways.
    #[tokio::test]
    async fn websocket_large_lines() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let logger = crate::logger::Logger::new(false);
        let ws = |io: tokio::io::DuplexStream, role| {
            WebSocketStream::from_raw_socket(Box::new(io) as BoxIo, role, Some(websocket_config()))
        };
        let (server, client) = tokio::join!(ws(a, Role::Server), ws(b, Role::Client));
        let (sc, mut sr) = Conn::new_websocket(server, logger.clone());
        let (cc, mut cr) = Conn::new_websocket(client, logger);
        let text = "x".repeat(2 * WEBSOCKET_WRITE_BUFFER_SIZE);
        cc.send_message(crate::msg!("PRIVMSG", "#a", text.clone()));
        let m = sr.read_message().await.unwrap().unwrap();
        assert_eq!(m.params[1], text);
        sc.send_message(crate::msg!("PRIVMSG", "#a", text.clone()));
        let m = cr.read_message().await.unwrap().unwrap();
        assert_eq!(m.params[1], text);
    }
}
