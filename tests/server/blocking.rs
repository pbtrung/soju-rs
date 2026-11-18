//! Contended SQLite operations must leave independent async I/O and timers live.
use super::*;

#[expect(clippy::unwrap_used, reason = "Fixture setup and assertions must fail immediately on invalid data.")]
pub(super) async fn sqlite_contention_does_not_stall_network_timers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let path = std::env::temp_dir().join(format!("soju-contention-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&path).unwrap();
    let file = path.join("contention.db");
    let db = Database::open(file.to_str().unwrap()).unwrap();
    let mut record = database::User::new(TEST_USERNAME);
    db.store_user(&mut record).unwrap();
    let server = Server::new(db, Logger::new(false));
    server.start().unwrap();
    let user = server.get_user(TEST_USERNAME).unwrap();
    let (ready, receive) = tokio::sync::oneshot::channel();
    assert!(user.send(soju::user::Event::Run(Box::new(move |_| { assert!(ready.send(()).is_ok()); }))));
    receive.await.unwrap();
    let lock = rusqlite::Connection::open(file).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        lock.execute_batch("COMMIT;").unwrap();
    });
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let first_order = order.clone();
    let (started, receive) = tokio::sync::oneshot::channel();
    let (done, complete) = tokio::sync::oneshot::channel();
    assert!(user.send(soju::user::Event::Run(Box::new(move |actor| {
        first_order.lock().unwrap().push("started");
        assert!(started.send(std::time::Instant::now()).is_ok());
        let mut contender = database::User::new("contended-write");
        actor.srv.db.store_user(&mut contender).unwrap();
        first_order.lock().unwrap().push("finished");
        assert!(done.send(()).is_ok());
    }))));
    let started = receive.await.unwrap();
    // Repeated config changes coalesce behind one active expiry/VACUUM worker.
    for i in 0..1000 {
        let mut config = server.config().as_ref().clone();
        config.basic.message_expiry = Duration::from_hours(24 + i % 2);
        server.reload_config(config);
    }
    assert!(server.maintenance_tasks() <= 3);
    let (mut client, mut peer) = tokio::io::duplex(64);
    let echo = tokio::spawn(async move {
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        peer.write_all(&byte).await.unwrap();
    });
    client.write_all(b"x").await.unwrap();
    let mut byte = [0];
    client.read_exact(&mut byte).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(started.elapsed() < Duration::from_millis(200), "independent I/O/timer stalled: {:?}", started.elapsed());
    assert_eq!(byte, *b"x");
    echo.await.unwrap();
    let next_order = order.clone();
    let (next, receive) = tokio::sync::oneshot::channel();
    assert!(user.send(soju::user::Event::Run(Box::new(move |_| {
        next_order.lock().unwrap().push("next");
        assert!(next.send(()).is_ok());
    }))));
    complete.await.unwrap();
    receive.await.unwrap();
    assert_eq!(*order.lock().unwrap(), ["started", "finished", "next"]);
    release.join().unwrap();
    server.shutdown().await;
    assert_eq!(server.maintenance_tasks(), 0);
    drop(server);
    std::fs::remove_dir_all(path).unwrap();
}
