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
