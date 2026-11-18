//! Native HTTP listener checks. These run against real TCP connections.
use std::time::Duration;

use soju::database::Database;
use soju::listen::{parse_listen_uri, spawn_listener};
use soju::logger::Logger;
use soju::server::Server;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
async fn websocket_handshake_validates_keys_before_upgrading() {
    let server = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    let address = spawn_listener(
        server.clone(),
        parse_listen_uri("http+insecure://127.0.0.1:0").unwrap(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    for (key, expected_status) in [
        ("not-base64", "400"),
        ("", "400"),
        ("AAEC", "400"),
        ("dGhlIHNhbXBsZSBub25jZQ==", "101"),
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET /socket HTTP/1.1\r\nHost: localhost\r\nConnection: \
            Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: \
            {key}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut status))
            .await
            .unwrap()
            .unwrap();
        assert!(
            status.starts_with(&format!("HTTP/1.1 {expected_status} ")),
            "{status:?}"
        );
        if expected_status == "101" {
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=")
            );
        }
    }
    server.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
async fn websocket_disconnect_releases_the_user_and_allows_reconnection() {
    use futures_util::{SinkExt, StreamExt};
    use soju::irc::Message;
    use std::sync::atomic::Ordering;
    use tokio_tungstenite::tungstenite::Message as Frame;

    let database = Database::open_in_memory().unwrap();
    let mut user = soju::database::User::new("alice");
    user.set_password("secret").unwrap();
    database.store_user(&mut user).unwrap();
    let server = Server::new(database, Logger::new(false));
    server.start().unwrap();
    let address = spawn_listener(
        server.clone(),
        parse_listen_uri("http+insecure://127.0.0.1:0").unwrap(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let handle = server.get_user("alice").unwrap();
    for graceful in [false, true] {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut client, _) =
            tokio_tungstenite::client_async(format!("ws://{address}/socket"), stream)
                .await
                .unwrap();
        for line in ["PASS secret", "NICK alice", "USER alice 0 * :Alice"] {
            client.send(Frame::text(line)).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let frame = client.next().await.unwrap().unwrap();
                if let Frame::Text(text) = frame
                    && Message::parse(&text).unwrap().command == "001"
                {
                    break;
                }
            }
        })
        .await
        .expect("WebSocket registration did not complete");
        // Welcome output can reach the client before the actor finishes
        // attachment accounting on its blocking worker.
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.num_downstream_conns.load(Ordering::SeqCst) != 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("registered WebSocket was not counted");
        assert_eq!(handle.num_downstream_conns.load(Ordering::SeqCst), 1);
        if graceful {
            client.close(None).await.unwrap();
        }
        // Dropping without a close frame reproduces an abrupt browser/proxy reset.
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.num_downstream_conns.load(Ordering::SeqCst) != 0
                || server.stats().downstreams != 0
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("disconnected WebSocket retained a downstream connection");
    }
    assert_eq!(server.user_records().len(), 1);
    server.shutdown().await;
}

async fn wait_for_sessions(server: &Server, expected: usize) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.accepted_sessions() != expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .map_err(|error| {
        anyhow::anyhow!(
            "waiting for {expected} sessions, observed {}: {error}",
            server.accepted_sessions()
        )
    })?;
    Ok(())
}

fn limited_server(limit: usize) -> anyhow::Result<std::sync::Arc<Server>> {
    let server = Server::new(Database::open_in_memory()?, Logger::new(false));
    let mut cfg = server.config().as_ref().clone();
    cfg.basic.max_accepted_connections = limit;
    server.set_config(cfg);
    Ok(server)
}

