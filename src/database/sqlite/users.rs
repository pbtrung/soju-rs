//! Users and their device certificates.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, named_params, params};

use super::{Database, lossy_string, null_string, time_from_sql, time_to_sql};
use crate::database::{DeviceCertificate, DuplicateDeviceCertificate, User};
use crate::xirc;

impl Database {
    fn user_from_row(
        r: &rusqlite::Row,
        username: Option<String>,
    ) -> rusqlite::Result<(User, Option<String>)> {
        // Columns: id, [username], password, admin, nick, realname, enabled,
        // downstream_interacted_at, max_networks
        let off = if username.is_some() { 0 } else { 1 };
        let username = match username {
            Some(u) => u,
            None => r.get(1)?,
        };
        let user = User {
            id: r.get(0)?,
            username,
            password: r.get::<_, Option<String>>(1 + off)?.unwrap_or_default(),
            admin: r.get(2 + off)?,
            nick: lossy_string(r, 3 + off)?,
            realname: lossy_string(r, 4 + off)?,
            enabled: r.get(5 + off)?,
            downstream_interacted_at: None,
            max_networks: r.get(7 + off)?,
        };
        Ok((user, r.get(6 + off)?))
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, username, password, admin, nick, realname, enabled,
                downstream_interacted_at, max_networks
            FROM User",
        )?;
        let rows = stmt.query_map([], |r| Self::user_from_row(r, None))?;
        let mut users = Vec::new();
        for row in rows {
            let (mut user, t) = row?;
            user.downstream_interacted_at = time_from_sql(t)?;
            users.push(user);
        }
        Ok(users)
    }

    /// Returns None if the user doesn't exist.
    pub fn get_user(&self, username: &str) -> Result<Option<User>> {
        let conn = self.conn();
        let res = conn
            .query_row(
                "SELECT id, password, admin, nick, realname, enabled,
                    downstream_interacted_at, max_networks
                FROM User
                WHERE username = ?",
                [username],
                |r| Self::user_from_row(r, Some(username.to_owned())),
            )
            .optional()?;
        match res {
            Some((mut user, t)) => {
                user.downstream_interacted_at = time_from_sql(t)?;
                Ok(Some(user))
            }
            None => Ok(None),
        }
    }

    pub fn get_username_by_id(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT username FROM User WHERE id = ?", [id], |r| r.get(0))
            .optional()?)
    }

    /// Lists the enabled users who haven't interacted since limit.
    pub fn list_inactive_usernames(&self, limit: DateTime<Utc>) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT username FROM User
            WHERE coalesce(downstream_interacted_at, created_at) < ? AND enabled = 1",
        )?;
        let rows = stmt.query_map([xirc::format_server_time(limit)], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_user(&self, user: &mut User) -> Result<()> {
        let conn = self.conn();
        let now = xirc::format_server_time(Utc::now());
        let interacted = time_to_sql(user.downstream_interacted_at);
        if user.id != 0 {
            conn.execute(
                "UPDATE User
                SET password = :password, admin = :admin, nick = :nick,
                    realname = :realname, enabled = :enabled,
                    downstream_interacted_at = :downstream_interacted_at,
                    max_networks = :max_networks
                WHERE username = :username",
                named_params! {
                    ":username": user.username,
                    ":password": null_string(&user.password),
                    ":admin": user.admin,
                    ":nick": null_string(&user.nick),
                    ":realname": null_string(&user.realname),
                    ":enabled": user.enabled,
                    ":downstream_interacted_at": interacted,
                    ":max_networks": user.max_networks,
                },
            )?;
        } else {
            conn.execute(
                "INSERT INTO
                User(username, password, admin, nick, realname, created_at,
                    enabled, downstream_interacted_at, max_networks)
                VALUES (:username, :password, :admin, :nick, :realname, :now,
                    :enabled, :downstream_interacted_at, :max_networks)",
                named_params! {
                    ":username": user.username,
                    ":password": null_string(&user.password),
                    ":admin": user.admin,
                    ":nick": null_string(&user.nick),
                    ":realname": null_string(&user.realname),
                    ":now": now,
                    ":enabled": user.enabled,
                    ":downstream_interacted_at": interacted,
                    ":max_networks": user.max_networks,
                },
            )?;
            user.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn delete_user(&self, id: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM DeliveryReceipt
            WHERE id IN (
                SELECT DeliveryReceipt.id
                FROM DeliveryReceipt
                JOIN Network ON DeliveryReceipt.network = Network.id
                WHERE Network.user = ?
            )",
            [id],
        )?;
        tx.execute(
            "DELETE FROM ReadReceipt
            WHERE id IN (
                SELECT ReadReceipt.id
                FROM ReadReceipt
                JOIN Network ON ReadReceipt.network = Network.id
                WHERE Network.user = ?
            )",
            [id],
        )?;
        tx.execute(
            "DELETE FROM Message
            WHERE id IN (
                SELECT Message.id
                FROM Message, MessageTarget, Network
                WHERE Message.target = MessageTarget.id
                AND MessageTarget.network = Network.id
                AND Network.user = ?
            )",
            [id],
        )?;
        tx.execute(
            "DELETE FROM MessageTarget
            WHERE id IN (
                SELECT MessageTarget.id
                FROM MessageTarget, Network
                WHERE MessageTarget.network = Network.id
                AND Network.user = ?
            )",
            [id],
        )?;
        tx.execute("DELETE FROM WebPushSubscription WHERE user = ?", [id])?;
        tx.execute("DELETE FROM DeviceCertificate WHERE user = ?", [id])?;
        tx.execute(
            "DELETE FROM Channel
            WHERE id IN (
                SELECT Channel.id
                FROM Channel
                JOIN Network ON Channel.network = Network.id
                WHERE Network.user = ?
            )",
            [id],
        )?;
        tx.execute("DELETE FROM Network WHERE user = ?", [id])?;
        tx.execute("DELETE FROM User WHERE id = ?", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Returns the owner user ID and the certificate, if any.
    pub fn get_device_certificate(
        &self,
        fingerprint: &[u8],
    ) -> Result<Option<(i64, DeviceCertificate)>> {
        let conn = self.conn();
        let res = conn
            .query_row(
                "SELECT user, id, label, last_used, last_ip
                FROM DeviceCertificate
                WHERE fingerprint = ?",
                [fingerprint],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        match res {
            None => Ok(None),
            Some((user, id, label, last_used, last_ip)) => Ok(Some((
                user,
                DeviceCertificate {
                    id,
                    label,
                    fingerprint: fingerprint.to_vec(),
                    last_used: time_from_sql(last_used)?,
                    last_ip,
                },
            ))),
        }
    }

    pub fn list_device_certificates(&self, user_id: i64) -> Result<Vec<DeviceCertificate>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, label, fingerprint, last_used, last_ip
            FROM DeviceCertificate
            WHERE user = ?",
        )?;
        let rows = stmt.query_map([user_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut certs = Vec::new();
        for row in rows {
            let (id, label, fingerprint, last_used, last_ip) = row?;
            certs.push(DeviceCertificate {
                id,
                label,
                fingerprint,
                last_used: time_from_sql(last_used)?,
                last_ip,
            });
        }
        Ok(certs)
    }

    /// Stores a device certificate. Fails with [`DuplicateDeviceCertificate`]
    /// if the fingerprint is already registered.
    pub fn store_device_certificate(
        &self,
        user_id: i64,
        cert: &mut DeviceCertificate,
    ) -> Result<()> {
        let conn = self.conn();
        let last_used = time_to_sql(cert.last_used);
        let res = if cert.id != 0 {
            conn.execute(
                "UPDATE DeviceCertificate
                SET label = :label, fingerprint = :fingerprint, last_used = :last_used, last_ip = :last_ip
                WHERE id = :id",
                named_params! {
                    ":label": cert.label,
                    ":fingerprint": cert.fingerprint,
                    ":last_used": last_used,
                    ":last_ip": cert.last_ip,
                    ":id": cert.id,
                },
            )
        } else {
            conn.execute(
                "INSERT INTO DeviceCertificate(user, label, fingerprint, last_used, last_ip)
                VALUES (:user, :label, :fingerprint, :last_used, :last_ip)",
                named_params! {
                    ":user": user_id,
                    ":label": cert.label,
                    ":fingerprint": cert.fingerprint,
                    ":last_used": last_used,
                    ":last_ip": cert.last_ip,
                },
            )
        };
        match res {
            Ok(_) => {
                if cert.id == 0 {
                    cert.id = conn.last_insert_rowid();
                }
                Ok(())
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Err(DuplicateDeviceCertificate.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_device_certificate(&self, user_id: i64, fingerprint: &[u8]) -> Result<()> {
        self.conn().execute(
            "DELETE FROM DeviceCertificate WHERE user = ? AND fingerprint = ?",
            params![user_id, fingerprint],
        )?;
        Ok(())
    }
}
