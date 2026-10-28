//! Routes account operations to the controller and tenant operations to the
//! owning user database. SQLite uses a single database for both.

use std::ops::Deref;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::*;
use crate::config;
use crate::irc::Message;

pub struct Database {
    account: Arc<sqlite::Database>,
    cloud: Option<Box<turso::Turso>>,
}

impl Deref for Database {
    type Target = sqlite::Database;
    fn deref(&self) -> &Self::Target {
        &self.account
    }
}

macro_rules! tenant_methods {
    ($($selector:ident($key:ident): fn $name:ident($($arg:ident: $typ:ty),*) -> $result:ty;)*) => {
        impl Database { $(
            pub fn $name(&self, $($arg: $typ),*) -> Result<$result> {
                self.$selector($key)?.$name($($arg),*)
            }
        )* }
    }
}

tenant_methods! {
    network_db(id): fn delete_network(id: i64) -> ();
    network_db(network_id): fn list_channels(network_id: i64) -> Vec<Channel>;
    network_db(network_id): fn store_channel(network_id: i64, ch: &mut Channel) -> ();
    channel_db(id): fn delete_channel(id: i64) -> ();
    network_db(network_id): fn list_delivery_receipts(network_id: i64) -> Vec<DeliveryReceipt>;
    network_db(network_id): fn store_client_delivery_receipts(network_id: i64, client: &str, receipts: &mut [DeliveryReceipt]) -> ();
    network_db(network_id): fn get_read_receipt(network_id: i64, name: &str) -> Option<ReadReceipt>;
    network_db(network_id): fn store_read_receipt(network_id: i64, receipt: &mut ReadReceipt) -> ();
    network_db(network_id): fn get_message_last_id(network_id: i64, name: &str) -> i64;
    network_db(network_id): fn get_message_target(network_id: i64, target: &str) -> MessageTarget;
    network_db(network_id): fn list_message_targets(network_id: i64) -> Vec<MessageTarget>;
    network_db(network_id): fn store_message_target(network_id: i64, mt: &mut MessageTarget) -> ();
    network_db(network_id): fn store_messages(network_id: i64, name: &str, msgs: &[Message]) -> Vec<i64>;
    network_db(network_id): fn list_message_last_per_target(network_id: i64, options: &MessageOptions) -> Vec<MessageTargetLast>;
    network_db(network_id): fn list_messages(network_id: i64, name: &str, options: &MessageOptions) -> Vec<Message>;
    user_db(user_id): fn list_web_push_networks(user_id: i64) -> Vec<i64>;
    user_db(user_id): fn list_web_push_subscriptions(user_id: i64, network_id: i64) -> Vec<WebPushSubscription>;
    user_db(user_id): fn store_web_push_subscription(user_id: i64, network_id: i64, sub: &mut WebPushSubscription) -> ();
    subscription_db(id): fn delete_web_push_subscription(id: i64) -> ();
}

impl Database {
    #[cfg(test)]
    pub(super) fn from_cloud(cloud: Box<turso::Turso>) -> Self {
        Self {
            account: cloud.controller.clone(),
            cloud: Some(cloud),
        }
    }