#[tokio::test]
async fn admission_bounds_incomplete_handshakes_and_releases_errors() -> anyhow::Result<()> {
    use soju::listen::{Address, Kind, Listener, TlsState};
    use tokio::io::AsyncReadExt;

    let server = limited_server(2)?;
    let mut addresses = Vec::new();
    for (kind, tls) in [(Kind::Irc, false), (Kind::Irc, true), (Kind::Http, false)] {
        let address = spawn_listener(
            server.clone(),
            Listener {
                label: format!("{kind:?}-{tls}"),
                kind,
                addr: Address::Tcp("127.0.0.1:0".into()),
                tls,
            },
            Some(TlsState {
                certs: std::sync::Arc::default(),
            }),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("TCP address missing"))?;
        addresses.push((address, if tls { b"\x16" } else { b"P" }));
    }
    for (address, prefix) in &addresses {
        // A partial PROXY line, TLS header or HTTP request occupies admission.
        let mut held = Vec::new();
        for _ in 0..2 {
            let mut stream = tokio::net::TcpStream::connect(address).await?;
            stream.write_all(*prefix).await?;
            held.push(stream);
        }
        wait_for_sessions(&server, 2)
            .await
            .map_err(|error| error.context(format!("admit on {address}")))?;
        let task_count = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        let fds_before = std::fs::read_dir("/proc/self/fd").ok().map(Iterator::count);
        for _ in 0..100 {
            // Every listener shares the same budget; no waiter task is spawned.
            let mut excess = tokio::net::TcpStream::connect(
                addresses
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("no listeners"))?
                    .0,
            )
            .await?;
            let mut byte = [0];
            let read = tokio::time::timeout(Duration::from_secs(2), excess.read(&mut byte)).await?;
            assert!(matches!(read, Ok(0) | Err(_)));
            assert_eq!(server.accepted_sessions(), 2);
            assert!(
                tokio::runtime::Handle::current()
                    .metrics()
                    .num_alive_tasks()
                    <= task_count + 2
            );
        }
        let fds_after = std::fs::read_dir("/proc/self/fd").ok().map(Iterator::count);
        println!(
            "admission burst on {address}: active={} tasks={} fds={fds_before:?}->{fds_after:?}",
            server.accepted_sessions(),
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks()
        );
        drop(held);
        wait_for_sessions(&server, 0)
            .await
            .map_err(|error| error.context(format!("release on {address}")))?;
    }
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn admission_survives_websocket_upgrade_reload_and_shutdown() -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;

    let server = limited_server(2)?;
    let address = spawn_listener(
        server.clone(),
        parse_listen_uri("http+insecure://127.0.0.1:0")?,
        None,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("TCP address missing"))?;
    let mut clients = Vec::new();
    for _ in 0..2 {
        let (client, _) = tokio_tungstenite::client_async(
            format!("ws://{address}/socket"),
            tokio::net::TcpStream::connect(address).await?,
        )
        .await?;
        clients.push(client);
    }
    wait_for_sessions(&server, 2).await?;
    let mut cfg = server.config().as_ref().clone();
    cfg.basic.max_accepted_connections = 1;
    server.reload_config(cfg);
    let mut excess = tokio::net::TcpStream::connect(address).await?;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), excess.read(&mut byte)).await??,
        0
    );
    let mut client = clients
        .pop()
        .ok_or_else(|| anyhow::anyhow!("missing client"))?;
    client.close(None).await?;
    drop(client);
    wait_for_sessions(&server, 1).await?;
    // The upgraded survivor still owns its physical-session permit. Shutdown
    // cancels it even while it has not sent IRC registration.
    server.shutdown().await;
    wait_for_sessions(&server, 0).await?;
    drop(clients);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn admission_includes_unix_admin_and_shutdown_cancellation() -> anyhow::Result<()> {
    use soju::listen::{Address, Kind, Listener};
    use tokio::io::AsyncReadExt;

    let server = limited_server(1)?;
    let path = std::env::temp_dir().join(format!(
        "soju-admission-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    spawn_listener(
        server.clone(),
        Listener {
            label: "admin admission test".into(),
            kind: Kind::Admin,
            addr: Address::Unix(path.to_string_lossy().into_owned()),
            tls: false,
        },
        None,
    )
    .await?;
    let mut held = tokio::net::UnixStream::connect(&path).await?;
    // Complete PROXY sniffing, then wait in the administrative IRC reader.
    held.write_all(b"PING :test\r\n").await?;
    wait_for_sessions(&server, 1).await?;
    let mut excess = tokio::net::UnixStream::connect(&path).await?;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), excess.read(&mut byte)).await??,
        0
    );
    server.shutdown().await;
    wait_for_sessions(&server, 0).await?;
    std::fs::remove_file(path)?;
    Ok(())
}
