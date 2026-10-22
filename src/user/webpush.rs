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
        let (srv, user_id, net_id) = (self.srv.clone(), self.record.id, net.id());
        let logger = net.logger.clone();
        tokio::spawn(async move {
            broadcast_web_push(srv, user_id, net_id, logger, msg).await;
        });
    }
}

/// Sends a Web Push notification to all of a network's subscriptions.
async fn broadcast_web_push(
    srv: Arc<Server>,
    user_id: i64,
    net_id: i64,
    logger: Logger,
    msg: Message,
) {
    let subs = web_push_subscriptions(&srv, user_id, net_id, &logger).await;
    send_web_push_to(&srv, subs, &logger, &msg).await;
}

/// Lists a network's Web Push subscriptions. Errors are logged, and give no
/// subscriptions.
pub async fn web_push_subscriptions(
    srv: &Arc<Server>,
    user_id: i64,
    net_id: i64,
    logger: &Logger,
) -> Vec<db::WebPushSubscription> {
    if srv.web_push().is_none() {
        return Vec::new();
    }
    let subs = db::run(&srv.db, move |db| {
        db.list_web_push_subscriptions(user_id, net_id)
    })
    .await;
    subs.unwrap_or_else(|e| {
        logf!(logger, "failed to list Web push subscriptions: {e}");
        Vec::new()
    })
}

/// Sends a Web Push notification to subscriptions, deleting the expired
/// ones.
pub async fn send_web_push_to(
    srv: &Arc<Server>,
    subs: Vec<db::WebPushSubscription>,
    logger: &Logger,
    msg: &Message,
) {
    let Some(sender) = srv.web_push() else { return };
    for sub in subs {
        let res = send_web_push(&sender, &sub, msg).await;
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
