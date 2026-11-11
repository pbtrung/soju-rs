use super::*;
use crate::config::MsgStoreDriver;
use crate::{msg, xirc};
use chrono::Utc;

#[test]
fn vacuum_requires_25_percent_free_and_maintenance_request() {
    let srv = Server::new(Database::open_in_memory().unwrap(), Logger::new(false));
    let mut cfg = ServerConfig::default();
    cfg.basic.message_expiry = Duration::from_secs(86400);
    srv.set_config(cfg);
    let mut user = db::User::new("vacuum");
    srv.db.store_user(&mut user).unwrap();
    let mut net = db::Network::new("irc.example.org");
    srv.db.store_network(user.id, &mut net).unwrap();
    let text = "x".repeat(1000);
    let now = Utc::now();
    for (count, time) in [(300, now - chrono::Duration::days(3)), (3700, now)] {
        let messages: Vec<_> = (0..count)
            .map(|_| {
                msg!("PRIVMSG", "#a", text.clone()).with_tag("time", xirc::format_server_time(time))
            })
            .collect();
        srv.db.store_messages(net.id, "#a", &messages).unwrap();
    }
    // Daily expiry frees some pages but does not rewrite the file.
    srv.delete_expired_messages(false).unwrap();
    let low = srv.db.free_page_ratio().unwrap();
    assert!(low > 0.0 && low < 0.25, "{low}");
    // Startup/reload maintenance must preserve free pages below 25%.
    srv.delete_expired_messages(true).unwrap();
    assert_eq!(srv.db.free_page_ratio().unwrap(), low);

    srv.db
        .delete_messages_before(now + chrono::Duration::days(1), usize::MAX)
        .unwrap();
    let high = srv.db.free_page_ratio().unwrap();
    assert!(high >= 0.25, "{high}");
    // Even a mostly empty file is not vacuumed during daily expiry.
    srv.delete_expired_messages(false).unwrap();
    assert_eq!(srv.db.free_page_ratio().unwrap(), high);
    srv.delete_expired_messages(true).unwrap();
    assert_eq!(srv.db.free_page_ratio().unwrap(), 0.0);
}

#[tokio::test(flavor = "multi_thread")]
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
/// listed by start() and created meanwhile runs once.
#[tokio::test(flavor = "multi_thread")]
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
        xirc::format_server_time(Utc::now() - chrono::Duration::days(3)),
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
    cfg.basic.message_expiry = Duration::from_secs(86400);
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
    assert!(
        srv.db
            .list_messages(net.id, "#a", &opts)
            .unwrap()
            .is_empty()
    );
    srv.shutdown().await;
}
