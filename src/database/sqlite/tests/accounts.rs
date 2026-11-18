//! Accounts regression scenarios.

use super::*;
use crate::database::{Channel, MessageFilter, Network, User};

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
#[expect(
    clippy::indexing_slicing,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
fn users() {
    let db = db();
    let mut u = User::new("alice");
    u.set_password("pw").unwrap();
    db.store_user(&mut u).unwrap();
    assert_ne!(u.id, 0);
    let mut got = db.get_user("alice").unwrap().unwrap();
    assert_eq!(got, u);
    got.nick = "Alice".into();
    got.downstream_interacted_at = Some(Timestamp::now());
    db.store_user(&mut got).unwrap();
    let users = db.list_users().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].nick, "Alice");
    assert!(users[0].downstream_interacted_at.is_some());
    assert_eq!(
        db.get_username_by_id(u.id).unwrap().as_deref(),
        Some("alice")
    );
    assert!(db.get_user("bob").unwrap().is_none());
    let inactive = db
        .list_inactive_usernames(Timestamp::now() + jiff::SignedDuration::from_hours(1))
        .unwrap();
    assert_eq!(inactive, vec!["alice"]);
    // Disabled users are already inactive
    let mut disabled = got.clone();
    disabled.enabled = false;
    db.store_user(&mut disabled).unwrap();
    assert_eq!(
        db.list_inactive_usernames(Timestamp::now() + jiff::SignedDuration::from_hours(1))
            .unwrap(),
        [] as [std::string::String; 0]
    );
    assert_eq!(
        db.list_inactive_usernames(Timestamp::now() - jiff::SignedDuration::from_hours(1))
            .unwrap(),
        [] as [std::string::String; 0]
    );
    db.delete_user(u.id).unwrap();
    assert_eq!(db.list_users().unwrap(), [] as [crate::database::User; 0]);
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
#[expect(
    clippy::indexing_slicing,
    reason = "Fixture setup and assertions use checked test data; any failure must fail this scenario."
)]
fn networks_channels() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    n.connect_commands = vec!["PRIVMSG a :b".into(), "JOIN #x".into()];
    n.sasl.mechanism = "EXTERNAL".into();
    n.sasl.external_cert = Some(vec![1, 2, 3]);
    n.sasl.external_key = Some(vec![4]);
    db.store_network(u.id, &mut n).unwrap();
    let nets = db.list_networks(u.id).unwrap();
    assert_eq!(nets, vec![n.clone()]);
    n.sasl.mechanism = "PLAIN".into();
    n.sasl.plain_username = "acc".into();
    db.store_network(u.id, &mut n).unwrap();
    let nets = db.list_networks(u.id).unwrap();
    assert_eq!(nets[0].sasl.external_cert, None);
    assert_eq!(nets[0].sasl.plain_username, "acc");

    let mut ch = Channel {
        name: "#soju".into(),
        detach_after: Duration::from_millis(1500),
        relay_detached: MessageFilter::Message,
        ..Default::default()
    };
    db.store_channel(n.id, &mut ch).unwrap();
    let chs = db.list_channels(n.id).unwrap();
    assert_eq!(chs.len(), 1);
    assert_eq!(chs[0].detach_after, Duration::from_secs(2));
    assert_eq!(chs[0].relay_detached, MessageFilter::Message);
    db.delete_channel(ch.id).unwrap();
    assert_eq!(
        db.list_channels(n.id).unwrap(),
        [] as [crate::database::Channel; 0]
    );
    db.delete_network(n.id).unwrap();
    assert_eq!(
        db.list_networks(u.id).unwrap(),
        [] as [crate::database::Network; 0]
    );
}
