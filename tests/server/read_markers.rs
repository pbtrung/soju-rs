//! Read markers regression scenarios.

use super::*;

pub(super) async fn read_marker() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "draft/read-marker").await;

    dc.send(msg!("MARKREAD", "#rm"));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params, vec!["#rm", "*"]);

    let ts = "timestamp=2024-01-01T00:00:00.000Z";
    dc.send(msg!("MARKREAD", "#RM", ts));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], ts);
    // Read markers only move forward, and targets are casemapped
    dc.send(msg!(
        "MARKREAD",
        "#rm",
        "timestamp=2023-01-01T00:00:00.000Z"
    ));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], ts);
    env.srv.shutdown().await;
}

/// soju.im/unread tags TARGETS with unread text and mentions, relative to
/// the read markers, without the client loading any history.
pub(super) async fn unread_targets() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    let us = Prefix::full(nick.clone(), "u", "h");
    for ch in ["#quiet", "#mention"] {
        uc.send(msg!("JOIN", ch).with_prefix(us.clone()));
        uc.send(msg!("366", nick.clone(), ch, "End of /NAMES list").with_prefix(server_prefix()));
    }
    let bob = Prefix::new("bob");
    uc.send(msg!("PRIVMSG", "#quiet", "hello").with_prefix(bob.clone()));
    uc.send(msg!("PRIVMSG", "#mention", format!("hi {nick}")).with_prefix(bob.clone()));
    uc.send(msg!("PRIVMSG", nick.clone(), "private").with_prefix(bob.clone()));
    // Our own messages don't mention us, nor private notices and CTCP
    uc.send(msg!("PRIVMSG", "#quiet", format!("I am {nick}")).with_prefix(us.clone()));
    uc.send(
        msg!(
            "NOTICE",
            nick.clone(),
            format!("{nick}: you are identified")
        )
        .with_prefix(Prefix::new("chanserv")),
    );
    uc.send(msg!("PRIVMSG", nick.clone(), "\u{1}VERSION\u{1}").with_prefix(Prefix::new("carol")));
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut dc,
        Some(&env.network),
        "batch server-time draft/chathistory draft/read-marker soju.im/unread",
    )
    .await;
    // Mentions are recorded in the background
    let mut targets = Vec::new();
    for _ in 0..100 {
        targets = unread_target_tags(&mut dc).await;
        if targets.iter().filter(|t| t.2).count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        targets,
        [
            ("#mention".to_owned(), true, true),
            ("#quiet".to_owned(), true, false),
            ("bob".to_owned(), true, true),
            ("carol".to_owned(), true, false),
            ("chanserv".to_owned(), true, false),
        ]
    );

    // Read markers clear them
    for target in ["#mention", "bob"] {
        dc.send(msg!(
            "MARKREAD",
            target,
            "timestamp=2100-01-01T00:00:00.000Z"
        ));
        dc.expect("MARKREAD").await;
    }
    assert_eq!(
        unread_target_tags(&mut dc).await,
        [
            ("#mention".to_owned(), false, false),
            ("#quiet".to_owned(), true, false),
            ("bob".to_owned(), false, false),
            ("carol".to_owned(), true, false),
            ("chanserv".to_owned(), true, false),
        ]
    );

    // Not without the capability
    let mut plain = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut plain,
        Some(&env.network),
        "batch server-time draft/chathistory",
    )
    .await;
    assert!(
        unread_target_tags(&mut plain)
            .await
            .iter()
            .all(|t| !t.1 && !t.2)
    );
    env.srv.shutdown().await;
}
