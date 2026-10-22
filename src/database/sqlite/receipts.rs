//! Delivery and read receipts.

use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, named_params, params};

use super::{Database, lossy_string, null_string, time_from_sql, time_to_sql};
use crate::database::{DeliveryReceipt, ReadReceipt};

impl Database {
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
                target: lossy_string(r, 1)?,
                client: lossy_string(r, 2)?,
                internal_msg_id: lossy_string(r, 3)?,
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
}
