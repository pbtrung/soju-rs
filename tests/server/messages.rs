//! Messages regression scenarios.

use super::*;

pub(super) async fn multiline() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_caps(
        &mut uc,
        "batch echo-message labeled-response message-tags server-time \
            draft/multiline=max-bytes=4096,max-lines=4",
    )
    .await;

    let mut ml = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut ml,
        Some(&env.network),
        "batch echo-message labeled-response message-tags draft/chathistory draft/multiline",
    )
    .await;
    let mut plain = connect_downstream(&env.srv);
    register_downstream_caps(&mut plain, Some(&env.network), "server-time").await;

    ml.send(msg!("CAP", "LIST"));
    let m = ml.expect("CAP").await;
    assert!(
        m.params[2].split(' ').any(|c| c == "draft/multiline"),
        "{m}"
    );

    // A multiline message from the upstream server
    for line in [
        "@msgid=m1 :bob!b@h BATCH +x draft/multiline #c",
        "@batch=x :bob!b@h PRIVMSG #c hello",
        "@batch=x;draft/multiline-concat :bob!b@h PRIVMSG #c :\u{20}world",
        "@batch=x :bob!b@h PRIVMSG #c :",
        "@batch=x :bob!b@h PRIVMSG #c bye",
        "BATCH -x",
    ] {
        uc.send(Message::parse(line).unwrap());
    }
    let (opener, lines) = read_multiline(&mut ml).await;
    assert_eq!(opener.params[2], "#c");
    assert_eq!(opener.prefix_name(), "bob");
    assert_eq!(opener.tags.get("msgid"), Some("m1"));
    let texts: Vec<&str> = lines.iter().map(|m| m.params[1].as_str()).collect();
    assert_eq!(texts, ["hello world", "", "bye"]);
    // Clients without draft/multiline get separate messages
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "hello world"]);
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "bye"]);

    // A multiline message sent by a client, echoed by the upstream server
    for line in [
        "@label=l1;+draft/reply=m1 BATCH +a draft/multiline #c",
        "@batch=a PRIVMSG #c one",
        "@batch=a PRIVMSG #c two",
        "BATCH -a",
    ] {
        ml.send(Message::parse(line).unwrap());
    }
    let (opener, lines) = read_multiline(&mut uc).await;
    let label = opener.tags.get("label").unwrap().to_owned();
    assert_eq!(opener.tags.get("+draft/reply"), Some("m1"));
    let texts: Vec<&str> = lines.iter().map(|m| m.params[1].as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
    assert!(lines.iter().all(|m| m.tags.len() == 1), "{lines:?}");

    let us = format!("{nick}!u@h");
    for line in [
        format!("@label={label};msgid=m2;+draft/reply=m1 :{us} BATCH +e draft/multiline #c"),
        format!("@batch=e :{us} PRIVMSG #c one"),
        format!("@batch=e :{us} PRIVMSG #c two"),
        "BATCH -e".to_owned(),
    ] {
        uc.send(Message::parse(&line).unwrap());
    }
    // The echo is the labeled response, not wrapped in another batch
    let (opener, lines) = read_multiline(&mut ml).await;
    assert_eq!(opener.tags.get("label"), Some("l1"));
    assert_eq!(opener.tags.get("msgid"), Some("m2"));
    assert_eq!(lines.len(), 2);
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "one"]);
    let m = read_until(&mut plain, "PRIVMSG").await;
    assert_eq!(m.params, ["#c", "two"]);

    // Each multiline message is a single history entry
    ml.send(msg!("CHATHISTORY", "LATEST", "#c", "*", "10"));
    let outer = read_until(&mut ml, "BATCH").await;
    assert_eq!(outer.params[1], "chathistory");
    let outer_ref = outer.params[0].strip_prefix('+').unwrap().to_owned();
    for want in [vec!["hello world", "", "bye"], vec!["one", "two"]] {
        let (opener, lines) = read_multiline(&mut ml).await;
        assert_eq!(opener.tags.get("batch"), Some(outer_ref.as_str()));
        let texts: Vec<&str> = lines.iter().map(|m| m.params[1].as_str()).collect();
        assert_eq!(texts, want);
    }
    let m = ml.expect("BATCH").await;
    assert_eq!(m.params[0], format!("-{outer_ref}"));

    // Limits are enforced
    ml.send(Message::parse("@label=l2 BATCH +b draft/multiline #c").unwrap());
    for _ in 0..5 {
        ml.send(Message::parse("@batch=b PRIVMSG #c x").unwrap());
    }
    ml.send(msg!("BATCH", "-b"));
    let m = ml.expect("FAIL").await;
    assert_eq!(m.tags.get("label"), Some("l2"));
    assert_eq!(m.params[..3], ["BATCH", "MULTILINE_MAX_LINES", "4"]);
    env.srv.shutdown().await;
}

/// Messages from a user blocked with METADATA are hidden, from the flags
/// the user's event loop keeps without querying the database per message.
pub(super) async fn blocked_users() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    // A first message stores the target before it's blocked
    uc.send(msg!("PRIVMSG", nick.clone(), "first").with_prefix(Prefix::new("spammer")));
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "draft/metadata-2").await;
    dc.send(msg!("METADATA", "spammer", "SET", "soju.im/blocked", "1"));
    let m = read_until(&mut dc, "761").await; // RPL_KEYVALUE
    assert_eq!(m.params[1..], ["spammer", "soju.im/blocked", "*", "1"]);
    read_until(&mut dc, "METADATA").await;

    uc.send(msg!("PRIVMSG", nick.clone(), "spam").with_prefix(Prefix::new("SPAMMER")));
    uc.send(msg!("PRIVMSG", nick.clone(), "hello").with_prefix(Prefix::new("friend")));
    let m = read_until(&mut dc, "PRIVMSG").await;
    assert_eq!(m.params[1], "hello");

    // Invites obey the same block flag, including case-mapped sources.
    uc.send(msg!("INVITE", nick.clone(), "#spam").with_prefix(Prefix::new("SPAMMER")));
    uc.send(msg!("INVITE", nick.clone(), "#friend").with_prefix(Prefix::new("friend")));
    let m = read_until(&mut dc, "INVITE").await;
    assert_eq!(m.params[1], "#friend");

    // Unblocking shows them again
    dc.send(msg!("METADATA", "spammer", "SET", "soju.im/blocked", "0"));
    read_until(&mut dc, "METADATA").await;
    uc.send(msg!("PRIVMSG", nick.clone(), "back").with_prefix(Prefix::new("spammer")));
    let m = read_until(&mut dc, "PRIVMSG").await;
    assert_eq!(m.params[1], "back");
    uc.send(msg!("INVITE", nick, "#back").with_prefix(Prefix::new("spammer")));
    assert_eq!(read_until(&mut dc, "INVITE").await.params[1], "#back");
    env.srv.shutdown().await;
}
