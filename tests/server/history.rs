//! History regression scenarios.

use super::*;

pub(super) async fn chat_history_db() {
    chat_history(MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    })
    .await;
}

pub(super) async fn chat_history_params() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    let base = Utc.with_ymd_and_hms(2023, 5, 23, 6, 0, 0).unwrap();
    for i in 0..3 {
        uc.send(
            msg!("PRIVMSG", nick.clone(), format!("msg {i}"))
                .with_prefix(Prefix::new("Foo"))
                .with_tag(
                    "time",
                    xirc::format_server_time(base + chrono::Duration::seconds(i)),
                ),
        );
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut dc,
        Some(&env.network),
        "batch server-time draft/chathistory",
    )
    .await;

    for limit in ["-1", "1001", "abc"] {
        dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", limit));
        let m = dc.expect("FAIL").await;
        assert_eq!(
            m.params,
            [
                "CHATHISTORY",
                "INVALID_PARAMS",
                "LATEST",
                limit,
                "Invalid limit"
            ],
        );
    }

    // Targets are casemapped; LATEST returns the most recent messages
    dc.send(msg!("CHATHISTORY", "LATEST", "FOO", "*", "2"));
    let start = dc.expect("BATCH").await;
    assert_eq!(start.params[1], "chathistory");
    let name = start.params[0].trim_start_matches('+').to_owned();
    let mut texts = Vec::new();
    loop {
        let m = dc.read().await;
        if m.command == "BATCH" {
            assert_eq!(m.params[0], format!("-{name}"));
            break;
        }
        assert_eq!(m.tags.get("batch"), Some(name.as_str()));
        assert!(m.tags.contains("time"), "{m}");
        texts.push(m.params[1].clone());
    }
    assert_eq!(texts, ["msg 1", "msg 2"]);

    // A limit of 0 gives an empty batch
    dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", "0"));
    dc.expect("BATCH").await;
    let end = dc.expect("BATCH").await;
    assert!(end.params[0].starts_with('-'));
    env.srv.shutdown().await;
}

pub(super) async fn upstream_clock_skew() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;

    // The upstream server's clock is an hour ahead of ours
    let ahead = Utc::now() + chrono::Duration::hours(1);
    let ahead_str = xirc::format_server_time(ahead);
    uc.send(
        msg!("PRIVMSG", nick, "from the future")
            .with_prefix(Prefix::new("foo"))
            .with_tag("time", ahead_str.clone()),
    );
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream_caps(&mut dc, Some(&env.network), "draft/read-marker").await;

    // CHATHISTORY isn't bounded by our clock
    for params in [
        vec!["LATEST", "foo", "*", "10"],
        vec!["AFTER", "foo", "timestamp=2020-01-01T00:00:00.000Z", "10"],
    ] {
        dc.send(Message::new(
            "CHATHISTORY",
            params.iter().map(|s| s.to_string()).collect(),
        ));
        let got: Vec<String> = dc
            .roundtrip()
            .await
            .into_iter()
            .map(|m| m.params[1].clone())
            .collect();
        assert_eq!(got, ["from the future"], "{params:?}");
    }

    // Read markers can reach the latest message, but not beyond it
    dc.send(msg!("MARKREAD", "foo", format!("timestamp={ahead_str}")));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], format!("timestamp={ahead_str}"));
    let later = xirc::format_server_time(ahead + chrono::Duration::hours(1));
    dc.send(msg!("MARKREAD", "foo", format!("timestamp={later}")));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], format!("timestamp={ahead_str}"));

    // Without messages in the future, markers stop at our clock
    let before = Utc::now();
    let far = xirc::format_server_time(before + chrono::Duration::days(1));
    dc.send(msg!("MARKREAD", "#empty", format!("timestamp={far}")));
    let m = dc.expect("MARKREAD").await;
    let t = xirc::parse_server_time(m.params[1].strip_prefix("timestamp=").unwrap()).unwrap();
    // Server times have millisecond precision
    let before = before - chrono::Duration::milliseconds(1);
    assert!(t >= before && t <= Utc::now(), "{m}");
    env.srv.shutdown().await;
}

