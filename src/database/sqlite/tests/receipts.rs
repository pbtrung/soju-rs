//! Receipts regression scenarios.

use super::*;
use crate::database::{
    Channel, DeliveryReceipt, DeviceCertificate, DuplicateDeviceCertificate, Network, ReadReceipt,
    User, WebPushConfig, WebPushSubscription,
};

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
fn legacy_timestamp_text_remains_readable_and_writes_milliseconds() {
    let db = db();
    let mut user = User::new("timestamps");
    db.store_user(&mut user).unwrap();
    let mut network = Network::new("irc.example.org");
    db.store_network(user.id, &mut network).unwrap();
    let mut receipt = ReadReceipt {
        target: "#time".into(),
        timestamp: Some(Timestamp::UNIX_EPOCH),
        ..Default::default()
    };
    db.store_read_receipt(network.id, &mut receipt).unwrap();
    for (legacy, canonical) in [
        ("0000-01-01T00:00:00Z", "0000-01-01T00:00:00.000Z"),
        ("1969-12-31T23:59:59.999999999Z", "1969-12-31T23:59:59.999Z"),
        ("2024-02-29T12:00:00.123456789Z", "2024-02-29T12:00:00.123Z"),
    ] {
        db.conn()
            .execute(
                "UPDATE ReadReceipt SET timestamp=? WHERE id=?",
                (legacy, receipt.id),
            )
            .unwrap();
        let mut loaded = db.get_read_receipt(network.id, "#time").unwrap().unwrap();
        assert_eq!(loaded.timestamp, xirc::parse_server_time(legacy));
        db.store_read_receipt(network.id, &mut loaded).unwrap();
        let stored: String = db
            .conn()
            .query_row(
                "SELECT timestamp FROM ReadReceipt WHERE id=?",
                [receipt.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, canonical);
        assert_eq!(
            db.get_read_receipt(network.id, "#time")
                .unwrap()
                .unwrap()
                .timestamp,
            xirc::parse_server_time(canonical)
        );
    }
    db.conn()
        .execute(
            "UPDATE ReadReceipt SET timestamp='invalid' WHERE id=?",
            [receipt.id],
        )
        .unwrap();
    assert!(db.get_read_receipt(network.id, "#time").is_err());
}

#[test]
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
fn receipts_targets_webpush_certs() {
    let db = db();
    let mut u = User::new("alice");
    db.store_user(&mut u).unwrap();
    let mut n = Network::new("irc.example.org");
    db.store_network(u.id, &mut n).unwrap();

    let mut rcpts = vec![DeliveryReceipt {
        target: "#a".into(),
        internal_msg_id: "id1".into(),
        ..Default::default()
    }];
    db.store_client_delivery_receipts(n.id, "", &mut rcpts)
        .unwrap();
    db.store_client_delivery_receipts(n.id, "", &mut rcpts)
        .unwrap();
    db.store_client_delivery_receipts(n.id, "phone", &mut rcpts)
        .unwrap();
    assert_eq!(db.list_delivery_receipts(n.id).unwrap().len(), 2);

    assert!(db.get_read_receipt(n.id, "#a").unwrap().is_none());
    let mut r = ReadReceipt {
        target: "#a".into(),
        timestamp: Some(Timestamp::now()),
        ..Default::default()
    };
    db.store_read_receipt(n.id, &mut r).unwrap();
    db.store_read_receipt(n.id, &mut r).unwrap();
    assert_eq!(db.get_read_receipt(n.id, "#a").unwrap().unwrap().id, r.id);

    let mut mt = db.get_message_target(n.id, "#a").unwrap();
    assert_eq!(mt.id, 0);
    mt.pinned = true;
    db.store_message_target(n.id, &mut mt).unwrap();
    assert_eq!(db.list_message_targets(n.id).unwrap(), vec![mt.clone()]);
    mt.pinned = false;
    db.store_message_target(n.id, &mut mt).unwrap();
    assert_eq!(
        db.list_message_targets(n.id).unwrap(),
        [] as [crate::database::MessageTarget; 0]
    );

    let mut cfg = WebPushConfig {
        vapid_public: "pub".into(),
        vapid_private: "priv".into(),
        ..Default::default()
    };
    db.store_web_push_config(&mut cfg).unwrap();
    assert_eq!(db.list_web_push_configs().unwrap(), vec![cfg]);
    let mut sub = WebPushSubscription {
        endpoint: "https://push.example.org/x".into(),
        ..Default::default()
    };
    db.store_web_push_subscription(u.id, 0, &mut sub).unwrap();
    assert_eq!(db.list_web_push_subscriptions(u.id, 0).unwrap().len(), 1);
    assert_eq!(db.list_web_push_networks(u.id).unwrap(), [0]);
    assert_eq!(
        db.list_web_push_subscriptions(u.id, n.id).unwrap(),
        [] as [crate::database::WebPushSubscription; 0]
    );
    db.delete_web_push_subscription(sub.id).unwrap();
    assert_eq!(
        db.list_web_push_subscriptions(u.id, 0).unwrap(),
        [] as [crate::database::WebPushSubscription; 0]
    );
    assert_eq!(db.list_web_push_networks(u.id).unwrap(), [] as [i64; 0]);

    let mut cert = DeviceCertificate {
        label: "laptop".into(),
        fingerprint: vec![7; 64],
        ..Default::default()
    };
    cert.mark_used(Some("127.0.0.1:1234".parse().unwrap()));
    db.store_device_certificate(u.id, &mut cert).unwrap();
    let mut dup = DeviceCertificate {
        fingerprint: vec![7; 64],
        ..Default::default()
    };
    dup.mark_used(None);
    let err = db.store_device_certificate(u.id, &mut dup).unwrap_err();
    assert!(err.is::<DuplicateDeviceCertificate>());
    let (owner, got) = db.get_device_certificate(&[7; 64]).unwrap().unwrap();
    assert_eq!(owner, u.id);
    assert_eq!(got.last_ip, "127.0.0.1");
    assert_eq!(db.list_device_certificates(u.id).unwrap().len(), 1);
    db.delete_device_certificate(u.id, &[7; 64]).unwrap();
    assert!(db.get_device_certificate(&[7; 64]).unwrap().is_none());

    assert_eq!(
        db.stats().unwrap(),
        DatabaseStats {
            users: 1,
            networks: 1,
            channels: 0
        }
    );
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
fn unchanged_records_and_receipts_do_not_write() {
    let db = db();
    let mut user = User::new("cost-test");
    db.store_user(&mut user).unwrap();
    let mut network = Network::new("irc.cost-test");
    db.store_network(user.id, &mut network).unwrap();
    let mut channel = Channel {
        name: "#cost".into(),
        ..Default::default()
    };
    db.store_channel(network.id, &mut channel).unwrap();
    let mut target = db.get_message_target(network.id, "#cost").unwrap();
    db.store_message_target(network.id, &mut target).unwrap();
    let mut receipt = ReadReceipt {
        target: "#cost".into(),
        timestamp: Some(Timestamp::now()),
        ..Default::default()
    };
    db.store_read_receipt(network.id, &mut receipt).unwrap();
    let mut cert = DeviceCertificate {
        label: "cost".into(),
        fingerprint: vec![17; 64],
        last_used: Some(Timestamp::now()),
        ..Default::default()
    };
    db.store_device_certificate(user.id, &mut cert).unwrap();
    let mut delivered = vec![DeliveryReceipt {
        target: "#cost".into(),
        internal_msg_id: "same".into(),
        ..Default::default()
    }];
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    let cursor = delivered[0].id;
    let before: i64 = db
        .conn()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    db.store_user(&mut user).unwrap();
    db.store_user_keep_password(&user).unwrap();
    db.store_network(user.id, &mut network).unwrap();
    db.store_channel(network.id, &mut channel).unwrap();
    db.store_message_target(network.id, &mut target).unwrap();
    db.store_read_receipt(network.id, &mut receipt).unwrap();
    db.store_device_certificate(user.id, &mut cert).unwrap();
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    let after: i64 = db
        .conn()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(delivered[0].id, cursor);
    delivered[0].internal_msg_id = "changed".into();
    delivered.push(DeliveryReceipt {
        target: "#new".into(),
        internal_msg_id: "new".into(),
        ..Default::default()
    });
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    assert_eq!(delivered[0].id, cursor);
    assert_ne!(delivered[1].id, 0);
    delivered.remove(0);
    db.store_client_delivery_receipts(network.id, "", &mut delivered)
        .unwrap();
    assert_eq!(db.list_delivery_receipts(network.id).unwrap().len(), 1);
}
