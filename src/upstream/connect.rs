//! Connecting to upstream servers, and reconnecting.

use super::*;

pub struct RunnerParams {
    pub srv: Arc<Server>,
    pub tx: mpsc::UnboundedSender<Event>,
    pub user_id: i64,
    pub user_ident: String,
    pub record: Arc<Mutex<db::Network>>,
    pub runner_id: u64,
    pub stop: CancellationToken,
    pub logger: Logger,
}

/// Connects to the upstream server and reconnects with exponential backoff,
/// until the network is stopped.
pub async fn run_network(p: RunnerParams) {
    let mut last_try: Option<Instant> = None;
    let mut backoff = Backoffer::new(
        crate::user::RETRY_CONNECT_MIN_DELAY,
        crate::user::RETRY_CONNECT_MAX_DELAY,
        crate::user::RETRY_CONNECT_JITTER,
    );
    loop {
        // A closed event channel means the user's event loop is gone (e.g.
        // it panicked): stop instead of reconnecting in a loop
        if p.stop.is_cancelled() || p.tx.is_closed() {
            return;
        }

        let delay = backoff.next_delay();
        let elapsed = last_try.map(|t| t.elapsed()).unwrap_or(Duration::MAX);
        if delay > elapsed {
            let delay = delay - elapsed;
            let addr = p.record.lock().unwrap().addr.clone();
            logf!(
                p.logger,
                "waiting {}s before trying to reconnect to {addr:?}",
                delay.as_secs()
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = p.stop.cancelled() => return,
                _ = p.tx.closed() => return,
            }
        }
        last_try = Some(Instant::now());

        let record = p.record.lock().unwrap().clone();
        let net_id = record.id;
        p.srv.metrics.upstreams.fetch_add(1, Ordering::Relaxed);
        let res = run_conn(&p, &record).await;
        p.srv.metrics.upstreams.fetch_sub(1, Ordering::Relaxed);

        match res {
            Ok(()) => backoff.reset(),
            Err(err) => {
                logf!(
                    p.logger,
                    "connection error to {:?}: {}",
                    record.addr,
                    err.text
                );
                let _ = p.tx.send(Event::UpstreamConnectionError {
                    net_id,
                    runner_id: p.runner_id,
                    err: format!("connection error: {}", err.event),
                });
                metrics::inc(&p.srv.metrics.upstream_connect_errors_total);
                if !err.temporary {
                    return;
                }
            }
        }
    }
}

