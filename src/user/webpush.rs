//! Web Push notifications for a user's networks.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;

use super::User;
use crate::database as db;
use crate::irc::Message;
use crate::logf;
use crate::logger::Logger;
use crate::server::Server;

impl User {
    /// Broadcasts a Web Push notification for a message, in the background.
    pub fn broadcast_web_push(&self, ni: usize, msg: Message) {
        let net = &self.networks[ni];
        spawn_web_push_broadcast(
            self.srv.clone(),
            self.record.id,
            net.id(),
            net.logger.clone(),
            msg,
        );
    }
}

pub fn spawn_web_push_broadcast(
    srv: Arc<Server>,
    user_id: i64,
    net_id: i64,
    logger: Logger,
    msg: Message,
) {
    tokio::spawn(async move {
        broadcast_web_push(srv, user_id, net_id, logger, msg).await;
    });
}

/// Sends a Web Push notification to all of a network's subscriptions.
pub async fn broadcast_web_push(
    srv: Arc<Server>,
    user_id: i64,
    net_id: i64,
    logger: Logger,
    msg: Message,
) {
    let Some(sender) = srv.web_push() else { return };
    let subs = db::run(&srv.db, move |db| {
        db.list_web_push_subscriptions(user_id, net_id)
    })
    .await;
    let subs = match subs {
        Ok(s) => s,
        Err(e) => {
            logf!(logger, "failed to list Web push subscriptions: {e}");
            return;
        }
    };
    for sub in subs {
        let res = send_web_push(&sender, &sub, &msg).await;
        match res {
            Ok(()) => {}
            Err(e) if e.is::<crate::webpush::SubscriptionExpired>() => {
                let id = sub.id;
                if let Err(e) =
                    db::run(&srv.db, move |db| db.delete_web_push_subscription(id)).await
                {
                    logf!(
                        logger,
                        "failed to delete expired Web Push subscription {:?}: {e}",
                        sub.endpoint
                    );
                } else {
                    crate::debugf!(
                        logger,
                        "deleted expired Web Push subscription {:?}",
                        sub.endpoint
                    );
                }
            }
            Err(e) => {
                logf!(
                    logger,
                    "failed to send Web push notification to endpoint {:?}: {e}",
                    sub.endpoint
                );
                // If it failed for any reason and is old, delete it
                let old = sub
                    .updated_at
                    .is_none_or(|t| Utc::now() - t > chrono::Duration::days(30));
                if old {
                    let id = sub.id;
                    if let Err(e) =
                        db::run(&srv.db, move |db| db.delete_web_push_subscription(id)).await
                    {
                        logf!(
                            logger,
                            "failed to delete pruned Web Push subscription {:?}: {e}",
                            sub.endpoint
                        );
                    } else {
                        logf!(
                            logger,
                            "deleted pruned Web Push subscription {:?}",
                            sub.endpoint
                        );
                    }
                }
            }
        }
    }
}

pub async fn send_web_push(
    sender: &crate::webpush::Sender,
    sub: &db::WebPushSubscription,
    msg: &Message,
) -> Result<()> {
    let urgency = match msg.command.as_str() {
        "PRIVMSG" | "NOTICE" | "INVITE" => crate::webpush::Urgency::High,
        _ => crate::webpush::Urgency::Normal,
    };
    sender
        .send(
            &sub.endpoint,
            &sub.keys.auth,
            &sub.keys.p256dh,
            &sub.keys.vapid,
            msg.to_string().as_bytes(),
            urgency,
        )
        .await
}
