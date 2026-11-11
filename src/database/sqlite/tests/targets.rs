//! Targets regression scenarios.

use super::*;

#[test]
fn target_unread_and_mentions() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    let t = |s: &str| xirc::parse_server_time(s).unwrap();
    let at = |s: &str| msg!("PRIVMSG", "#a", "hi").with_tag("time", s);
    // A mention may be recorded before its message is stored.
    // Recording it does not create the target.
    db.store_message_target_highlight(n.id, "bob", t("2024-01-01T00:00:03.000Z"))
        .unwrap();
    db.store_messages(n.id, "#a", &[at("2024-01-01T00:00:01.000Z")])
        .unwrap();
    db.store_messages(n.id, "#b", &[at("2024-01-01T00:00:02.000Z")])
        .unwrap();
    db.store_messages(
        n.id,
        "#events",
        &[msg!("JOIN", "#events").with_tag("time", "2024-01-01T00:00:02.000Z")],
    )
    .unwrap();
    db.store_message_target_highlight(n.id, "#a", t("2024-01-01T00:00:01.000Z"))
        .unwrap();
    // Only a later mention moves it
    db.store_message_target_highlight(n.id, "#a", t("2023-01-01T00:00:00.000Z"))
        .unwrap();
    db.store_messages(n.id, "bob", &[at("2024-01-01T00:00:03.000Z")])
        .unwrap();

    let state = |db: &Database, events: bool| {
        let opts = MessageOptions {
            limit: 10,
            events,
            ..Default::default()
        };
        let mut l = db
            .list_message_last_per_target(n.id, &opts)
            .unwrap()
            .into_iter()
            .map(|t| (t.name.clone(), t.unread(), t.mention()))
            .collect::<Vec<_>>();
        l.sort();
        l
    };
    // Events don't make a target unread, with or without event playback
    for events in [false, true] {
        assert_eq!(
            state(&db, events),
            [
                ("#a".to_owned(), true, true),
                ("#b".to_owned(), true, false),
                ("#events".to_owned(), false, false),
                ("bob".to_owned(), true, true),
            ]
        );
    }

    for (target, time) in [
        ("#a", "2024-01-01T00:00:01.000Z"),
        ("#b", "2024-01-01T00:00:01.500Z"),
    ] {
        db.store_read_receipt(
            n.id,
            &mut ReadReceipt {
                target: target.into(),
                timestamp: Some(t(time)),
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        state(&db, false),
        [
            ("#a".to_owned(), false, false),
            ("#b".to_owned(), true, false),
            ("#events".to_owned(), false, false),
            ("bob".to_owned(), true, true),
        ]
    );

    // Deleting the network deletes its mentions
    db.delete_network(n.id).unwrap();
    let left: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM TargetHighlight", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn flagging_an_existing_target() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();
    // Storing messages creates the target, unknown to the caller
    db.store_messages(n.id, "bob", &[msg!("PRIVMSG", "alice", "spam")])
        .unwrap();
    let mut mt = MessageTarget {
        target: "bob".into(),
        blocked: true,
        ..Default::default()
    };
    db.store_message_target(n.id, &mut mt).unwrap();
    let stored = db.get_message_target(n.id, "bob").unwrap();
    assert_eq!(stored.id, mt.id);
    assert!(stored.blocked);
}
