//! Storage regression scenarios.

use super::*;

#[test]
fn message_ids_not_reused() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let m = msg!("PRIVMSG", "#a", "x").with_tag("time", "2024-01-01T00:00:00.000Z");
    let after = |id| MessageOptions {
        after_id: id,
        limit: 10,
        ..Default::default()
    };

    // Expiry empties the table
    let id1 = db
        .store_messages(n.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    assert_eq!(
        db.delete_messages_before(Utc::now(), usize::MAX).unwrap(),
        1
    );
    let id2 = db
        .store_messages(n.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    assert!(id2 > id1);
    assert_eq!(db.list_messages(n.id, "#a", &after(id1)).unwrap().len(), 1);

    // Deleting a network or a user removes the highest IDs
    let mut n2 = Network::new("irc.example.com");
    db.store_network(u.id, &mut n2).unwrap();
    let id3 = db
        .store_messages(n2.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    db.delete_network(n2.id).unwrap();
    let id4 = db
        .store_messages(n.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    assert!(id4 > id3);
    let mut u2 = User::new("bob");
    db.store_user(&mut u2).unwrap();
    let mut n3 = Network::new("irc.example.org");
    db.store_network(u2.id, &mut n3).unwrap();
    let id5 = db
        .store_messages(n3.id, "#a", std::slice::from_ref(&m))
        .unwrap()[0];
    db.delete_user(u2.id).unwrap();
    let id6 = db.store_messages(n.id, "#a", &[m]).unwrap()[0];
    assert!(id6 > id5);
}

#[test]
fn delete_messages_in_chunks() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let old = msg!("PRIVMSG", "#a", "x").with_tag("time", "2024-01-01T00:00:00.000Z");
    let new = msg!("PRIVMSG", "#a", "y").with_tag("time", "2030-01-01T00:00:00.000Z");
    let mut msgs = vec![old; 5];
    msgs.push(new);
    db.store_messages(n.id, "#a", &msgs).unwrap();
    let before = xirc::parse_server_time("2025-01-01T00:00:00.000Z").unwrap();
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 2);
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 2);
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 1);
    assert_eq!(db.delete_messages_before(before, 2).unwrap(), 0);
    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 1);
}

#[test]
fn delete_messages_vacuum() {
    let dir = std::env::temp_dir().join(format!("soju-rs-vacuum-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.db");
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let text = "x".repeat(1000);
    let msgs: Vec<Message> = (0..2000)
        .map(|_| msg!("PRIVMSG", "#a", text.as_str()).with_tag("time", "2024-01-01T00:00:00.000Z"))
        .collect();
    db.store_messages(n.id, "#a", &msgs).unwrap();
    let size = || std::fs::metadata(&path).unwrap().len();
    db.conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let before = size();

    assert!(db.free_page_ratio().unwrap() < 0.05);
    assert_eq!(
        db.delete_messages_before(Utc::now(), usize::MAX).unwrap(),
        2000
    );
    assert!(db.free_page_ratio().unwrap() > 0.5);
    db.optimize().unwrap();
    db.vacuum().unwrap();
    db.shrink_memory().unwrap();
    assert_eq!(db.free_page_ratio().unwrap(), 0.0);
    assert!(size() < before / 2, "{} >= {before} / 2", size());
    let wal = std::fs::metadata(dir.join("main.db-wal")).map_or(0, |m| m.len());
    assert_eq!(wal, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_message_batch_rolls_back_rows_targets_and_id_reservations() {
    let db = db();
    let mut user = User::new("atomic");
    db.store_user(&mut user).unwrap();
    let mut network = Network::new("irc.example.org");
    db.store_network(user.id, &mut network).unwrap();
    let first = msg!("PRIVMSG", "#a", "valid").with_tag("time", "2024-01-01T00:00:00.000Z");
    let invalid = msg!("PRIVMSG", "#a", "invalid").with_tag("time", "not-a-time");
    assert!(
        db.store_messages(network.id, "#a", &[first.clone(), invalid])
            .is_err()
    );
    assert_eq!(db.get_message_target(network.id, "#a").unwrap().id, 0);
    assert_eq!(db.get_message_last_id(network.id, "#a").unwrap(), 0);
    let ids = db.store_messages(network.id, "#a", &[first]).unwrap();
    assert_eq!(ids, [1]);
}

#[test]
fn message_id_exhaustion_is_atomic() {
    let db = db();
    let mut user = User::new("ids");
    db.store_user(&mut user).unwrap();
    let mut network = Network::new("irc.example.org");
    db.store_network(user.id, &mut network).unwrap();
    db.conn()
        .execute(
            "INSERT INTO MessageIdHighWater(id,last_id) VALUES(0,?)",
            [i64::MAX - 1],
        )
        .unwrap();
    let message = msg!("PRIVMSG", "#a", "last").with_tag("time", "2024-01-01T00:00:00.000Z");
    assert_eq!(
        db.store_messages(network.id, "#a", std::slice::from_ref(&message))
            .unwrap(),
        [i64::MAX]
    );
    assert!(db.store_messages(network.id, "#b", &[message]).is_err());
    assert_eq!(db.get_message_target(network.id, "#b").unwrap().id, 0);
    assert_eq!(db.get_message_last_id(network.id, "#a").unwrap(), i64::MAX);
    let high: i64 = db
        .conn()
        .query_row("SELECT last_id FROM MessageIdHighWater", [], |r| r.get(0))
        .unwrap();
    assert_eq!(high, i64::MAX);
}

#[test]
fn expiry_rejects_zero_chunks_and_honors_preexisting_cancellation() {
    let db = db();
    assert!(
        db.expire_messages(Utc::now(), 0, Duration::ZERO, &|| false)
            .is_err()
    );
    assert_eq!(
        db.expire_messages(Utc::now(), 1, Duration::ZERO, &|| true)
            .unwrap(),
        0
    );
}
