//! Protocol regression scenarios.

use super::*;

pub(super) async fn broadcast() {
    let env = setup(MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    })
    .await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    let notice = "This is a very important server notice.";
    uc.send(msg!("NOTICE", TEST_USERNAME, notice).with_prefix(server_prefix()));
    let msg = loop {
        let msg = dc.read().await;
        if msg.command == "NOTICE" {
            break msg;
        }
    };
    assert_eq!(msg.params[1], notice);
    env.srv.shutdown().await;
}

pub(super) async fn statusmsg_keeps_sigil() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream_with(&mut uc, &["STATUSMSG=@+"]).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "echo-message").await;

    // A message to "@#chan" must only reach channel operators
    dc.send(msg!("PRIVMSG", "@#chan", "ops only"));
    let m = read_until(&mut uc, "PRIVMSG").await;
    assert_eq!(m.params, vec!["@#chan", "ops only"]);
    // The upstream doesn't support echo-message: the bouncer echoes it
    let echo = read_until(&mut dc, "PRIVMSG").await;
    assert_eq!(echo.params, vec!["@#chan", "ops only"]);
    env.srv.shutdown().await;
}

pub(super) async fn upstream_malformed_messages() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    // Multi-byte membership prefixes used to panic when parsing NAMES
    let nick = register_upstream_with(&mut uc, &["PREFIX=(abc)\u{FFFD}"]).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;
    dc.roundtrip().await;

    // Messages without a prefix come from the server, they used to panic
    for cmd in ["NICK", "JOIN", "SETNAME", "ACCOUNT"] {
        uc.send(msg!(cmd, "x"));
    }
    let us = Prefix::full(nick.clone(), "u", "h");
    uc.send(msg!("JOIN", "#c").with_prefix(us));
    uc.send(msg!("353", nick.clone(), "=", "#c", "\u{F000}x @y").with_prefix(server_prefix()));
    uc.send(msg!("366", nick.clone(), "#c", "End of /NAMES list").with_prefix(server_prefix()));
    uc.roundtrip().await;

    // The user's event loop is still alive
    uc.send(msg!("NOTICE", nick, "still alive").with_prefix(Prefix::new("bob")));
    let m = read_until(&mut dc, "NOTICE").await;
    assert_eq!(m.params[1], "still alive");
    env.srv.shutdown().await;
}

pub(super) async fn cap_req_removal() {
    let env = setup(db_store()).await;
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("CAP", "LS", "302"));
    let caps = read_cap_ls(&mut dc).await;
    assert!(
        caps.iter().any(|c| c.starts_with("draft/multiline=")),
        "{caps:?}"
    );
    dc.send(msg!("CAP", "REQ", "batch server-time"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1..], ["ACK", "batch server-time"]);
    dc.send(msg!("CAP", "REQ", "-batch"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1..], ["ACK", "-batch"]);
    // A REQ is atomic: one unavailable cap rejects all of them
    dc.send(msg!("CAP", "REQ", "batch bogus-cap"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1..], ["NAK", "batch bogus-cap"]);
    dc.send(msg!("CAP", "LIST"));
    let m = dc.expect("CAP").await;
    assert_eq!(m.params[1], "LIST");
    let mut caps: Vec<&str> = m.params[2].split(' ').collect();
    caps.sort();
    assert_eq!(caps, ["cap-notify", "server-time"]);
    env.srv.shutdown().await;
}

pub(super) async fn labeled_response_ack() {
    let env = setup(db_store()).await;
    // labeled-response is passed through from the upstream: use a bouncer
    // connection, which has none
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, None, "labeled-response batch").await;

    // A labeled command without any reply gets an ACK
    dc.send(msg!("PONG", "foo").with_tag("label", "z"));
    let m = dc.expect("ACK").await;
    assert_eq!(m.tags.get("label"), Some("z"));

    // Errors keep the label, without a batch around them
    dc.send(msg!("CHATHISTORY", "WHAT", "#x").with_tag("label", "e"));
    let m = dc.expect("FAIL").await;
    assert_eq!(m.tags.get("label"), Some("e"));
    assert_eq!(m.params[..3], ["CHATHISTORY", "INVALID_PARAMS", "WHAT"]);
    env.srv.shutdown().await;
}

/// A peer sending commands before registration without reading their
/// replies is disconnected, rather than having them buffered.
pub(super) async fn unread_replies_before_registration() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let env = setup(db_store()).await;
    let (a, mut b) = tokio::io::duplex(1 << 16);
    let logger = Logger::new(false);
    let (conn, reader) = Conn::new_stream(Box::new(a), logger.clone(), None);
    let info = ConnInfo {
        remote_addr: "127.0.0.1:1234".to_owned(),
        ..Default::default()
    };
    tokio::spawn(soju::downstream::serve(
        env.srv.clone(),
        conn,
        reader,
        info,
        logger,
    ));
    // Each request of a few bytes queues a much longer reply
    let flood = "CAP LS 302\r\n".repeat(5000);
    let (mut rd, mut wr) = tokio::io::split(&mut b);
    let write = async {
        // The server stops reading once it closes the connection
        let _ = wr.write_all(flood.as_bytes()).await;
    };
    let wait_closed = async {
        // Not reading until the server gave up, then draining to EOF
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut sink = Vec::new();
        rd.read_to_end(&mut sink).await.unwrap();
        sink.len()
    };
    let (_, received) = tokio::time::timeout(TIMEOUT, async { tokio::join!(write, wait_closed) })
        .await
        .expect("the connection wasn't closed");
    // Far less than the replies to all requests
    assert!(received < 5000 * 500, "received {received} bytes");
    env.srv.shutdown().await;
}

/// MONITOR ignores empty targets and targets containing spaces.
pub(super) async fn monitor_invalid_targets() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream_with(&mut uc, &["MONITOR=100"]).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    dc.send(msg!("MONITOR", "+", "a,,b"));
    dc.send(msg!("MONITOR", "+", "c d"));
    dc.send(msg!("MONITOR", "L"));
    let mut names = Vec::new();
    loop {
        let m = dc.read().await;
        match m.command.as_str() {
            "732" => names.push(m.params[1].clone()),
            "733" => break,
            _ => {}
        }
    }
    names.sort();
    assert_eq!(names, ["a", "b"]);
    env.srv.shutdown().await;
}

pub(super) async fn setname_survives_network_update() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_caps(&mut uc, "setname").await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "setname").await;
    dc.roundtrip().await;
    dc.send(msg!("SETNAME", "New real name"));
    let m = read_until(&mut uc, "SETNAME").await;
    assert_eq!(m.params, ["New real name"]);
    uc.send(msg!("SETNAME", "New real name").with_prefix(Prefix::full(nick, "u", "h")));
    let m = read_until(&mut dc, "SETNAME").await;
    assert_eq!(m.params, ["New real name"]);
    dc.send(msg!(
        "BOUNCER",
        "CHANGENETWORK",
        env.network.id.to_string(),
        "name=renamed"
    ));
    read_until(&mut dc, "BOUNCER").await;
    let user_id = env.srv.db.get_user(TEST_USERNAME).unwrap().unwrap().id;
    let networks = env.srv.db.list_networks(user_id).unwrap();
    assert_eq!(networks[0].name, "renamed");
    assert_eq!(networks[0].realname, "New real name");
    // The reconnect uses the snapshot, not just the persisted row.
    let mut next = accept_upstream(&env).await;
    next.expect("CAP").await;
    next.expect("NICK").await;
    assert_eq!(next.expect("USER").await.params[3], "New real name");
    env.srv.shutdown().await;
}