/// A lower chathistory-limit is advertised and caps replies.
pub(super) async fn chat_history_limit() {
    let env = setup(db_store()).await;
    let mut cfg = (*env.srv.config()).clone();
    cfg.basic.chat_history_limit = 2;
    env.srv.set_config(cfg);
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    for i in 0..3 {
        uc.send(msg!("PRIVMSG", nick.clone(), format!("msg {i}")).with_prefix(Prefix::new("foo")));
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    let isupport: Vec<String> = register_downstream_caps(
        &mut dc,
        Some(&env.network),
        "batch server-time draft/chathistory",
    )
    .await
    .into_iter()
    .filter(|m| m.command == "005")
    .flat_map(|m| m.params)
    .collect();
    assert!(
        isupport.iter().any(|p| p == "CHATHISTORY=2"),
        "missing CHATHISTORY=2 in {isupport:?}"
    );

    dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", "1001"));
    let m = dc.expect("FAIL").await;
    assert_eq!(m.params[..2], ["CHATHISTORY", "INVALID_PARAMS"]);

    // Bigger limits are capped, e.g. TARGETS still works with 1000
    dc.send(msg!(
        "CHATHISTORY",
        "TARGETS",
        "timestamp=2000-01-01T00:00:00.000Z",
        "timestamp=2100-01-01T00:00:00.000Z",
        "1000"
    ));
    let m = read_until(&mut dc, "CHATHISTORY").await;
    assert_eq!(m.params[..2], ["TARGETS", "foo"]);
    read_until(&mut dc, "BATCH").await;
    dc.send(msg!("CHATHISTORY", "LATEST", "foo", "*", "1000"));
    let outer = dc.expect("BATCH").await;
    assert_eq!(outer.params[1], "chathistory");
    for want in ["msg 1", "msg 2"] {
        let m = dc.expect("PRIVMSG").await;
        assert_eq!(m.params[1], want);
    }
    dc.expect("BATCH").await;
    env.srv.shutdown().await;
}

async fn chat_history(msg_store: MsgStore) {
    let env = setup(msg_store).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;

    let texts = [
        "Hiya!",
        "How are you doing?",
        "Can I take a sip from your glass of soju?",
    ];
    let base = Utc.with_ymd_and_hms(2023, 5, 23, 6, 0, 0).unwrap();
    let at = |secs: i64| base + chrono::Duration::milliseconds(secs);
    for (i, text) in texts.iter().enumerate() {
        uc.send(
            msg!("PRIVMSG", TEST_USERNAME, *text)
                .with_prefix(Prefix::new("foo"))
                .with_tag("time", xirc::format_server_time(at(i as i64 * 1000))),
        );
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;
    dc.roundtrip().await; // drain post-connection-registration messages

    let n = texts.len() as i64;
    let ts = |ms: i64| format!("timestamp={}", xirc::format_server_time(at(ms)));
    let s = |r: std::ops::Range<usize>| texts[r].iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let cases: Vec<(&str, Vec<String>, usize, Vec<String>)> = vec![
        (
            "before_all",
            vec!["BEFORE".into(), ts(n * 1000)],
            0,
            s(0..3),
        ),
        ("before_none", vec!["BEFORE".into(), ts(0)], 0, vec![]),
        ("before_first", vec!["BEFORE".into(), ts(1000)], 0, s(0..1)),
        (
            "before_with_limit",
            vec!["BEFORE".into(), ts((n - 1) * 1000)],
            1,
            s(1..2),
        ),
        ("after_all", vec!["AFTER".into(), ts(-1000)], 0, s(0..3)),
        (
            "after_none",
            vec!["AFTER".into(), ts((n - 1) * 1000)],
            0,
            vec![],
        ),
        (
            "after_all_but_first",
            vec!["AFTER".into(), ts(0)],
            0,
            s(1..3),
        ),
        ("after_with_limit", vec!["AFTER".into(), ts(0)], 1, s(1..2)),
        (
            "between_all",
            vec!["BETWEEN".into(), ts(-1000), ts(n * 1000)],
            0,
            s(0..3),
        ),
        (
            "between_all_reverse",
            vec!["BETWEEN".into(), ts(n * 1000), ts(-1000)],
            0,
            s(0..3),
        ),
        (
            "between_middle",
            vec!["BETWEEN".into(), ts(0), ts((n - 1) * 1000)],
            0,
            s(1..2),
        ),
        (
            "between_middle_reverse",
            vec!["BETWEEN".into(), ts((n - 1) * 1000), ts(0)],
            0,
            s(1..2),
        ),
        (
            "between_last_two",
            vec!["BETWEEN".into(), ts(0), ts(n * 1000)],
            0,
            s(1..3),
        ),
        (
            "between_last_two_reverse",
            vec!["BETWEEN".into(), ts(n * 1000), ts(0)],
            0,
            s(1..3),
        ),
        (
            "between_first_two",
            vec!["BETWEEN".into(), ts(-1000), ts((n - 1) * 1000)],
            0,
            s(0..2),
        ),
        (
            "between_first_two_reverse",
            vec!["BETWEEN".into(), ts((n - 1) * 1000), ts(-1000)],
            0,
            s(0..2),
        ),
        (
            "between_with_limit",
            vec!["BETWEEN".into(), ts(0), ts(n * 1000)],
            1,
            s(1..2),
        ),
        ("around_all", vec!["AROUND".into(), ts(1000)], 0, s(0..3)),
        (
            "around_with_limit",
            vec!["AROUND".into(), ts(1500)],
            2,
            s(1..3),
        ),
    ];

    for (name, bounds, limit, want) in cases {
        let limit = if limit == 0 {
            "100".to_owned()
        } else {
            limit.to_string()
        };
        let mut params = vec![bounds[0].clone(), "foo".to_owned()];
        params.extend_from_slice(&bounds[1..]);
        params.push(limit);
        dc.send(Message::new("CHATHISTORY", params));
        let got: Vec<String> = dc
            .roundtrip()
            .await
            .into_iter()
            .map(|m| {
                assert_eq!(m.command, "PRIVMSG", "{name}: unexpected reply: {m}");
                m.params[1].clone()
            })
            .collect();
        assert_eq!(got, want, "{name}");
    }
    env.srv.shutdown().await;
}
