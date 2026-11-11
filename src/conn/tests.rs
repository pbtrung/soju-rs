use super::transport::parse_line;
use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[tokio::test]
async fn stream_roundtrip() {
    let (a, b) = tokio::io::duplex(4096);
    let logger = Logger::new(false);
    let (ca, mut ra) = Conn::new_stream(Box::new(a), logger.clone(), None);
    let (cb, mut rb) = Conn::new_stream(Box::new(b), logger, None);
    ca.send_message(crate::msg!("PRIVMSG", "#a", "hello world"));
    ca.send_message(crate::msg!("PING", "x"));
    let m = rb.read_message().await.unwrap().unwrap();
    assert_eq!(m.to_string(), "PRIVMSG #a :hello world");
    assert_eq!(rb.read_message().await.unwrap().unwrap().command, "PING");
    ca.shutdown();
    assert!(rb.read_message().await.unwrap().is_none());
    cb.close();
    assert!(rb.read_message().await.unwrap().is_none());
    drop(ra.read_message().await);
}

#[tokio::test]
async fn rate_limited() {
    tokio::time::pause();
    let (a, b) = tokio::io::duplex(4096);
    let logger = Logger::new(false);
    let flag: RateLimitFlag = Arc::new(AtomicBool::new(true));
    let rate = RateLimit {
        delay: Duration::from_secs(2),
        burst: 2,
    };
    let (ca, _ra) = Conn::new_stream(Box::new(a), logger.clone(), Some((rate, flag)));
    let (_cb, mut rb) = Conn::new_stream(Box::new(b), logger, None);
    let start = tokio::time::Instant::now();
    for i in 0..3 {
        ca.send_message(crate::msg!("PING", i.to_string()));
    }
    for _ in 0..3 {
        rb.read_message().await.unwrap().unwrap();
    }
    assert!(start.elapsed() >= Duration::from_secs(2));
}

fn tagged_message() -> Message {
    crate::msg!("PRIVMSG", "#a", "hello world")
        .with_prefix(crate::irc::Prefix::full("nick", "user", "host"))
        .with_tag("time", "2020-01-01T00:00:00.000Z")
        .with_tag("msgid", "a b;c")
}

#[test]
fn queued_line() {
    let msg = tagged_message();
    let line = format_line(&msg);
    assert_eq!(line, msg.to_string());
    assert_eq!(line.capacity(), line.len());
    // Line breaks can't inject another message
    let line = format_line(&crate::msg!("PRIVMSG", "#a", "x\r\nQUIT"));
    assert_eq!(line, "PRIVMSG #a :x  QUIT");
    let msg = Message::parse(&line).unwrap();
    assert_eq!(msg.params, ["#a", "x  QUIT"]);
    let line = format_line(&crate::msg!("PRIVMSG", "#a\n", "x").with_tag("a\nb", "c\n"));
    assert_eq!(line, "@a b=c\\n PRIVMSG #a  x");
}

#[tokio::test]
async fn stream_wire_format() {
    use tokio::io::AsyncReadExt;
    let (a, mut b) = tokio::io::duplex(4096);
    let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let msg = tagged_message();
    c.send_message(msg.clone());
    c.send_message(crate::msg!("PING", "x"));
    c.shutdown();
    let mut wire = Vec::new();
    b.read_to_end(&mut wire).await.unwrap();
    assert_eq!(
        String::from_utf8(wire).unwrap(),
        format!("{msg}\r\nPING x\r\n")
    );
}

#[tokio::test]
async fn stream_lines_preserve_message_boundaries() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    let (_c, mut r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let large = "x".repeat(4096);
    b.write_all(b"\r\n@time=2024-01-01T00:00:00Z :peer!u@h PRIVMSG #a :hello\r\n")
        .await
        .unwrap();
    b.write_all(format!("PRIVMSG #a :{large}\r\n").as_bytes())
        .await
        .unwrap();
    b.write_all(b"\nPRIVMSG #a :invalid \xff\r\nPING next\r\nPRIVMSG #a :unfinished")
        .await
        .unwrap();
    b.shutdown().await.unwrap();

    let first = r.read_message().await.unwrap().unwrap();
    assert_eq!(first.command, "PRIVMSG");
    assert_eq!(first.params, ["#a", "hello"]);
    assert_eq!(first.prefix_name(), "peer");
    assert_eq!(first.tags.get("time"), Some("2024-01-01T00:00:00Z"));
    let second = r.read_message().await.unwrap().unwrap();
    assert_eq!(second.params, ["#a", large.as_str()]);
    let third = r.read_message().await.unwrap().unwrap();
    assert_eq!(third.params, ["#a", "invalid \u{fffd}"]);
    let fourth = r.read_message().await.unwrap().unwrap();
    assert_eq!(fourth.command, "PING");
    assert_eq!(fourth.params, ["next"]);
    // Reusing the input storage must not change previously parsed messages.
    assert_eq!(first.params, ["#a", "hello"]);
    assert!(r.read_message().await.unwrap().is_none());
    assert!(r.read_message().await.unwrap().is_none());
}

