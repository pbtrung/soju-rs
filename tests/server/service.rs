//! Service regression scenarios.

use super::*;

/// BouncerServ commands waiting on another user's event loop.
pub(super) async fn service_other_user() {
    let env = setup_with(db_store(), |db, user| {
        user.admin = true;
        db.store_user(&mut database::User::new("other")).unwrap();
    })
    .await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user update other -max-networks 5"
    ));
    assert_eq!(read_service_reply(&mut dc).await, r#"updated user "other""#);

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user run other user update -max-networks 3"
    ));
    assert_eq!(read_service_reply(&mut dc).await, r#"updated user "other""#);

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user run other network status"
    ));
    let reply = read_service_reply(&mut dc).await;
    assert!(!reply.starts_with("error:"), "unexpected reply: {reply}");

    let hash = {
        use sha1::{Digest, Sha1};
        hex::encode(&Sha1::digest(b"other")[..3])
    };
    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        format!("user delete other {hash}")
    ));
    assert_eq!(read_service_reply(&mut dc).await, r#"deleted user "other""#);
    assert!(env.srv.get_user("other").is_none());
    env.srv.shutdown().await;
}

/// BouncerServ commands doing CPU-heavy work (key generation, password
/// hashing) off the event loop, then applying it.
pub(super) async fn service_blocking_work() {
    let env = setup_with(db_store(), |_, user| user.admin = true).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "certfp generate -key-type ed25519"
    ));
    assert_eq!(read_service_reply(&mut dc).await, "certificate generated");
    assert!(
        read_service_reply(&mut dc)
            .await
            .starts_with("SHA-1 fingerprint: ")
    );
    read_service_reply(&mut dc).await;
    read_service_reply(&mut dc).await;
    let user_id = env.srv.db.get_user(TEST_USERNAME).unwrap().unwrap().id;
    let sasl = &env.srv.db.list_networks(user_id).unwrap()[0].sasl;
    assert_eq!(sasl.mechanism, "EXTERNAL");
    assert!(sasl.external_key.is_some());

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user update -password newpass"
    ));
    assert_eq!(
        read_service_reply(&mut dc).await,
        format!("updated user {TEST_USERNAME:?}")
    );
    let mut record = env.srv.db.get_user(TEST_USERNAME).unwrap().unwrap();
    record.check_password("newpass").unwrap();

    dc.send(msg!(
        "PRIVMSG",
        "BouncerServ",
        "user create -username created -password pw"
    ));
    assert_eq!(
        read_service_reply(&mut dc).await,
        r#"created user "created""#
    );
    assert!(env.srv.get_user("created").is_some());
    env.srv.shutdown().await;
}

/// Identical nicknames, network names and channel names must not mix live
/// traffic or history, either between users or between a user's networks.
pub(super) async fn user_network_channel_isolation() {
    let db = Database::open_in_memory().unwrap();
    let mut scopes = Vec::new();
    for username in [TEST_USERNAME, "other-user"] {
        let mut user = database::User::new(username);
        user.set_password(TEST_PASSWORD).unwrap();
        db.store_user(&mut user).unwrap();
        for name in ["testnet", "othernet"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut network = database::Network::new(&format!(
                "irc+insecure://{}",
                listener.local_addr().unwrap()
            ));
            network.name = name.to_owned();
            network.nick = TEST_USERNAME.to_owned();
            db.store_network(user.id, &mut network).unwrap();
            scopes.push((username, network, listener));
        }
    }
    let srv = Server::new(db, Logger::new(false));
    srv.set_config(ServerConfig::default());
    srv.start().unwrap();

    let mut connections = Vec::new();
    for (username, network, listener) in &scopes {
        let (stream, _) = tokio::time::timeout(TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut uc = TestConn::new(Box::new(stream));
        register_upstream(&mut uc).await;
        for channel in ["#shared", "#other"] {
            uc.send(msg!("JOIN", channel).with_prefix(Prefix::full(TEST_USERNAME, "u", "h")));
            uc.send(
                msg!("366", TEST_USERNAME, channel, "End of /NAMES list")
                    .with_prefix(server_prefix()),
            );
        }
        uc.roundtrip().await;
        let mut dc = connect_downstream(&srv);
        register_downstream_as(&mut dc, username, network).await;
        dc.roundtrip().await;
        connections.push((uc, dc));
    }

    // Interleave bursts from all networks before reading any client. Each
    // client must see exactly its own messages, in their original order.
    let mut expected = vec![Vec::new(); connections.len()];
    for sequence in 0..6 {
        for (scope, (uc, _)) in connections.iter().enumerate() {
            for channel in ["#shared", "#other"] {
                let text = format!("scope {scope}, {channel}, message {sequence}");
                uc.send(msg!("PRIVMSG", channel, text.clone()).with_prefix(Prefix::new("peer")));
                expected[scope].push(vec![channel.to_owned(), text]);
            }
        }
    }
    for (uc, _) in &mut connections {
        uc.roundtrip().await;
    }
    for (scope, (_, dc)) in connections.iter_mut().enumerate() {
        let live: Vec<_> = dc
            .roundtrip()
            .await
            .into_iter()
            .map(|m| {
                assert_eq!(m.command, "PRIVMSG", "unexpected live reply: {m}");
                m.params
            })
            .collect();
        assert_eq!(live, expected[scope], "live traffic for scope {scope}");
        for channel in ["#shared", "#other"] {
            let want: Vec<_> = expected[scope]
                .iter()
                .filter(|params| params[0] == channel)
                .map(|params| params[1].clone())
                .collect();
            assert_eq!(history_texts(dc, channel).await, want, "scope {scope}");
        }
    }

    // Commands travel in the other direction through the same scope.
    for (scope, (_, dc)) in connections.iter().enumerate() {
        dc.send(msg!("PRIVMSG", "#shared", format!("outgoing {scope}")));
    }
    for (_, dc) in &mut connections {
        dc.roundtrip().await;
    }
    for (scope, (uc, _)) in connections.iter_mut().enumerate() {
        let outgoing: Vec<_> = uc
            .roundtrip()
            .await
            .into_iter()
            .filter(|m| m.command == "PRIVMSG")
            .map(|m| m.params)
            .collect();
        assert_eq!(
            outgoing,
            [vec!["#shared".to_owned(), format!("outgoing {scope}")]]
        );
    }

    // Knowing another user's numeric network ID grants no access to it.
    let foreign_id = scopes[2].1.id.to_string();
    let dc = &mut connections[0].1;
    dc.send(msg!(
        "BOUNCER",
        "CHANGENETWORK",
        foreign_id.clone(),
        "name=stolen"
    ));
    let denied = dc.expect("FAIL").await;
    assert_eq!(
        denied.params[..3],
        ["BOUNCER", "INVALID_NETID", "CHANGENETWORK"]
    );
    assert_eq!(denied.params[3], foreign_id);
    srv.shutdown().await;
}
