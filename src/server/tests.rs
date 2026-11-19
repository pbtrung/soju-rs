use super::*;
use crate::config::MsgStoreDriver;
use crate::{msg, xirc};
use jiff::Timestamp;

#[tokio::test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
async fn oversized_maintenance_durations_return_errors() {
    let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    for duration in [Duration::MAX, Duration::from_hours(175_680_000)] {
        let mut cfg = ServerConfig::default();
        cfg.basic.message_expiry = duration;
        cfg.basic.disable_inactive_users_delay = duration;
        srv.set_config(cfg);
        assert!(srv.delete_expired_messages().is_err());
        assert!(srv.disable_inactive_users().await.is_err());
    }
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
fn expiry_vacuums_only_when_at_least_25_percent_of_pages_are_free() {
    let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    let mut cfg = ServerConfig::default();
    cfg.basic.message_expiry = Duration::from_hours(24);
    srv.set_config(cfg);
    let mut user = db::User::new("vacuum");
    srv.db.store_user(&mut user).unwrap();
    let mut net = db::Network::new("irc.example.org");
    srv.db.store_network(user.id, &mut net).unwrap();
    let text = "x".repeat(1000);
    let now = Timestamp::now();
    for (count, time) in [
        (300, now - jiff::SignedDuration::from_hours(72)),
        (3700, now),
    ] {
        let messages: Vec<_> = (0..count)
            .map(|_| {
                msg!("PRIVMSG", "#a", text.clone()).with_tag("time", xirc::format_server_time(time))
            })
            .collect();
        srv.db.store_messages(net.id, "#a", &messages).unwrap();
    }
    // Expiry preserves free pages below the vacuum threshold.
    srv.delete_expired_messages().unwrap();
    let low = srv.db.free_page_ratio().unwrap();
    assert!(low > 0.0 && low < 0.25, "{low}");
    // A repeated expiry run must also preserve free pages below 25%.
    srv.delete_expired_messages().unwrap();
    assert_eq!(srv.db.free_page_ratio().unwrap(), low);

    srv.db
        .delete_messages_before(now + jiff::SignedDuration::from_hours(24), usize::MAX)
        .unwrap();
    let high = srv.db.free_page_ratio().unwrap();
    assert!(high >= 0.25, "{high}");
    // A daily expiry run vacuums above the threshold, even with no new deletions.
    srv.delete_expired_messages().unwrap();
    assert_eq!(srv.db.free_page_ratio().unwrap(), 0.0);
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
async fn create_user_on_auth_concurrently() {
    let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    let mut cfg = ServerConfig::default();
    cfg.basic.enable_users_on_auth = true;
    srv.set_config(cfg);
    srv.start().unwrap();
    // e.g. a client connecting to several networks on first login
    let results =
        futures_util::future::join_all((0..8).map(|_| srv.get_or_create_user("alice"))).await;
    for res in results {
        res.unwrap();
    }
    assert_eq!(srv.db.list_users().unwrap().len(), 1);
    srv.shutdown().await;
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
fn msg_store_not_reloaded() {
    let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    srv.set_config(ServerConfig::default());
    let mut cfg = ServerConfig::default();
    cfg.basic.msg_store.driver = MsgStoreDriver::Memory;
    cfg.basic.title = "reloaded".to_owned();
    srv.reload_config(cfg);
    let cfg = srv.config();
    assert_eq!(cfg.basic.msg_store.driver, MsgStoreDriver::Db);
    assert_eq!(cfg.basic.title, "reloaded");
}

/// A reload changing message-expiry applies it right away, and a user
/// listed by `start()` and created meanwhile runs once.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
async fn expiry_reload_and_duplicate_users() {
    let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    srv.set_config(ServerConfig::default());
    let mut user = db::User::new("alice");
    srv.db.store_user(&mut user).unwrap();
    let mut net = db::Network::new("irc.example.org");
    srv.db.store_network(user.id, &mut net).unwrap();
    let mut old = msg!("PRIVMSG", "#a", "old");
    old.tags.set(
        "time",
        xirc::format_server_time(Timestamp::now() - jiff::SignedDuration::from_hours(72)),
    );
    srv.db.store_messages(net.id, "#a", &[old]).unwrap();

    srv.start().unwrap();
    {
        let mut map = srv.users.lock().unwrap();
        let first = map.get("alice").unwrap().tx.clone();
        let again = srv.add_user_locked(&mut map, user.clone());
        assert!(again.tx.same_channel(&first));
        assert_eq!(map.len(), 1);
    }

    let mut cfg = ServerConfig::default();
    cfg.basic.message_expiry = Duration::from_hours(24);
    srv.reload_config(cfg);
    let opts = db::MessageOptions {
        limit: 10,
        ..Default::default()
    };
    for _ in 0..100 {
        if srv
            .db
            .list_messages(net.id, "#a", &opts)
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        srv.db.list_messages(net.id, "#a", &opts).unwrap(),
        [] as [crate::irc::Message; 0]
    );
    srv.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
async fn user_creation_is_rejected_after_shutdown() {
    let server = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    server.start().unwrap();
    server.shutdown().await;
    assert!(server.create_user(db::User::new("late")).is_err());
    assert!(server.db.get_user("late").unwrap().is_none());
    assert!(server.get_user("late").is_none());
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
async fn concurrent_creation_cannot_escape_the_shutdown_snapshot() {
    for _ in 0..16 {
        let server = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
        server.start().unwrap();
        let creator = server.clone();
        let creation =
            tokio::task::spawn_blocking(move || creator.create_user(db::User::new("racing")));
        server.shutdown().await;
        if let Ok(handle) = creation.await.unwrap() {
            tokio::time::timeout(Duration::from_secs(2), handle.done.cancelled())
                .await
                .expect("a new user was left running after server shutdown");
        }
        assert_eq!(server.user_records(), [] as [crate::database::User; 0]);
    }
}

#[tokio::test]
async fn actor_blocking_admission_is_bounded_and_started_work_is_joined() -> Result<()> {
    use std::sync::atomic::AtomicUsize;
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let released = Arc::new(AtomicBool::new(false));
    let release = Release(released.clone());
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let (released, active, maximum) = (released.clone(), active.clone(), maximum.clone());
        tasks.push(tokio::spawn(blocking(move || {
            let count = active.fetch_add(1, Ordering::Relaxed) + 1;
            maximum.fetch_max(count, Ordering::Relaxed);
            while !released.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            active.fetch_sub(1, Ordering::Relaxed);
        })));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while active.load(Ordering::Relaxed) != 16 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(maximum.load(Ordering::Relaxed), 16);
    drop(release);
    for task in tasks {
        task.await??;
    }
    assert_eq!(active.load(Ordering::Relaxed), 0);
    assert_eq!(maximum.load(Ordering::Relaxed), 16);
    Ok(())
}
