//! Messages regression scenarios.

use super::*;

#[test]
fn messages() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();

    let m1 = msg!("PRIVMSG", "#a", "hello \x02world\x02")
        .with_prefix(crate::irc::Prefix::new("bob"))
        .with_tag("time", "2024-01-01T00:00:00.000Z");
    let m2 = msg!("JOIN", "#a")
        .with_prefix(crate::irc::Prefix::new("carol"))
        .with_tag("time", "2024-01-01T00:00:01.000Z");
    let m3 = msg!("NOTICE", "#a", "goodbye world")
        .with_prefix(crate::irc::Prefix::new("bob"))
        .with_tag("time", "2024-01-01T00:00:02.000Z");
    let ids = db
        .store_messages(n.id, "#a", &[m1.clone(), m2.clone(), m3.clone()])
        .unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[2]);
    assert_eq!(db.get_message_last_id(n.id, "#b").unwrap(), 0);

    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m1.clone(), m3.clone()]
    );
    let opts = MessageOptions {
        limit: 10,
        events: true,
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 3);
    let opts = MessageOptions {
        limit: 1,
        take_last: true,
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m3.clone()]
    );
    let opts = MessageOptions {
        limit: 10,
        text: "world\"".into(),
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 2);
    // Nothing to search for, rather than an FTS syntax error
    let opts = MessageOptions {
        limit: 10,
        text: "!!!".into(),
        ..Default::default()
    };
    assert!(db.list_messages(n.id, "#a", &opts).unwrap().is_empty());
    let opts = MessageOptions {
        limit: 10,
        text: "hello world".into(),
        sender: "bob".into(),
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m1.clone()]
    );
    let opts = MessageOptions {
        limit: 10,
        after_time: xirc::parse_server_time("2024-01-01T00:00:00.000Z"),
        ..Default::default()
    };
    assert_eq!(
        db.list_messages(n.id, "#a", &opts).unwrap(),
        vec![m3.clone()]
    );

    db.store_messages(
        n.id,
        "#b",
        &[msg!("PRIVMSG", "#b", "x").with_tag("time", "2024-01-01T00:00:05.000Z")],
    )
    .unwrap();
    let opts = MessageOptions {
        limit: 10,
        after_time: xirc::parse_server_time("2023-01-01T00:00:00.000Z"),
        before_time: xirc::parse_server_time("2025-01-01T00:00:00.000Z"),
        ..Default::default()
    };
    let targets = db.list_message_last_per_target(n.id, &opts).unwrap();
    assert_eq!(
        targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["#a", "#b"]
    );
    assert_eq!(
        targets[0].latest_message,
        xirc::parse_server_time("2024-01-01T00:00:02.000Z")
    );

    db.delete_messages_before(
        xirc::parse_server_time("2024-01-01T00:00:01.500Z").unwrap(),
        usize::MAX,
    )
    .unwrap();
    let opts = MessageOptions {
        limit: 10,
        events: true,
        ..Default::default()
    };
    assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap(), vec![m3]);
}

#[test]
fn messages_same_time() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();

    // e.g. the lines of a multiline message
    let msgs: Vec<Message> = (0..5)
        .map(|i| msg!("PRIVMSG", "#a", i.to_string()).with_tag("time", "2024-01-01T00:00:00.000Z"))
        .collect();
    let ids = db.store_messages(n.id, "#a", &msgs).unwrap();
    assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[4]);
    let texts = |opts: &MessageOptions| -> Vec<String> {
        db.list_messages(n.id, "#a", opts)
            .unwrap()
            .into_iter()
            .map(|m| m.params[1].clone())
            .collect()
    };
    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(texts(&opts), ["0", "1", "2", "3", "4"]);
    let opts = MessageOptions {
        limit: 2,
        take_last: true,
        ..Default::default()
    };
    assert_eq!(texts(&opts), ["3", "4"]);
    let opts = MessageOptions {
        after_id: ids[2],
        limit: 10,
        take_last: true,
        ..Default::default()
    };
    assert_eq!(texts(&opts), ["3", "4"]);
}

#[test]
fn message_last_id_insertion_order() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    // The upstream server's clock went backwards
    let m1 = msg!("PRIVMSG", "#a", "x").with_tag("time", "2030-01-01T00:00:00.000Z");
    let m2 = msg!("PRIVMSG", "#a", "y").with_tag("time", "2024-01-01T00:00:00.000Z");
    let ids = db.store_messages(n.id, "#a", &[m1, m2]).unwrap();
    assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[1]);
}

#[test]
fn messages_invalid_utf8() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    db.store_messages(n.id, "#a", &[msg!("PRIVMSG", "#a", "x")])
        .unwrap();

    // Go soju stores raw upstream bytes, which may be e.g. Latin-1
    let raw: &[u8] = b"@time=2024-01-01T00:00:00.000Z :bob PRIVMSG #a :caf\xe9";
    db.conn()
        .execute(
            "INSERT INTO Message(id, target, raw, time, sender, text)
            SELECT 100, id, CAST(?1 AS TEXT), '2024-01-01T00:00:00.000Z', 'bob', 'caf'
            FROM MessageTarget WHERE target = '#a'",
            [raw],
        )
        .unwrap();

    let opts = MessageOptions {
        limit: 10,
        ..Default::default()
    };
    let l = db.list_messages(n.id, "#a", &opts).unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[0].params, vec!["#a", "caf\u{fffd}"]);

    // Channel names and delivery receipt targets come from upstream too
    let name: &[u8] = b"#caf\xe9";
    db.conn()
        .execute(
            "INSERT INTO Channel(network, name) VALUES (?1, CAST(?2 AS TEXT))",
            rusqlite::params![n.id, name],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO DeliveryReceipt(network, target, internal_msgid)
            VALUES (?1, CAST(?2 AS TEXT), 'x')",
            rusqlite::params![n.id, name],
        )
        .unwrap();
    let channels = db.list_channels(n.id).unwrap();
    assert_eq!(channels[0].name, "#caf\u{fffd}");
    let receipts = db.list_delivery_receipts(n.id).unwrap();
    assert_eq!(receipts[0].target, "#caf\u{fffd}");
    assert_eq!(receipts[0].client, "");
}