#[tokio::test]
async fn stream_parse_error_does_not_replay_failed_line() {
    let (a, mut b) = tokio::io::duplex(4096);
    let (_c, mut r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    b.write_all(b"@tag-without-command\r\nPING next\r\n")
        .await
        .unwrap();
    assert!(r.read_message().await.is_err());
    let next = r.read_message().await.unwrap().unwrap();
    assert_eq!(next.command, "PING");
    assert_eq!(next.params, ["next"]);
}

#[tokio::test]
async fn websocket_wire_format() {
    let (a, b) = tokio::io::duplex(4096);
    let ws = |io: tokio::io::DuplexStream, role| {
        WebSocketStream::from_raw_socket(Box::new(io) as BoxIo, role, None)
    };
    let (server, mut client) = tokio::join!(
        ws(a, tokio_tungstenite::tungstenite::protocol::Role::Server),
        ws(b, tokio_tungstenite::tungstenite::protocol::Role::Client)
    );
    let (c, _r) = Conn::new_websocket(server, Logger::new(false));
    let msg = tagged_message();
    c.send_message(msg.clone());
    // One text frame per message, without CRLF
    match client.next().await.unwrap().unwrap() {
        WsMessage::Text(t) => assert_eq!(t.as_str(), msg.to_string()),
        m => panic!("unexpected frame: {m:?}"),
    }
}

#[test]
fn outgoing_credentials_are_redacted() {
    for msg in [
        crate::msg!("PASS", "secret"),
        crate::msg!("AUTHENTICATE", "encoded-secret"),
        crate::msg!("BOUNCERSERV", "user create -password secret"),
        crate::msg!("PRIVMSG", "NickServ", "IDENTIFY secret"),
    ] {
        let logged = redact_line_for_log(&format_line(&msg));
        assert!(!logged.contains("secret"));
        assert!(logged.contains("<redacted>"));
    }
    assert_eq!(redact_line_for_log("PING alive"), "PING alive");
}

#[tokio::test]
async fn websocket_rejects_oversized_frames_without_listener_configuration() {
    let (a, b) = tokio::io::duplex(1 << 17);
    let server = WebSocketStream::from_raw_socket(
        Box::new(a) as BoxIo,
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let mut client = WebSocketStream::from_raw_socket(
        Box::new(b) as BoxIo,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let (conn, mut reader) = Conn::new_websocket(server, Logger::new(false));
    client
        .send(WsMessage::text(format!(
            "PRIVMSG #a :{}",
            "x".repeat(MAX_LINE_LENGTH)
        )))
        .await
        .unwrap();
    assert!(
        reader
            .read_message()
            .await
            .unwrap_err()
            .to_string()
            .contains("line too long")
    );
    conn.close();
}

#[tokio::test]
async fn queue_budget() {
    // The peer never reads
    let (a, _b) = tokio::io::duplex(4096);
    let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let text = "x".repeat(400);
    let n = MAX_QUEUED_BYTES / (text.len() + QUEUED_LINE_OVERHEAD) + 1;
    for _ in 0..n {
        c.send_message(crate::msg!("PRIVMSG", "#a", text.clone()));
    }
    assert!(c.is_closed());
    assert!(c.shared.queued_bytes.load(Ordering::Relaxed) <= MAX_QUEUED_BYTES);
}

#[tokio::test]
async fn input_in_flight() {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ca, _ra) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let (_cb, mut rb) = Conn::new_stream(Box::new(b), Logger::new(false), None);
    for i in 0..MAX_INPUT_IN_FLIGHT + 1 {
        ca.send_message(crate::msg!("PRIVMSG", "#a", i.to_string()));
    }
    ca.send_message(crate::msg!("PING", "alive"));
    let mut permits = Vec::new();
    for _ in 0..MAX_INPUT_IN_FLIGHT {
        permits.push(rb.read_message_limited().await.unwrap().unwrap());
    }
    // Waiting for a permit is cancel-safe: the message isn't lost
    let next = tokio::time::timeout(Duration::from_millis(50), rb.read_message_limited());
    assert!(next.await.is_err());
    permits.pop();
    let (msg, permit) = rb.read_message_limited().await.unwrap().unwrap();
    assert_eq!(msg.params[1], MAX_INPUT_IN_FLIGHT.to_string());
    assert!(permit.is_some());
    // Keepalives are read even when every permit is held
    let (msg, permit) = rb.read_message_limited().await.unwrap().unwrap();
    assert_eq!(msg.command, "PING");
    assert!(permit.is_some());
}

#[tokio::test]
async fn cancelled_read_preserves_partial_line_and_length_limit() {
    for oversized in [false, true] {
        let (a, mut b) = tokio::io::duplex(1 << 16);
        let (_c, mut r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
        b.write_all(b"PRIVMSG #a :hel").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), r.read_message_limited())
                .await
                .is_err()
        );
        if oversized {
            b.write_all(&vec![b'x'; MAX_LINE_LENGTH]).await.unwrap();
            b.write_all(b"\r\n").await.unwrap();
            assert!(
                r.read_message_limited()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("line too long")
            );
        } else {
            b.write_all(b"lo\r\nPING next\r\n").await.unwrap();
            let (m, _permit) = r.read_message_limited().await.unwrap().unwrap();
            assert_eq!(m.command, "PRIVMSG");
            assert_eq!(m.params, ["#a", "hello"]);
            assert_eq!(
                r.read_message_limited().await.unwrap().unwrap().0.command,
                "PING"
            );
        }
    }
}

