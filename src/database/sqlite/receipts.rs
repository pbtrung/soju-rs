//! Delivery and read receipts.

use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, named_params, params};

use super::{Database, lossy_string, null_string, time_from_sql, time_to_sql};
use crate::database::connection::{Connection, statement};
use crate::database::{DeliveryReceipt, ReadReceipt};
use std::collections::{HashMap, HashSet};

enum Change {
    Delete(i64),
    Update(i64, usize),
    Insert(usize),
}

fn changes(
    conn: &Connection,
    network: i64,
    client: &str,
    receipts: &[DeliveryReceipt],
) -> Result<(Vec<Change>, Vec<i64>)> {
    let saved=conn.prepare("SELECT id,target,internal_msgid FROM DeliveryReceipt WHERE network=? AND client IS ? ORDER BY id")?
        .query_map(params![network,null_string(client)],|r|Ok((r.get::<_,i64>(0)?,lossy_string(r,1)?,lossy_string(r,2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut existing = HashMap::new();
    let mut changes = Vec::new();
    let mut ids = vec![0; receipts.len()];
    for (id, target, message) in saved {
        if let Some((previous, _)) = existing.insert(target, (id, message)) {
            changes.push(Change::Delete(previous));
        }
    }
    for (i, receipt) in receipts.iter().enumerate() {
        if let Some((id, message)) = existing.remove(&receipt.target) {
            ids[i] = id;
            if message != receipt.internal_msg_id {
                changes.push(Change::Update(id, i));
            }
        } else {
            changes.push(Change::Insert(i));
        }
    }
    changes.extend(existing.into_values().map(|(id, _)| Change::Delete(id)));
    Ok((changes, ids))
}

fn execute_changes(
    conn: &Connection,
    network: i64,
    client: &str,
    receipts: &[DeliveryReceipt],
    changes: &[Change],
    ids: &mut [i64],
) -> Result<()> {
    for change in changes {
        match *change {
            Change::Delete(id) => {
                conn.execute("DELETE FROM DeliveryReceipt WHERE id=?", [id])?;
            }
            Change::Update(id, i) => {
                conn.execute(
                    "UPDATE DeliveryReceipt SET internal_msgid=? WHERE id=?",
                    params![receipts[i].internal_msg_id, id],
                )?;
            }
            Change::Insert(i) => {
                conn.execute("INSERT INTO DeliveryReceipt(id,network,target,client,internal_msgid) VALUES(?,?,?,?,?)",params![if ids[i]==0{None}else{Some(ids[i])},network,receipts[i].target,null_string(client),receipts[i].internal_msg_id])?;
                if ids[i] == 0 {
                    ids[i] = conn.last_insert_rowid();
                }
            }
        }
    }
    Ok(())
}

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
        // Preserve the old NULL-client duplicate semantics for unusual
        // callers. The normal delivered-target map contains unique targets.
        let unique: HashSet<_> = receipts.iter().map(|r| &r.target).collect();
        if unique.len() != receipts.len() {
            return self.replace_duplicate_delivery_receipts(network_id, client, receipts);
        }
        let mut conn = self.conn();
        let ids = if conn.is_remote() {
            // This connection's mutex serializes receipt writers in the one
            // bouncer instance supported per controller. Background message
            // writers never mutate receipts. Commit only the resulting delta.
            let (changes, mut ids) = changes(&conn, network_id, client, receipts)?;
            let mut statements = Vec::new();
            for change in &changes {
                statements.push(match *change {
                    Change::Delete(id)=>statement("DELETE FROM DeliveryReceipt WHERE id=?",[id])?,
                    Change::Update(id,i)=>statement("UPDATE DeliveryReceipt SET internal_msgid=? WHERE id=?",params![receipts[i].internal_msg_id,id])?,
                    Change::Insert(i)=>{
                        ids[i]=super::new_row_id(&conn).unwrap();
                        statement("INSERT INTO DeliveryReceipt(id,network,target,client,internal_msgid) VALUES(?,?,?,?,?)",params![ids[i],network_id,receipts[i].target,null_string(client),receipts[i].internal_msg_id])?
                    },
                });
            }
            if !statements.is_empty()
                && let Err(error) = conn.atomic_batch(statements)
            {
                if !error.to_string().contains("HTTP body budget") {
                    return Err(error.into());
                }
                // The size check rejected the body before any HTTP send.
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                execute_changes(&tx, network_id, client, receipts, &changes, &mut ids)?;
                tx.commit()?;
            }
            ids
        } else {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (changes, mut ids) = changes(&tx, network_id, client, receipts)?;
            execute_changes(&tx, network_id, client, receipts, &changes, &mut ids)?;
            tx.commit()?;
            ids
        };
        for (receipt, id) in receipts.iter_mut().zip(ids) {
            receipt.id = id;
        }
        Ok(())
    }

    fn replace_duplicate_delivery_receipts(
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
                "UPDATE ReadReceipt SET timestamp = :timestamp WHERE id = :id AND (timestamp IS NOT :timestamp)",
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