async fn run_conn(p: &RunnerParams, record: &db::Network) -> Result<(), OutcomeError> {
    let to_err = |e: anyhow::Error| {
        let text = format!("failed to connect: {e}");
        OutcomeError {
            event: text.clone(),
            text,
            temporary: true,
        }
    };

    let logger = p
        .logger
        .with_prefix(format!("upstream {:?}: ", record.get_name()));
    let identd = p.srv.identd();
    let connected = tokio::select! {
        res = tokio::time::timeout(CONNECT_TIMEOUT, connect(p, record, &logger, identd.as_deref())) => res,
        _ = p.stop.cancelled() => return Ok(()),
    };
    let (io, _identd_entry) = match connected {
        Ok(Ok(c)) => c,
        // Upstream performs the TLS handshake lazily, on the first write:
        // handshake errors surface while reading registration replies
        Ok(Err(e)) if e.is::<TlsHandshakeError>() => {
            let text = format!("failed to register: failed to read message: {e}");
            return Err(OutcomeError {
                event: text.clone(),
                text,
                temporary: true,
            });
        }
        Ok(Err(e)) => return Err(to_err(e)),
        Err(_) => return Err(to_err(anyhow!("i/o timeout"))),
    };

    let rate_flag: RateLimitFlag = Arc::new(AtomicBool::new(true));
    let rate = RateLimit {
        delay: UPSTREAM_MESSAGE_DELAY,
        burst: UPSTREAM_MESSAGE_BURST,
    };
    let (conn, mut reader) = Conn::new_stream(io, logger.clone(), Some((rate, rate_flag.clone())));
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    let (outcome_tx, outcome_rx) = oneshot::channel();

    let new = NewConn {
        id: conn_id,
        conn: conn.clone(),
        rate_flag,
        outcome_tx,
        net_id: record.id,
        events: p.tx.clone(),
        srv: p.srv.clone(),
    };
    if p.tx
        .send(Event::UpstreamConnected {
            net_id: record.id,
            runner_id: p.runner_id,
            conn: Box::new(new),
        })
        .is_err()
    {
        conn.close();
        return Ok(());
    }

    let read_error = loop {
        let res = tokio::select! {
            res = reader.read_message() => res,
            _ = p.stop.cancelled() => {
                conn.close();
                Ok(None)
            }
            _ = p.tx.closed() => {
                conn.close();
                Ok(None)
            }
        };
        match res {
            Ok(Some(msg)) => {
                metrics::inc(&p.srv.metrics.upstream_in_messages_total);
                let _ = p.tx.send(Event::UpstreamMessage {
                    net_id: record.id,
                    conn_id,
                    msg,
                });
            }
            Ok(None) => break None,
            Err(e) => break Some(e.to_string()),
        }
    };
    conn.close();
    let _ = p.tx.send(Event::UpstreamClosed {
        net_id: record.id,
        conn_id,
        read_error,
    });
    let outcome = outcome_rx.await.unwrap_or(ConnOutcome { err: None });

    if p.stop.is_cancelled() {
        return Ok(());
    }
    match outcome.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Resolves a host, randomizing the result order instead of always
/// returning the same IP address, to distribute bursts of connections to the
/// same host among available addresses. IPv6 is preferred, for per-user
/// local IP addresses.
async fn resolve_ip_addr(host: &str) -> Result<IpAddr> {
    let addrs: Vec<IpAddr> = tokio::net::lookup_host((host, 0))
        .await?
        .map(|a| a.ip())
        .collect();
    let v6: Vec<IpAddr> = addrs.iter().copied().filter(|a| a.is_ipv6()).collect();
    let pool = if v6.is_empty() { addrs } else { v6 };
    pool.choose(&mut rand::rng())
        .copied()
        .ok_or_else(|| anyhow!("no such host"))
}

/// Formats an OS error like Go's syscall.Errno: the C error message with a
/// lowercase first letter, without Rust's " (os error N)" suffix.
pub fn go_errno_message(e: &std::io::Error) -> String {
    let s = e.to_string();
    let s = match s.rfind(" (os error ") {
        Some(i) => &s[..i],
        None => &s,
    };
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Connects to a TCP address, with errors worded like Go's net.Dial.
async fn connect_tcp(addr: &str) -> Result<tokio::net::TcpStream> {
    let host = crate::conn::host_of(addr);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .map_err(|_| anyhow!("dial tcp: lookup {host}: no such host"))?
        .collect();
    let mut last_err = anyhow!("dial tcp: lookup {host}: no such host");
    for a in addrs {
        match tokio::net::TcpStream::connect(a).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = anyhow!("dial tcp {a}: connect: {}", go_errno_message(&e)),
        }
    }
    Err(last_err)
}

async fn dial_tcp(srv: &Server, user_id: i64, addr: &str) -> Result<tokio::net::TcpStream> {
    let upstream_user_ips = srv.config().basic.upstream_user_ips.clone();
    if upstream_user_ips.is_empty() {
        return connect_tcp(addr).await;
    }
    let (host, port) = split_host_port(addr)?;
    let ip = resolve_ip_addr(&host)
        .await
        .map_err(|e| anyhow!("failed to resolve host {host:?}: {e}"))?;
    let local = crate::user::local_tcp_addr(&upstream_user_ips, user_id, ip)
        .map_err(|e| anyhow!("failed to pick local IP for remote host {host:?}: {e}"))?;
    let port: u16 = port.parse()?;
    let remote = SocketAddr::new(ip, port);
    let socket = if ip.is_ipv6() {
        tokio::net::TcpSocket::new_v6()?
    } else {
        tokio::net::TcpSocket::new_v4()?
    };
    if let Some(local) = local {
        socket
            .bind(SocketAddr::new(local, 0))
            .map_err(|e| anyhow!("dial tcp {remote}: bind: {}", go_errno_message(&e)))?;
    }
    socket
        .connect(remote)
        .await
        .map_err(|e| anyhow!("dial tcp {remote}: connect: {}", go_errno_message(&e)))
}

/// Dials the upstream server. Returns the stream and the remote/local
/// addresses (for identd).
async fn connect<'a>(
    p: &RunnerParams,
    record: &db::Network,
    logger: &Logger,
    identd: Option<&'a crate::identd::Identd>,
) -> Result<(BoxIo, Option<IdentdEntry<'a>>)> {
    let u = record.url()?;
    match u.scheme.as_str() {
        "ircs" => {
            let (host, addr) = match split_host_port(&u.host) {
                Ok((h, _)) => (h, u.host.clone()),
                Err(_) => (u.host.clone(), format!("{}:6697", u.host)),
            };
            let host = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned();

            let client_cert = if record.sasl.mechanism == "EXTERNAL" {
                let Some(cert) = &record.sasl.external_cert else {
                    bail!("missing certificate for authentication");
                };
                let Some(key) = &record.sasl.external_key else {
                    bail!("missing private key for authentication");
                };
                use sha2::Digest;
                logf!(
                    logger,
                    "using TLS client certificate {}",
                    hex::encode(sha2::Sha256::digest(cert))
                );
                Some((cert.as_slice(), key.as_slice()))
            } else {
                None
            };
            let tls_config = crate::tls::client_config(&record.certfp, client_cert)?;

            logf!(logger, "connecting to TLS server at address {addr:?}");
            let tcp = dial_tcp(&p.srv, p.user_id, &addr).await?;
            // Register the connection with identd before the TLS handshake:
            // servers may query identd as soon as the TCP connection is up
            let entry = IdentdEntry::store(identd, &tcp, &p.user_ident);
            let server_name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| anyhow!("invalid server name {host:?}: {e}"))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config)
                .connect(server_name, tcp)
                .await
                .map_err(|e| TlsHandshakeError(e.to_string()))?;
            Ok((Box::new(tls), entry))
        }
        "irc+insecure" => {
            let addr = match split_host_port(&u.host) {
                Ok(_) => u.host.clone(),
                Err(_) => format!("{}:6667", u.host),
            };
            logf!(
                logger,
                "connecting to plain-text server at address {addr:?}"
            );
            let tcp = dial_tcp(&p.srv, p.user_id, &addr).await?;
            let entry = IdentdEntry::store(identd, &tcp, &p.user_ident);
            Ok((Box::new(tcp), entry))
        }
        "irc+unix" | "unix" => {
            logf!(logger, "connecting to Unix socket at path {:?}", u.path);
            #[cfg(unix)]
            {
                let s = tokio::net::UnixStream::connect(&u.path)
                    .await
                    .map_err(|e| {
                        anyhow!(
                            "failed to connect to Unix socket {:?}: dial unix {}: connect: {}",
                            u.path,
                            u.path,
                            go_errno_message(&e)
                        )
                    })?;
                Ok((Box::new(s), None))
            }
            #[cfg(not(unix))]
            bail!("Unix sockets are not supported on this platform")
        }
        s => bail!("failed to dial {:?}: unknown scheme: {s}", record.addr),
    }
}

/// A connection registered with identd, removed when dropped.
struct IdentdEntry<'a> {
    identd: &'a crate::identd::Identd,
    remote: String,
    local: String,
}

impl<'a> IdentdEntry<'a> {
    fn store(
        identd: Option<&'a crate::identd::Identd>,
        tcp: &tokio::net::TcpStream,
        ident: &str,
    ) -> Option<IdentdEntry<'a>> {
        let fmt = crate::listen::fmt_addr;
        let entry = IdentdEntry {
            identd: identd?,
            remote: fmt(tcp.peer_addr().ok()?),
            local: fmt(tcp.local_addr().ok()?),
        };
        entry.identd.store(&entry.remote, &entry.local, ident);
        Some(entry)
    }
}

impl Drop for IdentdEntry<'_> {
    fn drop(&mut self) {
        self.identd.delete(&self.remote, &self.local);
    }
}
