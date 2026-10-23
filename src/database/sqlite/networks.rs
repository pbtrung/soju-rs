//! Networks and their channels.

use std::time::Duration;

use anyhow::{Result, bail};
use rusqlite::{TransactionBehavior, named_params};

use super::{Database, lossy_string, null_string};
use crate::database::{Channel, MessageFilter, Network, Sasl};

impl Database {
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
            let s = |i: usize| lossy_string(r, i);
            let connect_commands = s(8)?;
            Ok(Network {
                id: r.get(0)?,
                name: s(1)?,
                addr: r.get(2)?,
                nick: s(3)?,
                username: s(4)?,
                realname: s(5)?,
                certfp: s(6)?,
                pass: s(7)?,
                connect_commands: if connect_commands.is_empty() {
                    Vec::new()
                } else {
                    connect_commands.split("\r\n").map(str::to_owned).collect()
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
        let fields = named_params! {
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
        };
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
                [fields, named_params! {":id": network.id}].concat().as_slice(),
            )?;
        } else {
            conn.execute(
                "INSERT INTO Network(user, name, addr, nick, username, realname, certfp, pass,
                    connect_commands, sasl_mechanism, sasl_plain_username,
                    sasl_plain_password, sasl_external_cert, sasl_external_key, auto_away, enabled)
                VALUES (:user, :name, :addr, :nick, :username, :realname, :certfp, :pass,
                    :connect_commands, :sasl_mechanism, :sasl_plain_username,
                    :sasl_plain_password, :sasl_external_cert, :sasl_external_key, :auto_away, :enabled)",
                [fields, named_params! {":user": user_id}].concat().as_slice(),
            )?;
            network.id = conn.last_insert_rowid();
        }
        Ok(())
    }

    pub fn delete_network(&self, id: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::messages::record_message_id_high_water(&tx)?;
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
                name: lossy_string(r, 1)?,
                key: lossy_string(r, 2)?,
                detached: r.get(3)?,
                detached_internal_msg_id: lossy_string(r, 4)?,
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
        let fields = named_params! {
            ":network": network_id,
            ":name": ch.name,
            ":key": null_string(&ch.key),
            ":detached": ch.detached,
            ":detached_internal_msgid": null_string(&ch.detached_internal_msg_id),
            ":relay_detached": ch.relay_detached as i64,
            ":reattach_on": ch.reattach_on as i64,
            ":detach_after": detach_after,
            ":detach_on": ch.detach_on as i64,
        };
        let conn = self.conn();
        if ch.id != 0 {
            conn.execute(
                "UPDATE Channel
                SET network = :network, name = :name, key = :key, detached = :detached,
                    detached_internal_msgid = :detached_internal_msgid, relay_detached = :relay_detached,
                    reattach_on = :reattach_on, detach_after = :detach_after, detach_on = :detach_on
                WHERE id = :id",
                [fields, named_params! {":id": ch.id}].concat().as_slice(),
            )?;
        } else {
            conn.execute(
                "INSERT INTO Channel(network, name, key, detached, detached_internal_msgid, relay_detached, reattach_on, detach_after, detach_on)
                VALUES (:network, :name, :key, :detached, :detached_internal_msgid, :relay_detached, :reattach_on, :detach_after, :detach_on)",
                fields,
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
}
