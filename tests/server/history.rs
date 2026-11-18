//! History regression scenarios.

use super::*;

pub(super) async fn chat_history_db() {
    chat_history(MsgStore {
        driver: MsgStoreDriver::Db,
        source: String::new(),
    })
    .await;
}

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
#[expect(
    clippy::indexing_slicing,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
pub(super) async fn chat_history_params() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;
    let base = "2023-05-23T06:00:00Z".parse::<Timestamp>().unwrap();
    for i in 0..3 {
        uc.send(
            msg!("PRIVMSG", nick.clone(), format!("msg {i}"))
                .with_prefix(Prefix::new("Foo"))
                .with_tag(
                    "time",
                    xirc::format_server_time(base + jiff::SignedDuration::from_secs(i)),
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

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
#[expect(
    clippy::indexing_slicing,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
pub(super) async fn upstream_clock_skew() {
    let env = setup(db_store()).await;
    let mut uc = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut uc, &[]).await;

    // The upstream server's clock is an hour ahead of ours
    let ahead = Timestamp::now() + jiff::SignedDuration::from_hours(1);
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
            params
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
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
    let later = xirc::format_server_time(ahead + jiff::SignedDuration::from_hours(1));
    dc.send(msg!("MARKREAD", "foo", format!("timestamp={later}")));
    let m = dc.expect("MARKREAD").await;
    assert_eq!(m.params[1], format!("timestamp={ahead_str}"));

    // Without messages in the future, markers stop at our clock
    let before = Timestamp::now();
    let far = xirc::format_server_time(before + jiff::SignedDuration::from_hours(24));
    dc.send(msg!("MARKREAD", "#empty", format!("timestamp={far}")));
    let m = dc.expect("MARKREAD").await;
    let t = xirc::parse_server_time(m.params[1].strip_prefix("timestamp=").unwrap()).unwrap();
    // Server times have millisecond precision
    let before = before - jiff::SignedDuration::from_millis(1);
    assert!(t >= before && t <= Timestamp::now(), "{m}");
    env.srv.shutdown().await;
}

/// A lower chathistory-limit is advertised and caps replies.
#[expect(
    clippy::indexing_slicing,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
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

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
#[expect(
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
async fn chat_history(msg_store: MsgStore) {
    let env = setup(msg_store).await;
    let mut uc = accept_upstream(&env).await;
    register_upstream(&mut uc).await;

    let texts = [
        "Hiya!",
        "How are you doing?",
        "Can I take a sip from your glass of soju?",
    ];
    let base = "2023-05-23T06:00:00Z".parse::<Timestamp>().unwrap();
    let at = |secs: i64| base + jiff::SignedDuration::from_millis(secs);
    for (i, text) in texts.iter().enumerate() {
        uc.send(
            msg!("PRIVMSG", TEST_USERNAME, *text)
                .with_prefix(Prefix::new("foo"))
                .with_tag(
                    "time",
                    xirc::format_server_time(at(i64::try_from(i).unwrap() * 1000)),
                ),
        );
    }
    uc.roundtrip().await;

    let mut dc = connect_downstream(&env.srv);
    register_downstream(&mut dc, &env.network).await;
    dc.roundtrip().await; // drain post-connection-registration messages

    let n = i64::try_from(texts.len()).unwrap();
    let ts = |ms: i64| format!("timestamp={}", xirc::format_server_time(at(ms)));
    let s = |r: std::ops::Range<usize>| {
        texts[r]
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
    };
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

/// Timestamp ties, empty histories and service shortcuts must keep their wire
/// behavior when request parsing and store execution are separated.
#[expect(
    clippy::unwrap_used,
    reason = "Fixture timestamps must fail this scenario if invalid."
)]
pub(super) async fn history_ties_and_shortcuts() {
    let env = setup(db_store()).await;
    let mut upstream = accept_upstream(&env).await;
    let nick = register_upstream_with(&mut upstream, &[]).await;
    let base = "2023-05-23T06:00:00Z".parse::<Timestamp>().unwrap();
    for (offset, text) in [(0, "old"), (1, "tie-a"), (1, "tie-b"), (2, "new")] {
        upstream.send(
            msg!("PRIVMSG", nick.clone(), text)
                .with_prefix(Prefix::new("Foo"))
                .with_tag(
                    "time",
                    xirc::format_server_time(base + jiff::SignedDuration::from_secs(offset)),
                ),
        );
    }
    upstream.roundtrip().await;
    let mut downstream = connect_downstream(&env.srv);
    register_downstream_caps(
        &mut downstream,
        Some(&env.network),
        "batch server-time draft/chathistory",
    )
    .await;
    let bound = |offset| {
        format!(
            "timestamp={}",
            xirc::format_server_time(base + jiff::SignedDuration::from_secs(offset))
        )
    };
    let (old, tied, new) = (bound(0), bound(1), bound(2));
    let cases: &[(&[&str], &[&str])] = &[
        (&["BEFORE", "FOO", &new, "2"], &["tie-a", "tie-b"]),
        (&["AFTER", "foo", &old, "2"], &["tie-a", "tie-b"]),
        (&["AROUND", "foo", &tied, "3"], &["old", "tie-a"]),
        (&["BETWEEN", "foo", &tied, &tied, "10"], &[]),
        (&["LATEST", "#empty", "*", "10"], &[]),
        (&["LATEST", "BouncerServ", "invalid", "invalid"], &[]),
    ];
    for (params, expected) in cases {
        downstream.send(Message::new(
            "CHATHISTORY",
            params.iter().map(|param| (*param).to_owned()).collect(),
        ));
        let texts = read_history_batch(&mut downstream).await;
        assert_eq!(texts, *expected, "{params:?}");
    }
    let mut bouncer = connect_downstream(&env.srv);
    register_downstream_caps(&mut bouncer, None, "batch draft/chathistory").await;
    bouncer.send(msg!("CHATHISTORY", "TARGETS"));
    let begin = bouncer.expect("BATCH").await;
    assert_eq!(begin.param(1), "draft/chathistory-targets");
    let end = bouncer.expect("BATCH").await;
    assert_eq!(
        end.param(0),
        format!("-{}", begin.param(0).trim_start_matches('+'))
    );
    env.srv.shutdown().await;
}

async fn read_history_batch(downstream: &mut TestConn) -> Vec<String> {
    let begin = downstream.expect("BATCH").await;
    assert_eq!(begin.param(1), "chathistory");
    let batch = begin.param(0).trim_start_matches('+').to_owned();
    let mut texts = Vec::new();
    loop {
        let reply = downstream.read().await;
        if reply.command == "BATCH" {
            assert_eq!(reply.param(0), format!("-{batch}"));
            return texts;
        }
        assert_eq!(reply.command, "PRIVMSG");
        assert_eq!(reply.tags.get("batch"), Some(batch.as_str()));
        texts.push(reply.param(1).to_owned());
    }
}