    #[cfg(test)]
    pub(super) fn account_conn_for_test(
        &self,
    ) -> std::sync::MutexGuard<'_, connection::Connection> {
        self.account.conn()
    }

    pub fn open(source: &str) -> Result<Self> {
        Ok(Self {
            account: Arc::new(sqlite::Database::open(source)?),
            cloud: None,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            account: Arc::new(sqlite::Database::open_in_memory()?),
            cloud: None,
        })
    }

    pub fn open_config(cfg: &config::Server) -> Result<Self> {
        match &cfg.turso {
            Some(cfg) => {
                let cloud = Box::new(turso::Turso::open(cfg)?);
                Ok(Self {
                    account: cloud.controller.clone(),
                    cloud: Some(cloud),
                })
            }
            None => Self::open(&cfg.db.source),
        }
    }

    fn user_db(&self, id: i64) -> Result<Arc<sqlite::Database>> {
        match &self.cloud {
            Some(c) => c.user(id),
            None => Ok(self.account.clone()),
        }
    }

    fn route(&self, table: &'static str, id: i64) -> Result<Arc<sqlite::Database>> {
        match &self.cloud {
            Some(c) => c.route(table, id),
            None => Ok(self.account.clone()),
        }
    }
    fn network_db(&self, id: i64) -> Result<Arc<sqlite::Database>> {
        self.route("Network", id)
    }
    fn channel_db(&self, id: i64) -> Result<Arc<sqlite::Database>> {
        self.route("Channel", id)
    }
    fn subscription_db(&self, id: i64) -> Result<Arc<sqlite::Database>> {
        self.route("WebPushSubscription", id)
    }

    pub fn list_networks(&self, user_id: i64) -> Result<Vec<Network>> {
        let networks = self.user_db(user_id)?.list_networks(user_id)?;
        if let Some(c) = &self.cloud {
            for network in &networks {
                c.remember_network(user_id, network.id)?;
            }
        }
        Ok(networks)
    }

    pub fn store_network(&self, user_id: i64, network: &mut Network) -> Result<()> {
        self.user_db(user_id)?.store_network(user_id, network)?;
        if let Some(c) = &self.cloud {
            c.remember_network(user_id, network.id)?;
        }
        Ok(())
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        self.account
            .list_users()?
            .into_iter()
            .filter_map(|u| match self.ready(u.id) {
                Ok(true) => Some(Ok(u)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    fn ready(&self, id: i64) -> Result<bool> {
        match &self.cloud {
            Some(c) => c.ready(id),
            None => Ok(true),
        }
    }

    pub fn get_user(&self, username: &str) -> Result<Option<User>> {
        match self.account.get_user(username)? {
            Some(u) if self.ready(u.id)? => Ok(Some(u)),
            _ => Ok(None),
        }
    }

    pub fn get_device_certificate(
        &self,
        fingerprint: &[u8],
    ) -> Result<Option<(i64, DeviceCertificate)>> {
        match self.account.get_device_certificate(fingerprint)? {
            Some((id, cert)) if self.ready(id)? => Ok(Some((id, cert))),
            _ => Ok(None),
        }
    }

    pub fn store_user(&self, user: &mut User) -> Result<()> {
        match &self.cloud {
            Some(c) if user.id == 0 => c.create_user(user),
            _ => self.account.store_user(user),
        }
    }

    pub fn delete_user(&self, id: i64) -> Result<()> {
        match &self.cloud {
            Some(c) => c.delete_user(id),
            None => self.account.delete_user(id),
        }
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let Some(c) = &self.cloud else {
            return self.account.stats();
        };
        let mut stats = DatabaseStats {
            users: self.list_users()?.len() as i64,
            ..Default::default()
        };
        for db in c.databases() {
            let s = db.stats()?;
            stats.networks += s.networks;
            stats.channels += s.channels;
        }
        Ok(stats)
    }

    pub fn delete_messages_before(&self, before: DateTime<Utc>, limit: usize) -> Result<usize> {
        let Some(c) = &self.cloud else {
            return self.account.delete_messages_before(before, limit);
        };
        let mut count = 0;
        for db in c.databases() {
            if count >= limit {
                break;
            }
            count += db.delete_messages_before(before, limit - count)?;
        }
        Ok(count)
    }

    pub fn free_page_ratio(&self) -> Result<f64> {
        if self.cloud.is_some() {
            Ok(0.0)
        } else {
            self.account.free_page_ratio()
        }
    }
    pub fn optimize(&self) -> Result<()> {
        if let Some(c) = &self.cloud {
            for db in c.databases() {
                db.conn()
                    .execute_batch("INSERT INTO MessageFTS(MessageFTS) VALUES ('optimize')")?;
            }
            Ok(())
        } else {
            self.account.optimize()
        }
    }
    pub fn vacuum(&self) -> Result<()> {
        if self.cloud.is_some() {
            Ok(())
        } else {
            self.account.vacuum()
        }
    }
    pub fn shrink_memory(&self) -> Result<()> {
        if self.cloud.is_some() {
            Ok(())
        } else {
            self.account.shrink_memory()
        }
    }
}
