//! Persistence regression scenarios.

use super::*;

/// A disconnected client can read messages received while it was away,
/// without interrupting another client or duplicating subsequent live input.
pub(super) async fn disconnected_client_history() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut first = connect_downstream(&env.srv);
    register_downstream(&mut first, &env.network).await;
    first.roundtrip().await;
    let mut observer = connect_downstream(&env.srv);
    register_downstream(&mut observer, &env.network).await;
    observer.roundtrip().await;

    // QUIT closes only this client's session. Wait for EOF, not a sleep.
    first.send(msg!("QUIT", "back soon"));
    tokio::time::timeout(TIMEOUT, async {
        while first.reader.read_message().await.unwrap().is_some() {}
    })
    .await
    .expect("QUIT did not close the client connection");

    let texts: Vec<_> = (0..20).map(|i| format!("while away {i}")).collect();
    for text in &texts {
        uc.send(msg!("PRIVMSG", TEST_USERNAME, text.clone()).with_prefix(Prefix::new("peer")));
    }
    uc.roundtrip().await;
    let received: Vec<_> = observer
        .roundtrip()
        .await
        .into_iter()
        .map(|m| {
            assert_eq!(m.command, "PRIVMSG", "{m}");
            m.params[1].clone()
        })
        .collect();
    assert_eq!(received, texts);

    let mut reconnected = connect_downstream(&env.srv);
    register_downstream(&mut reconnected, &env.network).await;
    reconnected.roundtrip().await;
    assert_eq!(history_texts(&mut reconnected, "peer").await, texts);
    assert!(
        observer.roundtrip().await.is_empty(),
        "history leaked to another client"
    );

    uc.send(msg!("PRIVMSG", TEST_USERNAME, "back online").with_prefix(Prefix::new("peer")));
    uc.roundtrip().await;
    for dc in [&mut observer, &mut reconnected] {
        let messages = dc.roundtrip().await;
        assert_eq!(messages.len(), 1, "unexpected live messages: {messages:?}");
        assert_eq!(messages[0].command, "PRIVMSG");
        assert_eq!(messages[0].params[1], "back online");
        let mut want = texts.clone();
        want.push("back online".to_owned());
        assert_eq!(history_texts(dc, "peer").await, want);
    }
    env.srv.shutdown().await;
}

/// Reopen an actual SQLite file with a new server: messages and read markers
/// must remain available, and new messages must extend the old history.
pub(super) async fn sqlite_history_survives_restart() {
    let dir = TestDatabaseDir::new();
    let path = dir.0.join("history.db");
    let source = path.to_str().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let db = Database::open(source).unwrap();
    let mut user = database::User::new(TEST_USERNAME);
    user.set_password(TEST_PASSWORD).unwrap();
    db.store_user(&mut user).unwrap();
    let mut network = database::Network::new(&format!(
        "irc+insecure://{}",
        listener.local_addr().unwrap()
    ));
    network.name = "testnet".to_owned();
    db.store_network(user.id, &mut network).unwrap();
    drop(db);

    let texts = ["before shutdown 1", "before shutdown 2", "after restart"];
    let marker = "timestamp=2024-01-01T00:00:00.000Z";
    for generation in 0..2 {
        // Each generation gets a distinct database connection and server.
        let db = Database::open(source).unwrap();
        let srv = Server::new(db, Logger::new(false));
        srv.set_config(ServerConfig::default());
        srv.start().unwrap();
        let (stream, _) = tokio::time::timeout(TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut uc = TestConn::new(Box::new(stream));
        register_upstream(&mut uc).await;
        uc.roundtrip().await;
        let mut dc = connect_downstream(&srv);
        register_downstream_caps(&mut dc, Some(&network), "draft/read-marker").await;

        if generation == 0 {
            for text in &texts[..2] {
                uc.send(msg!("PRIVMSG", TEST_USERNAME, *text).with_prefix(Prefix::new("peer")));
            }
            uc.roundtrip().await;
            dc.roundtrip().await;
            assert_eq!(history_texts(&mut dc, "peer").await, texts[..2]);
            dc.send(msg!("MARKREAD", "peer", marker));
            assert_eq!(dc.expect("MARKREAD").await.params, ["peer", marker]);
        } else {
            assert_eq!(history_texts(&mut dc, "peer").await, texts[..2]);
            dc.send(msg!("MARKREAD", "peer"));
            assert_eq!(dc.expect("MARKREAD").await.params, ["peer", marker]);
            uc.send(msg!("PRIVMSG", TEST_USERNAME, texts[2]).with_prefix(Prefix::new("peer")));
            uc.roundtrip().await;
            let live = dc.roundtrip().await;
            assert_eq!(live.len(), 1, "{live:?}");
            assert_eq!(live[0].command, "PRIVMSG");
            assert_eq!(live[0].params[1], texts[2]);
            assert_eq!(history_texts(&mut dc, "peer").await, texts);
        }
        srv.shutdown().await;
    }
}

struct TestDatabaseDir(std::path::PathBuf);

impl TestDatabaseDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("soju-history-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDatabaseDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
