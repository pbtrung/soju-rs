//! Native HTTP listener checks. These run against real TCP connections.
use std::time::Duration;

use soju::database::Database;
use soju::listen::{parse_listen_uri, spawn_listener};
use soju::logger::Logger;
use soju::server::Server;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
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
        stream.write_all(format!("GET /socket HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n").as_bytes()).await.unwrap();
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
