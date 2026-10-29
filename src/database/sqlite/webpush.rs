//! Web Push configuration and subscriptions.

use anyhow::{Result, bail};
use chrono::Utc;
use rusqlite::{named_params, params};

use super::{Database, null_id, time_from_sql};
use crate::database::{WebPushConfig, WebPushKeys, WebPushSubscription};
use crate::xirc;

impl Database {
    pub fn list_web_push_configs(&self) -> Result<Vec<WebPushConfig>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, vapid_key_public, vapid_key_private FROM WebPushConfig")?;
        let rows = stmt.query_map([], |r| {
            Ok(WebPushConfig {
                id: r.get(0)?,
                vapid_public: r.get(1)?,
                vapid_private: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_web_push_config(&self, config: &mut WebPushConfig) -> Result<()> {
        if config.id != 0 {
            bail!("cannot update a WebPushConfig");
        }
        let conn = self.conn();
        conn.execute(
            "INSERT INTO WebPushConfig(created_at, vapid_key_public, vapid_key_private)
            VALUES (:now, :vapid_key_public, :vapid_key_private)",
            named_params! {
                ":vapid_key_public": config.vapid_public,
                ":vapid_key_private": config.vapid_private,
                ":now": xirc::format_server_time(Utc::now()),
            },
        )?;
        config.id = conn.last_insert_rowid();
        Ok(())
    }

    /// Lists the networks a user has subscriptions for (0 for the bouncer
    /// connection).
    pub fn list_web_push_networks(&self, user_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT coalesce(network, 0) FROM WebPushSubscription WHERE user = ?",
        )?;
        let rows = stmt.query_map([user_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Lists subscriptions for a user and network (0 means the bouncer
    /// connection, not bound to any network).
    pub fn list_web_push_subscriptions(
        &self,
        user_id: i64,
        network_id: i64,
    ) -> Result<Vec<WebPushSubscription>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, endpoint, created_at, updated_at, key_auth, key_p256dh, key_vapid
            FROM WebPushSubscription
            WHERE user = ? AND network IS ?",
        )?;
        let rows = stmt.query_map(params![user_id, null_id(network_id)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut subs = Vec::new();
        for row in rows {
            let (id, endpoint, created, updated, auth, p256dh, vapid) = row?;
            subs.push(WebPushSubscription {
                id,
                endpoint,
                created_at: time_from_sql(created)?,
                updated_at: time_from_sql(updated)?,
                keys: WebPushKeys {
                    auth: auth.unwrap_or_default(),
                    p256dh: p256dh.unwrap_or_default(),
                    vapid: vapid.unwrap_or_default(),
                },
            });
        }
        Ok(subs)
    }

    pub fn store_web_push_subscription(
        &self,
        user_id: i64,
        network_id: i64,
        sub: &mut WebPushSubscription,
    ) -> Result<()> {
        let conn = self.conn();
        let now = xirc::format_server_time(Utc::now());
        if sub.id != 0 {
            conn.execute(
                "UPDATE WebPushSubscription
                SET updated_at = :now, key_auth = :key_auth, key_p256dh = :key_p256dh,
                    key_vapid = :key_vapid
                WHERE id = :id AND (updated_at IS NOT :now OR key_auth IS NOT :key_auth OR key_p256dh IS NOT :key_p256dh OR key_vapid IS NOT :key_vapid)",
                named_params! {
                    ":now": now,
                    ":key_auth": sub.keys.auth,
                    ":key_p256dh": sub.keys.p256dh,
                    ":key_vapid": sub.keys.vapid,
                    ":id": sub.id,
                },
            )?;
        } else {
            conn.execute(
                "INSERT INTO
                WebPushSubscription(id, created_at, updated_at, user, network, endpoint,
                    key_auth, key_p256dh, key_vapid)
                VALUES (:id, :now, :now, :user, :network, :endpoint, :key_auth,
                    :key_p256dh, :key_vapid)",
                named_params! {
                    ":id": super::new_row_id(&conn),
                    ":now": now,
                    ":user": user_id,
                    ":network": null_id(network_id),
                    ":endpoint": sub.endpoint,
                    ":key_auth": sub.keys.auth,
                    ":key_p256dh": sub.keys.p256dh,
                    ":key_vapid": sub.keys.vapid,
                },
            )?;
            sub.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn delete_web_push_subscription(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM WebPushSubscription WHERE id = ?", [id])?;
        Ok(())
    }
}
