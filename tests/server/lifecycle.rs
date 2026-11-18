//! Session cancellation and cleanup regressions.
use super::*;

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions must fail immediately on invalid data."
)]
pub(super) async fn aborted_downstream_session() {
    let env = setup(db_store()).await;
    let mut upstream = accept_upstream(&env).await;
    register_upstream(&mut upstream).await;
    let (io, peer) = tokio::io::duplex(1 << 16);
    let logger = Logger::new(false);
    let (conn, reader) = Conn::new_stream(Box::new(io), logger.clone(), None);
    let task = tokio::spawn(soju::downstream::serve(
        env.srv.clone(),
        conn,
        reader,
        ConnInfo::default(),
        logger,
    ));
    let mut client = TestConn::new(Box::new(peer));
    register_downstream(&mut client, &env.network).await;
    client.roundtrip().await;
    let user = env.srv.get_user(TEST_USERNAME).unwrap();
    assert_eq!(
        user.num_downstream_conns
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    task.abort();
    assert!(task.await.is_err());
    tokio::time::timeout(TIMEOUT, async {
        while user
            .num_downstream_conns
            .load(std::sync::atomic::Ordering::SeqCst)
            != 0
        {
            tokio::task::yield_now().await;
        }
        assert!(client.reader.read_message().await.unwrap().is_none());
    })
    .await
    .unwrap();
    assert_eq!(env.srv.user_records().len(), 1);
    env.srv.shutdown().await;
}

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions must fail immediately on invalid data."
)]
pub(super) async fn webpush_subscription_limit() {
    let env = setup(db_store()).await;
    let mut upstream = accept_upstream(&env).await;
    register_upstream(&mut upstream).await;
    let user = env.srv.get_user(TEST_USERNAME).unwrap().record();
    for i in 0..25 {
        let mut sub = database::WebPushSubscription {
            endpoint: format!("https://push.example/{i}"),
            ..Default::default()
        };
        env.srv
            .db
            .store_web_push_subscription(user.id, env.network.id, &mut sub)
            .unwrap();
    }
    let mut client = connect_downstream(&env.srv);
    register_downstream_caps(&mut client, Some(&env.network), "soju.im/webpush").await;
    client.send(msg!(
        "WEBPUSH",
        "REGISTER",
        "https://push.example/extra",
        "auth=a;p256dh=b"
    ));
    let reply = client.expect("FAIL").await;
    assert_eq!(
        reply.params,
        [
            "WEBPUSH",
            "INTERNAL_ERROR",
            "REGISTER",
            "Too many subscriptions"
        ]
    );
    assert_eq!(
        env.srv
            .db
            .list_web_push_subscriptions(user.id, env.network.id)
            .unwrap()
            .len(),
        25
    );
    let (send, receive) = tokio::sync::oneshot::channel();
    assert!(
        env.srv
            .get_user(TEST_USERNAME)
            .unwrap()
            .send(soju::user::Event::Run(Box::new(move |user| {
                let available =
                    u32::try_from(user.web_push_check_slots.available_permits()).unwrap();
                assert!(available > 0);
                let slots = user
                    .web_push_check_slots
                    .clone()
                    .try_acquire_many_owned(available)
                    .unwrap();
                assert!(send.send(slots).is_ok());
            })))
    );
    let slots = receive.await.unwrap();
    client.send(msg!(
        "WEBPUSH",
        "REGISTER",
        "https://push.example/0",
        "auth=a;p256dh=b"
    ));
    let reply = client.expect("FAIL").await;
    assert_eq!(
        reply.params,
        [
            "WEBPUSH",
            "INTERNAL_ERROR",
            "REGISTER",
            "Too many pending subscriptions, try again later"
        ]
    );
    drop(slots);
    env.srv.shutdown().await;
}
