use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, named_params, params};

use super::*;
use crate::irc::Message;
use crate::xirc;

use super::sqlite_migrations::MIGRATIONS;

/// Current schema version, the same as upstream soju.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const SCHEMA: &str = include_str!("sqlite_schema.sql");

/// Times are stored as text in the server-time layout, which sorts
/// lexicographically.
fn time_to_sql(t: Option<DateTime<Utc>>) -> Option<String> {
    t.map(xirc::format_server_time)
}

fn time_from_sql(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        None => Ok(None),
        Some(s) => xirc::parse_server_time(&s)
            .map(Some)
            .ok_or_else(|| anyhow!("cannot parse time {s:?}")),
    }
}

/// Reads a text column without requiring valid UTF-8. Go soju stores
/// whatever bytes the upstream server sent, so a database it wrote may hold
/// e.g. Latin-1 messages.
fn lossy_string(r: &rusqlite::Row, i: usize) -> rusqlite::Result<String> {
    let b = r.get_ref(i)?.as_bytes()?;
    Ok(String::from_utf8_lossy(b).into_owned())
}

fn null_string(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// The SQLite database. All operations are serialized on a single
/// connection.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(source: &str) -> Result<Database> {
        let conn = Connection::open(source)?;
        Self::setup(conn)
    }

    pub fn open_in_memory() -> Result<Database> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(conn: Connection) -> Result<Database> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // journal_mode returns a row
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Database {
            conn: Mutex::new(conn),
        };
        db.upgrade()?;
        Ok(db)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn upgrade(&self) -> Result<()> {
        let mut conn = self.conn();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        } else if version > SCHEMA_VERSION {
            bail!("soju (version {SCHEMA_VERSION}) older than schema (version {version})");
        }
        // Some migrations rebuild tables referenced by foreign keys. As
        // recommended by SQLite, disable foreign key enforcement while
        // altering the schema (this can't be changed inside a transaction).
        conn.pragma_update(None, "foreign_keys", false)?;
        let res = Self::run_migrations(&mut conn, version);
        conn.pragma_update(None, "foreign_keys", true)?;
        res
    }

    fn run_migrations(conn: &mut Connection, version: i64) -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            tx.execute_batch(SCHEMA)
                .map_err(|e| anyhow!("failed to initialize schema: {e}"))?;
        } else {
            for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
                tx.execute_batch(migration)
                    .map_err(|e| anyhow!("failed to execute migration #{i}: {e}"))?;
            }
        }
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|e| anyhow!("failed to bump schema version: {e}"))?;
        tx.commit()?;
        Ok(())
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM User) AS users,
                (SELECT COUNT(*) FROM Network) AS networks,
                (SELECT COUNT(*) FROM Channel) AS channels",
            [],
            |r| {
                Ok(DatabaseStats {
                    users: r.get(0)?,
                    networks: r.get(1)?,
                    channels: r.get(2)?,
                })
            },
        )?)
    }

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
            nick: r.get::<_, Option<String>>(3 + off)?.unwrap_or_default(),
            realname: r.get::<_, Option<String>>(4 + off)?.unwrap_or_default(),
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

    pub fn list_inactive_usernames(&self, limit: DateTime<Utc>) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT username FROM User WHERE coalesce(downstream_interacted_at, created_at) < ?",
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

    pub fn list_networks(&self, user_id: i64) -> Result<Vec<Network>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, addr, nick, username, realname, certfp, pass,
                connect_commands, sasl_mechanism, sasl_plain_username, sasl_plain_password,
                sasl_external_cert, sasl_external_key, auto_away, enabled
            FROM Network
            WHERE user = ?",
        )?;
        let rows = stmt.query_map([user_id], |r| {
            let s = |i: usize| -> rusqlite::Result<String> {
                Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default())
            };
            let connect_commands: Option<String> = r.get(8)?;
            Ok(Network {
                id: r.get(0)?,
                name: s(1)?,
                addr: r.get(2)?,
                nick: s(3)?,
                username: s(4)?,
                realname: s(5)?,
                certfp: s(6)?,
                pass: s(7)?,
                connect_commands: match connect_commands {
                    Some(c) => c.split("\r\n").map(str::to_owned).collect(),
                    None => Vec::new(),
                },
                sasl: Sasl {
                    mechanism: s(9)?,
                    plain_username: s(10)?,
                    plain_password: s(11)?,
                    external_cert: r.get(12)?,
                    external_key: r.get(13)?,
                },
                auto_away: r.get(14)?,
                enabled: r.get(15)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_network(&self, user_id: i64, network: &mut Network) -> Result<()> {
        let (mut mech, mut plain_user, mut plain_pass) = (None, None, None);
        if !network.sasl.mechanism.is_empty() {
            mech = Some(network.sasl.mechanism.clone());
            match network.sasl.mechanism.as_str() {
                "PLAIN" => {
                    plain_user = null_string(&network.sasl.plain_username).map(str::to_owned);
                    plain_pass = null_string(&network.sasl.plain_password).map(str::to_owned);
                    network.sasl.external_cert = None;
                    network.sasl.external_key = None;
                }
                "EXTERNAL" => {
                    // keep sasl_plain_* NULL
                }
                m => bail!("soju: cannot store network: unsupported SASL mechanism {m:?}"),
            }
        }

        let connect_commands = network.connect_commands.join("\r\n");
        let conn = self.conn();
        if network.id != 0 {
            conn.execute(
                "UPDATE Network
                SET name = :name, addr = :addr, nick = :nick, username = :username,
                    realname = :realname, certfp = :certfp, pass = :pass, connect_commands = :connect_commands,
                    sasl_mechanism = :sasl_mechanism, sasl_plain_username = :sasl_plain_username, sasl_plain_password = :sasl_plain_password,
                    sasl_external_cert = :sasl_external_cert, sasl_external_key = :sasl_external_key,
                    auto_away = :auto_away, enabled = :enabled
                WHERE id = :id",
                named_params! {
                    ":name": null_string(&network.name),
                    ":addr": network.addr,
                    ":nick": null_string(&network.nick),
                    ":username": null_string(&network.username),
                    ":realname": null_string(&network.realname),
                    ":certfp": null_string(&network.certfp),
                    ":pass": null_string(&network.pass),
                    ":connect_commands": null_string(&connect_commands),
                    ":sasl_mechanism": mech,
                    ":sasl_plain_username": plain_user,
                    ":sasl_plain_password": plain_pass,
                    ":sasl_external_cert": network.sasl.external_cert,
                    ":sasl_external_key": network.sasl.external_key,
                    ":auto_away": network.auto_away,
                    ":enabled": network.enabled,
                    ":id": network.id,
                },
            )?;
        } else {
            conn.execute(
                "INSERT INTO Network(user, name, addr, nick, username, realname, certfp, pass,
                    connect_commands, sasl_mechanism, sasl_plain_username,
                    sasl_plain_password, sasl_external_cert, sasl_external_key, auto_away, enabled)
                VALUES (:user, :name, :addr, :nick, :username, :realname, :certfp, :pass,
                    :connect_commands, :sasl_mechanism, :sasl_plain_username,
                    :sasl_plain_password, :sasl_external_cert, :sasl_external_key, :auto_away, :enabled)",
                named_params! {
                    ":user": user_id,
                    ":name": null_string(&network.name),
                    ":addr": network.addr,
                    ":nick": null_string(&network.nick),
                    ":username": null_string(&network.username),
                    ":realname": null_string(&network.realname),
                    ":certfp": null_string(&network.certfp),
                    ":pass": null_string(&network.pass),
                    ":connect_commands": null_string(&connect_commands),
                    ":sasl_mechanism": mech,
                    ":sasl_plain_username": plain_user,
                    ":sasl_plain_password": plain_pass,
                    ":sasl_external_cert": network.sasl.external_cert,
                    ":sasl_external_key": network.sasl.external_key,
                    ":auto_away": network.auto_away,
                    ":enabled": network.enabled,
                },
            )?;
            network.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn delete_network(&self, id: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM Message WHERE target IN (SELECT id FROM MessageTarget WHERE network = ?)",
            [id],
        )?;
        tx.execute("DELETE FROM MessageTarget WHERE network = ?", [id])?;
        tx.execute("DELETE FROM WebPushSubscription WHERE network = ?", [id])?;
        tx.execute("DELETE FROM DeliveryReceipt WHERE network = ?", [id])?;
        tx.execute("DELETE FROM ReadReceipt WHERE network = ?", [id])?;
        tx.execute("DELETE FROM Channel WHERE network = ?", [id])?;
        tx.execute("DELETE FROM Network WHERE id = ?", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_channels(&self, network_id: i64) -> Result<Vec<Channel>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT
                id, name, key, detached, detached_internal_msgid,
                relay_detached, reattach_on, detach_after, detach_on
            FROM Channel
            WHERE network = ?",
        )?;
        let rows = stmt.query_map([network_id], |r| {
            let detach_after: i64 = r.get(7)?;
            Ok(Channel {
                id: r.get(0)?,
                name: r.get(1)?,
                key: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                detached: r.get(3)?,
                detached_internal_msg_id: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                relay_detached: MessageFilter::from_i64(r.get(5)?),
                reattach_on: MessageFilter::from_i64(r.get(6)?),
                detach_after: Duration::from_secs(detach_after.max(0) as u64),
                detach_on: MessageFilter::from_i64(r.get(8)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_channel(&self, network_id: i64, ch: &mut Channel) -> Result<()> {
        let detach_after = ch.detach_after.as_secs_f64().ceil() as i64;
        let conn = self.conn();
        if ch.id != 0 {
            conn.execute(
                "UPDATE Channel
                SET network = :network, name = :name, key = :key, detached = :detached,
                    detached_internal_msgid = :detached_internal_msgid, relay_detached = :relay_detached,
                    reattach_on = :reattach_on, detach_after = :detach_after, detach_on = :detach_on
                WHERE id = :id",
                named_params! {
                    ":network": network_id,
                    ":name": ch.name,
                    ":key": null_string(&ch.key),
                    ":detached": ch.detached,
                    ":detached_internal_msgid": null_string(&ch.detached_internal_msg_id),
                    ":relay_detached": ch.relay_detached as i64,
                    ":reattach_on": ch.reattach_on as i64,
                    ":detach_after": detach_after,
                    ":detach_on": ch.detach_on as i64,
                    ":id": ch.id,
                },
            )?;
        } else {
            conn.execute(
                "INSERT INTO Channel(network, name, key, detached, detached_internal_msgid, relay_detached, reattach_on, detach_after, detach_on)
                VALUES (:network, :name, :key, :detached, :detached_internal_msgid, :relay_detached, :reattach_on, :detach_after, :detach_on)",
                named_params! {
                    ":network": network_id,
                    ":name": ch.name,
                    ":key": null_string(&ch.key),
                    ":detached": ch.detached,
                    ":detached_internal_msgid": null_string(&ch.detached_internal_msg_id),
                    ":relay_detached": ch.relay_detached as i64,
                    ":reattach_on": ch.reattach_on as i64,
                    ":detach_after": detach_after,
                    ":detach_on": ch.detach_on as i64,
                },
            )?;
            ch.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn delete_channel(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM Channel WHERE id = ?", [id])?;
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

    pub fn list_delivery_receipts(&self, network_id: i64) -> Result<Vec<DeliveryReceipt>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, target, client, internal_msgid
            FROM DeliveryReceipt
            WHERE network = ?",
        )?;
        let rows = stmt.query_map([network_id], |r| {
            Ok(DeliveryReceipt {
                id: r.get(0)?,
                target: r.get(1)?,
                client: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                internal_msg_id: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_client_delivery_receipts(
        &self,
        network_id: i64,
        client: &str,
        receipts: &mut [DeliveryReceipt],
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM DeliveryReceipt WHERE network = ? AND client IS ?",
            params![network_id, null_string(client)],
        )?;
        for rcpt in receipts.iter_mut() {
            tx.execute(
                "INSERT INTO DeliveryReceipt(network, target, client, internal_msgid)
                VALUES (:network, :target, :client, :internal_msgid)",
                named_params! {
                    ":network": network_id,
                    ":target": rcpt.target,
                    ":client": null_string(client),
                    ":internal_msgid": rcpt.internal_msg_id,
                },
            )?;
            rcpt.id = tx.last_insert_rowid();
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_read_receipt(&self, network_id: i64, name: &str) -> Result<Option<ReadReceipt>> {
        let conn = self.conn();
        let res = conn
            .query_row(
                "SELECT id, timestamp FROM ReadReceipt WHERE network = :network AND target = :target",
                named_params! {":network": network_id, ":target": name},
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        match res {
            None => Ok(None),
            Some((id, ts)) => Ok(Some(ReadReceipt {
                id,
                target: name.to_owned(),
                timestamp: time_from_sql(ts)?,
            })),
        }
    }

    pub fn store_read_receipt(&self, network_id: i64, receipt: &mut ReadReceipt) -> Result<()> {
        let conn = self.conn();
        let ts = time_to_sql(receipt.timestamp);
        if receipt.id != 0 {
            conn.execute(
                "UPDATE ReadReceipt SET timestamp = :timestamp WHERE id = :id",
                named_params! {":timestamp": ts, ":id": receipt.id},
            )?;
        } else {
            conn.execute(
                "INSERT INTO ReadReceipt(network, target, timestamp)
                VALUES (:network, :target, :timestamp)",
                named_params! {
                    ":network": network_id,
                    ":target": receipt.target,
                    ":timestamp": ts,
                },
            )?;
            receipt.id = conn.last_insert_rowid();
        }
        Ok(())
    }

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

    /// Lists subscriptions for a user and network (0 means the bouncer
    /// connection, not bound to any network).
    pub fn list_web_push_subscriptions(
        &self,
        user_id: i64,
        network_id: i64,
    ) -> Result<Vec<WebPushSubscription>> {
        let conn = self.conn();
        let network = if network_id != 0 {
            Some(network_id)
        } else {
            None
        };
        let mut stmt = conn.prepare(
            "SELECT id, endpoint, created_at, updated_at, key_auth, key_p256dh, key_vapid
            FROM WebPushSubscription
            WHERE user = ? AND network IS ?",
        )?;
        let rows = stmt.query_map(params![user_id, network], |r| {
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
                WHERE id = :id",
                named_params! {
                    ":now": now,
                    ":key_auth": sub.keys.auth,
                    ":key_p256dh": sub.keys.p256dh,
                    ":key_vapid": sub.keys.vapid,
                    ":id": sub.id,
                },
            )?;
        } else {
            let network = if network_id != 0 {
                Some(network_id)
            } else {
                None
            };
            conn.execute(
                "INSERT INTO
                WebPushSubscription(created_at, updated_at, user, network, endpoint,
                    key_auth, key_p256dh, key_vapid)
                VALUES (:now, :now, :user, :network, :endpoint, :key_auth,
                    :key_p256dh, :key_vapid)",
                named_params! {
                    ":now": now,
                    ":user": user_id,
                    ":network": network,
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

    /// Returns the ID of the last message for a target, or 0.
    pub fn get_message_last_id(&self, network_id: i64, name: &str) -> Result<i64> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT m.id FROM Message AS m, MessageTarget AS t
                WHERE t.network = :network AND t.target = :target AND m.target = t.id
                ORDER BY m.time DESC LIMIT 1",
                named_params! {":network": network_id, ":target": name},
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Returns the message target. If it doesn't exist, a target with a zero
    /// ID is returned.
    pub fn get_message_target(&self, network_id: i64, target: &str) -> Result<MessageTarget> {
        let conn = self.conn();
        let mut mt = MessageTarget {
            target: target.to_owned(),
            ..Default::default()
        };
        if let Some((id, pinned, muted, blocked)) = conn
            .query_row(
                "SELECT id, pinned, muted, blocked FROM MessageTarget WHERE network = :network AND target = :target",
                named_params! {":network": network_id, ":target": target},
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
        {
            mt.id = id;
            mt.pinned = pinned;
            mt.muted = muted;
            mt.blocked = blocked;
        }
        Ok(mt)
    }

    pub fn list_message_targets(&self, network_id: i64) -> Result<Vec<MessageTarget>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, target, pinned, muted, blocked
            FROM MessageTarget
            WHERE network = ? AND (pinned OR muted OR blocked)",
        )?;
        let rows = stmt.query_map([network_id], |r| {
            Ok(MessageTarget {
                id: r.get(0)?,
                target: r.get(1)?,
                pinned: r.get(2)?,
                muted: r.get(3)?,
                blocked: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_message_target(&self, network_id: i64, mt: &mut MessageTarget) -> Result<()> {
        let conn = self.conn();
        if mt.id != 0 {
            conn.execute(
                "UPDATE MessageTarget SET pinned = :pinned, muted = :muted, blocked = :blocked WHERE id = :id",
                named_params! {
                    ":pinned": mt.pinned,
                    ":muted": mt.muted,
                    ":blocked": mt.blocked,
                    ":id": mt.id,
                },
            )?;
        } else {
            conn.execute(
                "INSERT INTO
                MessageTarget(network, target, pinned, muted, blocked)
                VALUES (:network, :target, :pinned, :muted, :blocked)",
                named_params! {
                    ":network": network_id,
                    ":target": mt.target,
                    ":pinned": mt.pinned,
                    ":muted": mt.muted,
                    ":blocked": mt.blocked,
                },
            )?;
            mt.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn store_messages(
        &self,
        network_id: i64,
        name: &str,
        msgs: &[Message],
    ) -> Result<Vec<i64>> {
        if msgs.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO MessageTarget(network, target)
            VALUES (:network, :target)
            ON CONFLICT DO NOTHING",
            named_params! {":network": network_id, ":target": name},
        )?;
        let mut ids = Vec::with_capacity(msgs.len());
        {
            let mut stmt = tx.prepare(
                "INSERT INTO Message(target, raw, time, sender, text)
                SELECT id, :raw, :time, :sender, :text
                FROM MessageTarget as t
                WHERE network = :network AND target = :target",
            )?;
            for msg in msgs {
                let t = match msg.tags.get("time") {
                    Some(tag) => xirc::parse_server_time(tag)
                        .ok_or_else(|| anyhow!("failed to parse message time tag: {tag:?}"))?,
                    None => Utc::now(),
                };
                let text = match msg.command.as_str() {
                    "PRIVMSG" | "NOTICE" if msg.params.len() > 1 => {
                        Some(strip_ansi(&msg.params[1]))
                    }
                    _ => None,
                };
                stmt.execute(named_params! {
                    ":network": network_id,
                    ":target": name,
                    ":raw": msg.to_string(),
                    ":time": xirc::format_server_time(t),
                    ":sender": msg.prefix_name(),
                    ":text": text,
                })?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn list_message_last_per_target(
        &self,
        network_id: i64,
        options: &MessageOptions,
    ) -> Result<Vec<MessageTargetLast>> {
        let mut inner = String::from(
            "SELECT time
            FROM Message
            WHERE target = MessageTarget.id ",
        );
        if !options.events {
            inner.push_str("AND text IS NOT NULL ");
        }
        inner.push_str("ORDER BY time DESC LIMIT 1");

        let mut query = format!(
            "SELECT target, ({inner}) latest
            FROM MessageTarget
            WHERE network = :network "
        );
        if options.after_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND latest > :after ");
        }
        if options.before_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND latest < :before ");
        }
        if options.take_last {
            query.push_str("ORDER BY latest DESC ");
        } else {
            query.push_str("ORDER BY latest ASC ");
        }
        query.push_str("LIMIT :limit");

        let conn = self.conn();
        let mut stmt = conn.prepare(&query)?;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> = vec![(":network", &network_id)];
        let after = time_to_sql(options.after_time);
        let before = time_to_sql(options.before_time);
        let limit = options.limit as i64;
        if options.after_time.is_some() {
            binds.push((":after", &after));
        }
        if options.before_time.is_some() {
            binds.push((":before", &before));
        }
        binds.push((":limit", &limit));
        let rows = stmt.query_map(binds.as_slice(), |r| {
            Ok((lossy_string(r, 0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut l = Vec::new();
        for row in rows {
            let (name, ts) = row?;
            l.push(MessageTargetLast {
                name,
                latest_message: time_from_sql(ts)?,
            });
        }
        if options.take_last {
            // We ordered by DESC to limit to the last lines. Reverse the list
            // to order by ASC these last lines.
            l.reverse();
        }
        Ok(l)
    }

    pub fn list_messages(
        &self,
        network_id: i64,
        name: &str,
        options: &MessageOptions,
    ) -> Result<Vec<Message>> {
        let mut query = String::from(
            "SELECT m.raw
            FROM Message AS m, MessageTarget AS t
            WHERE m.target = t.id AND t.network = :network AND t.target = :target ",
        );
        let after = time_to_sql(options.after_time);
        let before = time_to_sql(options.before_time);
        let text = quote_fts_query(&options.text);
        let limit = options.limit as i64;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> =
            vec![(":network", &network_id), (":target", &name)];
        if options.after_id > 0 {
            query.push_str("AND m.id > :afterID ");
            binds.push((":afterID", &options.after_id));
        }
        if options.after_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND m.time > :after ");
            binds.push((":after", &after));
        }
        if options.before_time.is_some() {
            // compares time strings by lexicographical order
            query.push_str("AND m.time < :before ");
            binds.push((":before", &before));
        }
        if !options.sender.is_empty() {
            query.push_str("AND m.sender = :sender ");
            binds.push((":sender", &options.sender));
        }
        if !options.text.is_empty() {
            query.push_str(
                "AND m.id IN (SELECT ROWID FROM MessageFTS WHERE MessageFTS MATCH :text) ",
            );
            binds.push((":text", &text));
        }
        if !options.events {
            query.push_str("AND m.text IS NOT NULL ");
        }
        if options.take_last {
            query.push_str("ORDER BY m.time DESC ");
        } else {
            query.push_str("ORDER BY m.time ASC ");
        }
        query.push_str("LIMIT :limit");
        binds.push((":limit", &limit));

        let conn = self.conn();
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map(binds.as_slice(), |r| lossy_string(r, 0))?;
        let mut l = Vec::new();
        for raw in rows {
            l.push(Message::parse(&raw?)?);
        }
        if options.take_last {
            l.reverse();
        }
        Ok(l)
    }

    /// Deletes messages older than before. Returns the number of deleted
    /// messages.
    pub fn delete_messages_before(&self, before: DateTime<Utc>) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM Message WHERE time < ?",
            [xirc::format_server_time(before)],
        )?)
    }

    /// Reclaims the disk space freed by deleted rows.
    pub fn vacuum(&self) -> Result<()> {
        let conn = self.conn();
        // VACUUM goes through the WAL, which keeps its size on disk until
        // truncated by a checkpoint
        conn.execute_batch(
            "INSERT INTO MessageFTS(MessageFTS) VALUES ('optimize');
            VACUUM;",
        )?;
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }
}

/// Returns whether a character is a token character for FTS5's unicode61
/// tokenizer: general categories L*, N* and Co.
fn is_fts_token_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

/// Quotes each token of a search query so that FTS5 operators are matched
/// literally.
fn quote_fts_query(query: &str) -> String {
    query
        .split(|c: char| !is_fts_token_char(c))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn users() {
        let db = db();
        let mut u = User::new("alice");
        u.set_password("pw").unwrap();
        db.store_user(&mut u).unwrap();
        assert_ne!(u.id, 0);
        let mut got = db.get_user("alice").unwrap().unwrap();
        assert_eq!(got, u);
        got.nick = "Alice".into();
        got.downstream_interacted_at = Some(Utc::now());
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
            .list_inactive_usernames(Utc::now() + chrono::Duration::hours(1))
            .unwrap();
        assert_eq!(inactive, vec!["alice"]);
        assert!(
            db.list_inactive_usernames(Utc::now() - chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
        db.delete_user(u.id).unwrap();
        assert!(db.list_users().unwrap().is_empty());
    }

    #[test]
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
        assert!(db.list_channels(n.id).unwrap().is_empty());
        db.delete_network(n.id).unwrap();
        assert!(db.list_networks(u.id).unwrap().is_empty());
    }

    #[test]
    fn messages() {
        let db = db();
        let mut u = User::new("alice");
        db.store_user(&mut u).unwrap();
        let mut n = Network::new("irc.example.org");
        db.store_network(u.id, &mut n).unwrap();

        let m1 = msg!("PRIVMSG", "#a", "hello \x02world\x02")
            .with_prefix(crate::irc::Prefix::new("bob"))
            .with_tag("time", "2024-01-01T00:00:00.000Z");
        let m2 = msg!("JOIN", "#a")
            .with_prefix(crate::irc::Prefix::new("carol"))
            .with_tag("time", "2024-01-01T00:00:01.000Z");
        let m3 = msg!("NOTICE", "#a", "goodbye world")
            .with_prefix(crate::irc::Prefix::new("bob"))
            .with_tag("time", "2024-01-01T00:00:02.000Z");
        let ids = db
            .store_messages(n.id, "#a", &[m1.clone(), m2.clone(), m3.clone()])
            .unwrap();
        assert_eq!(ids.len(), 3);
        assert_eq!(db.get_message_last_id(n.id, "#a").unwrap(), ids[2]);
        assert_eq!(db.get_message_last_id(n.id, "#b").unwrap(), 0);

        let opts = MessageOptions {
            limit: 10,
            ..Default::default()
        };
        assert_eq!(
            db.list_messages(n.id, "#a", &opts).unwrap(),
            vec![m1.clone(), m3.clone()]
        );
        let opts = MessageOptions {
            limit: 10,
            events: true,
            ..Default::default()
        };
        assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 3);
        let opts = MessageOptions {
            limit: 1,
            take_last: true,
            ..Default::default()
        };
        assert_eq!(
            db.list_messages(n.id, "#a", &opts).unwrap(),
            vec![m3.clone()]
        );
        let opts = MessageOptions {
            limit: 10,
            text: "world\"".into(),
            ..Default::default()
        };
        assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap().len(), 2);
        let opts = MessageOptions {
            limit: 10,
            text: "hello world".into(),
            sender: "bob".into(),
            ..Default::default()
        };
        assert_eq!(
            db.list_messages(n.id, "#a", &opts).unwrap(),
            vec![m1.clone()]
        );
        let opts = MessageOptions {
            limit: 10,
            after_time: xirc::parse_server_time("2024-01-01T00:00:00.000Z"),
            ..Default::default()
        };
        assert_eq!(
            db.list_messages(n.id, "#a", &opts).unwrap(),
            vec![m3.clone()]
        );

        db.store_messages(
            n.id,
            "#b",
            &[msg!("PRIVMSG", "#b", "x").with_tag("time", "2024-01-01T00:00:05.000Z")],
        )
        .unwrap();
        let opts = MessageOptions {
            limit: 10,
            after_time: xirc::parse_server_time("2023-01-01T00:00:00.000Z"),
            before_time: xirc::parse_server_time("2025-01-01T00:00:00.000Z"),
            ..Default::default()
        };
        let targets = db.list_message_last_per_target(n.id, &opts).unwrap();
        assert_eq!(
            targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["#a", "#b"]
        );
        assert_eq!(
            targets[0].latest_message,
            xirc::parse_server_time("2024-01-01T00:00:02.000Z")
        );

        db.delete_messages_before(xirc::parse_server_time("2024-01-01T00:00:01.500Z").unwrap())
            .unwrap();
        let opts = MessageOptions {
            limit: 10,
            events: true,
            ..Default::default()
        };
        assert_eq!(db.list_messages(n.id, "#a", &opts).unwrap(), vec![m3]);
    }

    #[test]
    fn delete_messages_vacuum() {
        let dir = std::env::temp_dir().join(format!("soju-rs-vacuum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.db");
        let db = Database::open(path.to_str().unwrap()).unwrap();
        let mut u = User::new("alice");
        db.store_user(&mut u).unwrap();
        let mut n = Network::new("irc.example.org");
        db.store_network(u.id, &mut n).unwrap();
        let text = "x".repeat(1000);
        let msgs: Vec<Message> = (0..2000)
            .map(|_| {
                msg!("PRIVMSG", "#a", text.as_str()).with_tag("time", "2024-01-01T00:00:00.000Z")
            })
            .collect();
        db.store_messages(n.id, "#a", &msgs).unwrap();
        let size = || std::fs::metadata(&path).unwrap().len();
        db.conn()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        let before = size();

        assert_eq!(db.delete_messages_before(Utc::now()).unwrap(), 2000);
        db.vacuum().unwrap();
        assert!(size() < before / 2, "{} >= {before} / 2", size());
        let wal = std::fs::metadata(dir.join("main.db-wal")).map_or(0, |m| m.len());
        assert_eq!(wal, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn messages_invalid_utf8() {
        let db = db();
        let mut u = User::new("alice");
        db.store_user(&mut u).unwrap();
        let mut n = Network::new("irc.example.org");
        db.store_network(u.id, &mut n).unwrap();
        db.store_messages(n.id, "#a", &[msg!("PRIVMSG", "#a", "x")])
            .unwrap();

        // Go soju stores raw upstream bytes, which may be e.g. Latin-1
        let raw: &[u8] = b"@time=2024-01-01T00:00:00.000Z :bob PRIVMSG #a :caf\xe9";
        db.conn()
            .execute(
                "INSERT INTO Message(target, raw, time, sender, text)
                SELECT id, CAST(?1 AS TEXT), '2024-01-01T00:00:00.000Z', 'bob', 'caf'
                FROM MessageTarget WHERE target = '#a'",
                [raw],
            )
            .unwrap();

        let opts = MessageOptions {
            limit: 10,
            ..Default::default()
        };
        let l = db.list_messages(n.id, "#a", &opts).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].params, vec!["#a", "caf\u{fffd}"]);
    }

    #[test]
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
            timestamp: Some(Utc::now()),
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
        assert!(db.list_message_targets(n.id).unwrap().is_empty());

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
        assert!(
            db.list_web_push_subscriptions(u.id, n.id)
                .unwrap()
                .is_empty()
        );
        db.delete_web_push_subscription(sub.id).unwrap();
        assert!(db.list_web_push_subscriptions(u.id, 0).unwrap().is_empty());

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
    fn fts_quote() {
        assert_eq!(quote_fts_query("foo bar"), "\"foo\" \"bar\"");
        assert_eq!(quote_fts_query("a\"b OR c*"), "\"a\" \"b\" \"OR\" \"c\"");
        assert_eq!(quote_fts_query(""), "");
    }

    // SQLite version 0 schema, from upstream. DO NOT EDIT.
    const SQLITE_V0_SCHEMA: &str = "
CREATE TABLE User (
	username VARCHAR(255) NOT NULL UNIQUE,
	password VARCHAR(255)
);

CREATE TABLE Network (
	id INTEGER PRIMARY KEY,
	name VARCHAR(255),
	user VARCHAR(255) NOT NULL,
	addr VARCHAR(255) NOT NULL,
	nick VARCHAR(255) NOT NULL,
	username VARCHAR(255),
	realname VARCHAR(255),
	pass VARCHAR(255),
	sasl_mechanism VARCHAR(255),
	sasl_plain_username VARCHAR(255),
	sasl_plain_password VARCHAR(255),
	UNIQUE(user, addr, nick),
	UNIQUE(user, name)
);

CREATE TABLE Channel (
	id INTEGER PRIMARY KEY,
	network INTEGER NOT NULL,
	name VARCHAR(255) NOT NULL,
	key VARCHAR(255),
	FOREIGN KEY(network) REFERENCES Network(id),
	UNIQUE(network, name)
);

PRAGMA user_version = 1;
";

    #[test]
    fn migrations() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SQLITE_V0_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO User(username, password) VALUES ('alice', NULL);
            INSERT INTO Network(name, user, addr, nick) VALUES ('net', 'alice', 'irc.example.org', 'alice');
            INSERT INTO Channel(network, name) VALUES (1, '#soju');",
        )
        .unwrap();
        let db = Database::setup(conn).unwrap();
        let users = db.list_users().unwrap();
        assert_eq!(users.len(), 1);
        let nets = db.list_networks(users[0].id).unwrap();
        assert_eq!(nets[0].name, "net");
        assert!(nets[0].enabled && nets[0].auto_away);
        assert_eq!(db.list_channels(nets[0].id).unwrap()[0].name, "#soju");
        let version: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 30);
    }

    #[test]
    fn newer_schema_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 999").unwrap();
        let err = Database::setup(conn).err().unwrap();
        assert!(err.to_string().contains("older than schema"));
    }
}
