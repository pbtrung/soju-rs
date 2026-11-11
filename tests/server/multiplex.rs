//! Multiplex regression scenarios.

use super::*;

/// Replies to messages for a network not attached keep their label.
pub(super) async fn multiplex_not_attached_label() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut dc,
        None,
        "batch message-tags labeled-response soju.im/bouncer-networks soju.im/multiplex",
    )
    .await;
    dc.send(
        msg!("PRIVMSG", "#chan", "x")
            .with_tag("soju.im/network", "12345")
            .with_tag("label", "L1"),
    );
    let m = read_until(&mut dc, "FAIL").await;
    assert_eq!(m.tags.get("label"), Some("L1"), "{m}");
    assert_eq!(m.params[..2], ["PRIVMSG", "NETWORK_NOT_ATTACHED"]);
    dc.send(
        msg!("PING", "x")
            .with_tag("soju.im/network", "12345")
            .with_tag("label", "L2"),
    );
    let m = read_until(&mut dc, "ACK").await;
    assert_eq!(m.tags.get("label"), Some("L2"), "{m}");
    env.srv.shutdown().await;
}

pub(super) async fn multiplex() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut dc,
        None,
        "batch cap-notify message-tags soju.im/bouncer-networks soju.im/bouncer-networks-notify soju.im/multiplex",
    )
    .await;
    let net = env.network.id.to_string();

    // The welcome burst of the network comes tagged, like on a bound
    // connection, and without network notifications
    dc.send(msg!("BOUNCER", "ATTACH", net.clone()));
    let welcome = read_until_tagged(&mut dc, "001", &net).await;
    assert_eq!(welcome.params[0], TEST_USERNAME);
    loop {
        let m = dc.read().await;
        assert_eq!(m.tags.get(NETWORK_TAG), Some(net.as_str()), "{m}");
        assert_ne!(m.command, "BOUNCER", "{m}");
        if m.command == "422" {
            break;
        }
    }

    // Tagged messages go to the network, and come back tagged
    dc.send(msg!("PRIVMSG", "#chan", "hello").with_tag(NETWORK_TAG, net.clone()));
    let m = read_until(&mut uc, "PRIVMSG").await;
    assert_eq!(m.params, ["#chan", "hello"]);
    assert!(m.tags.get(NETWORK_TAG).is_none(), "{m}");
    uc.send(
        msg!("PRIVMSG", TEST_USERNAME, "hi there").with_prefix(Prefix::full(
            "alice",
            "alice",
            "example.org",
        )),
    );
    let m = read_until_tagged(&mut dc, "PRIVMSG", &net).await;
    assert_eq!(m.params[1], "hi there");
    dc.send(msg!("PING", "tok").with_tag(NETWORK_TAG, net.clone()));
    let m = read_until_tagged(&mut dc, "PONG", &net).await;
    assert_eq!(m.params.last().unwrap(), "tok");

    // Untagged messages stay on the bouncer connection
    let m = dc.roundtrip().await;
    assert!(m.iter().all(|m| m.tags.get(NETWORK_TAG).is_none()), "{m:?}");

    // Messages for a network which isn't attached are rejected
    dc.send(msg!("PRIVMSG", "#chan", "x").with_tag(NETWORK_TAG, "12345"));
    let m = read_until(&mut dc, "FAIL").await;
    assert_eq!(m.params[..3], ["PRIVMSG", "NETWORK_NOT_ATTACHED", "12345"]);
    dc.send(msg!("BOUNCER", "ATTACH", "12345"));
    let m = read_until(&mut dc, "FAIL").await;
    assert_eq!(m.params[..3], ["BOUNCER", "INVALID_NETID", "ATTACH"]);

    // After DETACH, the network's messages aren't sent anymore
    dc.send(msg!("BOUNCER", "DETACH", net.clone()));
    dc.roundtrip().await;
    uc.send(
        msg!("PRIVMSG", TEST_USERNAME, "missed").with_prefix(Prefix::full(
            "alice",
            "alice",
            "example.org",
        )),
    );
    uc.send(msg!("PING", "sync"));
    read_until(&mut uc, "PONG").await;
    let m = dc.roundtrip().await;
    assert!(m.iter().all(|m| m.tags.get(NETWORK_TAG).is_none()), "{m:?}");
    // A late PING is dropped silently, other commands fail
    dc.send(msg!("PING", "late").with_tag(NETWORK_TAG, net.clone()));
    let m = dc.roundtrip().await;
    assert!(m.is_empty(), "{m:?}");

    // Attached again, then closed by the bouncer when the network goes
    dc.send(msg!("BOUNCER", "ATTACH", net.clone()));
    read_until_tagged(&mut dc, "001", &net).await;
    dc.send(msg!("BOUNCER", "DELNETWORK", net.clone()));
    let m = read_until_tagged(&mut dc, "ERROR", &net).await;
    assert_eq!(m.params[0], "Closing link");
    // The bouncer connection itself stays open
    let m = read_until(&mut dc, "BOUNCER").await;
    assert_eq!(m.params[..3], ["NETWORK", net.as_str(), "*"]);
    dc.roundtrip().await;
    env.srv.shutdown().await;
}

pub(super) async fn multiplex_unsupported() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;
    let net = env.network.id.to_string();

    // Without the cap, ATTACH is unknown and the tag is ignored
    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, None, "soju.im/bouncer-networks").await;
    dc.send(msg!("BOUNCER", "ATTACH", net.clone()));
    let m = read_until(&mut dc, "FAIL").await;
    assert_eq!(m.params[..3], ["BOUNCER", "UNKNOWN_COMMAND", "ATTACH"]);

    // A connection bound to a network can't attach others
    let mut bound = connect_downstream(&env.srv);
    register_downstream_caps(&mut bound, Some(&env.network), "soju.im/multiplex").await;
    bound.send(msg!("BOUNCER", "ATTACH", net));
    let m = read_until(&mut bound, "FAIL").await;
    assert_eq!(m.params[..3], ["BOUNCER", "INVALID_CONNECTION", "ATTACH"]);
    env.srv.shutdown().await;
}
