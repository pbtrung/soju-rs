//! Web Push notifications for a user's networks.

use std::sync::Arc;

use anyhow::Result;
use jiff::Timestamp;

use super::User;
use crate::database as db;
use crate::irc::Message;
use crate::logf;
use crate::logger::Logger;
use crate::server::Server;

impl User {
    pub(crate) fn notification_stop(&self) -> tokio_util::sync::CancellationToken {
        self.notifications.stopped()
    }

    /// Whether a network (0 for the bouncer connection) may have Web Push
    /// subscriptions. Most users have none: this avoids sending
    /// notifications in the background just to find out.
    pub fn has_web_push_subscriptions(&mut self, net_id: i64) -> bool {
        if self.srv.web_push().is_none() {
            return false;
        }
        if self.web_push_networks.is_none() {
            match self.srv.db.list_web_push_networks(self.record.id) {
                Ok(l) => self.web_push_networks = Some(l.into_iter().collect()),
                Err(e) => {
                    logf!(self.logger, "failed to list Web push subscriptions: {e}");
                    return true;
                }
            }
        }
        self.web_push_networks
            .as_ref()
            .is_some_and(|n| n.contains(&net_id))
    }

    /// Forgets the networks with Web Push subscriptions, after adding or
    /// deleting some.
    pub fn reset_web_push_networks(&mut self) {
        self.web_push_networks = None;
    }

    /// Admits a delayed message or immediate read-marker notification without
    /// spawning per-message work. Queue overflow is counted and never blocks IRC.
    pub fn queue_web_push(
        &mut self,
        ni: usize,
        target: String,
        msg: Message,
        mention: Option<Timestamp>,
    ) {
        let Some(net) = self.networks.get(ni) else {
            return;
        };
        let network = net.id();
        if !self.has_web_push_subscriptions(network) {
            return;
        }
        let Some(net) = self.networks.get(ni) else {
            return;
        };
        let job = super::notifications::Job::new(net, target, msg, mention);
        if self.notifications.admit(job) == super::notifications::Admission::Overflow {
            crate::debugf!(
                net.logger,
                "notification queue full; dropping optional push"
            );
        }
    }

    /// Broadcasts a read-marker update through the bounded notification worker.
    pub fn broadcast_web_push(&mut self, ni: usize, msg: Message) {
        let Some(net) = self.networks.get(ni) else {
            return;
        };
        let target = net.casemap(msg.param(0));
        self.queue_web_push(ni, target, msg, None);
    }
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
    user: i64,
    network: i64,
    subs: Vec<db::WebPushSubscription>,
    logger: &Logger,
    msg: &Message,
) -> bool {
    let Some(sender) = srv.web_push() else {
        return false;
    };
    let mut changed = false;
    for saved in subs {
        let sub = match db::run(&srv.db, move |db| {
            db.get_web_push_subscription(user, network, saved.id, &saved.endpoint)
        })
        .await
        {
            Ok(Some(sub)) => sub,
            Ok(None) => continue,
            Err(error) => {
                logf!(logger, "failed to recheck Web Push subscription: {error}");
                continue;
            }
        };
        let result = send_web_push(&sender, &sub, msg).await;
        changed |= finish_delivery(&srv.db, user, network, sub, logger, result).await;
    }
    changed
}

async fn finish_delivery(
    db: &Arc<db::Database>,
    user: i64,
    network: i64,
    sub: db::WebPushSubscription,
    logger: &Logger,
    result: Result<()>,
) -> bool {
    let Err(error) = result else {
        return false;
    };
    let expired = error.is::<crate::webpush::SubscriptionExpired>();
    if !expired {
        logf!(
            logger,
            "failed to send Web push notification to endpoint {:?}: {error}",
            sub.endpoint
        );
    }
    let old = sub
        .updated_at
        .is_none_or(|time| Timestamp::now() - time > jiff::SignedDuration::from_hours(720));
    if !expired && !old {
        return false;
    }
    match db::run(db, move |db| {
        db.delete_web_push_subscription_if_current(user, network, &sub)
    })
    .await
    {
        Ok(changed) => changed,
        Err(error) => {
            logf!(
                logger,
                "failed to delete expired Web Push subscription: {error}"
            );
            false
        }
    }
}

///
/// # Errors
///
/// Returns an error for invalid keys, rejected endpoints, cryptographic errors, or HTTP delivery/setup failures.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_delivery_preserves_replaced_and_other_user_subscriptions() -> Result<()> {
        let db = Arc::new(db::Database::open_in_memory()?);
        let mut user = db::User::new("push-expiry");
        let mut other = db::User::new("other");
        db.store_user(&mut user)?;
        db.store_user(&mut other)?;
        let logger = Logger::new(false);
        let mut sub = db::WebPushSubscription {
            endpoint: "https://push.example/one".into(),
            ..Default::default()
        };
        db.store_web_push_subscription(user.id, 0, &mut sub)?;
        let snapshot = db
            .get_web_push_subscription(user.id, 0, sub.id, &sub.endpoint)?
            .ok_or_else(|| anyhow::anyhow!("missing subscription"))?;
        assert!(
            db.get_web_push_subscription(other.id, 0, sub.id, &sub.endpoint)?
                .is_none()
        );
        sub.keys.auth = "renewed-key".into();
        db.store_web_push_subscription(user.id, 0, &mut sub)?;
        assert!(
            !finish_delivery(
                &db,
                user.id,
                0,
                snapshot,
                &logger,
                Err(crate::webpush::SubscriptionExpired.into())
            )
            .await
        );
        let current = db
            .get_web_push_subscription(user.id, 0, sub.id, &sub.endpoint)?
            .ok_or_else(|| anyhow::anyhow!("missing replacement"))?;
        assert!(
            finish_delivery(
                &db,
                user.id,
                0,
                current,
                &logger,
                Err(crate::webpush::SubscriptionExpired.into())
            )
            .await
        );
        assert_eq!(
            db.list_web_push_subscriptions(user.id, 0)?,
            [] as [db::WebPushSubscription; 0]
        );
        let mut replacement = db::WebPushSubscription {
            endpoint: "https://push.example/other".into(),
            ..Default::default()
        };
        db.store_web_push_subscription(other.id, 0, &mut replacement)?;
        // Row IDs may be reused: owner and endpoint checks prevent stale work.
        assert!(
            db.get_web_push_subscription(user.id, 0, sub.id, &sub.endpoint)?
                .is_none()
        );
        assert!(!db.delete_web_push_subscription_if_current(user.id, 0, &sub)?);
        assert_eq!(db.list_web_push_subscriptions(other.id, 0)?.len(), 1);
        Ok(())
    }
}