#[tokio::test]
async fn keepalive_flood_has_a_separate_bounded_backlog() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let (_peer, mut r) = Conn::new_stream(Box::new(b), Logger::new(false), None);
    let mut ordinary = Vec::new();
    for _ in 0..MAX_INPUT_IN_FLIGHT {
        c.send_message(crate::msg!("PRIVMSG", "#a", "text"));
        ordinary.push(r.read_message_limited().await.unwrap().unwrap());
    }
    let mut keepalives = Vec::new();
    for i in 0..=MAX_KEEPALIVE_IN_FLIGHT {
        c.send_message(crate::msg!(
            if i % 2 == 0 { "PING" } else { "PONG" },
            i.to_string()
        ));
    }
    for _ in 0..MAX_KEEPALIVE_IN_FLIGHT {
        keepalives.push(r.read_message_limited().await.unwrap().unwrap());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), r.read_message_limited())
            .await
            .is_err()
    );
    keepalives.pop();
    let (m, permit) = r.read_message_limited().await.unwrap().unwrap();
    assert_eq!(m.params[0], MAX_KEEPALIVE_IN_FLIGHT.to_string());
    assert!(permit.is_some());
    // No ordinary-message permits were released to allow this keepalive.
    assert_eq!(ordinary.len(), MAX_INPUT_IN_FLIGHT);
}

#[tokio::test]
async fn full_queue_closes_physical_connection_from_view() {
    for notify in [false, true] {
        let (a, _b) = tokio::io::duplex(4096);
        let (c, mut r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
        let v = c.view("soju.im/network", "1", || {});
        // No yield: the writer cannot drain any of this exact budget.
        let text = "x".repeat(MAX_QUEUED_BYTES - QUEUED_LINE_OVERHEAD - "PRIVMSG #a ".len());
        c.send_message(crate::msg!("PRIVMSG", "#a", text));
        assert_eq!(c.queued_bytes(), MAX_QUEUED_BYTES);
        if notify {
            v.close();
        } else {
            v.send_message(crate::msg!("PING", "x"));
        }
        assert!(c.is_closed() && v.is_closed());
        assert!(r.read_message().await.unwrap().is_none());
    }
}

#[tokio::test]
async fn view() {
    let (a, b) = tokio::io::duplex(4096);
    let (c, _r) = Conn::new_stream(Box::new(a), Logger::new(false), None);
    let (_cb, mut rb) = Conn::new_stream(Box::new(b), Logger::new(false), None);
    let closed = Arc::new(AtomicUsize::new(0));
    let n = closed.clone();
    let v = c.view("soju.im/network", "1", move || {
        n.fetch_add(1, Ordering::SeqCst);
    });
    v.send_message(crate::msg!("PING", "x").with_tag("label", "a"));
    let m = rb.read_message().await.unwrap().unwrap();
    assert_eq!(m.tags.get("soju.im/network"), Some("1"));
    assert_eq!(m.tags.get("label"), Some("a"));

    // Closing the view tells the peer, and leaves the connection open
    v.close();
    v.close();
    assert!(v.is_closed() && !c.is_closed());
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    let m = rb.read_message().await.unwrap().unwrap();
    assert_eq!(m.command, "ERROR");
    assert_eq!(m.tags.get("soju.im/network"), Some("1"));
    v.send_message(crate::msg!("PING", "dropped"));
    c.send_message(crate::msg!("PING", "y"));
    let m = rb.read_message().await.unwrap().unwrap();
    assert_eq!(m.params, ["y"]);
    assert!(m.tags.get("soju.im/network").is_none());

    // Closed quietly: no ERROR
    let q = c.view("soju.im/network", "2", || {});
    q.close_quietly();
    c.send_message(crate::msg!("PING", "z"));
    assert_eq!(rb.read_message().await.unwrap().unwrap().params, ["z"]);

    // Closing the connection closes its views
    let w = c.view("soju.im/network", "3", || {});
    c.close();
    assert!(w.is_closed());
}

#[test]
fn line_parsing() {
    assert!(parse_line("").unwrap().is_none());
    assert_eq!(
        parse_line("PRIVMSG #a :x\r\nQUIT")
            .unwrap()
            .unwrap()
            .to_string(),
        "PRIVMSG #a x"
    );
}
