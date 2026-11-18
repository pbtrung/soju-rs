//! Delivery and read receipts.

use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, named_params, params};

use super::{Database, lossy_string, null_string, time_from_sql, time_to_sql};
use crate::database::{DeliveryReceipt, ReadReceipt};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

// SQLite callbacks use the same lossy UTF-8 and IRC mappings as legacy eager
// loading. Ordinary client lookups retain the indexed native comparison.
pub(super) fn register_case_functions(conn: &Connection) -> rusqlite::Result<()> {
    use crate::xirc::{CASEMAPPING_ASCII, CASEMAPPING_RFC1459, CASEMAPPING_RFC1459_STRICT};
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;
    for (name, cm) in [
        ("soju_cm_ascii", CASEMAPPING_ASCII),
        ("soju_cm_rfc1459", CASEMAPPING_RFC1459),
        ("soju_cm_strict", CASEMAPPING_RFC1459_STRICT),
    ] {
        conn.create_scalar_function(name, 1, flags, move |ctx| {
            let raw = ctx.get_raw(0);
            let bytes = raw.as_bytes_or_null()?.unwrap_or_default();
            Ok(cm(&String::from_utf8_lossy(bytes)))
        })?;
    }
    conn.create_scalar_function("soju_lossy", 1, flags, |ctx| {
        Ok(
            String::from_utf8_lossy(ctx.get_raw(0).as_bytes_or_null()?.unwrap_or_default())
                .into_owned(),
        )
    })?;
    Ok(())
}

fn receipt_target_expr(cm: crate::xirc::CaseMapping) -> Result<&'static str> {
    use crate::xirc::{CASEMAPPING_ASCII, CASEMAPPING_RFC1459, CASEMAPPING_RFC1459_STRICT};
    if std::ptr::fn_addr_eq(cm, CASEMAPPING_ASCII) {
        Ok("soju_cm_ascii(target)")
    } else if std::ptr::fn_addr_eq(cm, CASEMAPPING_RFC1459) {
        Ok("soju_cm_rfc1459(target)")
    } else if std::ptr::fn_addr_eq(cm, CASEMAPPING_RFC1459_STRICT) {
        Ok("soju_cm_strict(target)")
    } else {
        anyhow::bail!("unsupported delivery-receipt case mapping");
    }
}

fn receipt_client_predicate(client: &str) -> &'static str {
    if client.contains('\u{fffd}') {
        "soju_lossy(client)=?"
    } else {
        "client IS ?"
    }
}

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
    let saved = conn
        .prepare(
            "SELECT id,target,internal_msgid FROM DeliveryReceipt WHERE network=? \
        AND client IS ? ORDER BY id",
        )?
        .query_map(params![network, null_string(client)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                lossy_string(r, 1)?,
                lossy_string(r, 2)?,
            ))
        })?
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
            {
                #![expect(
                    clippy::indexing_slicing,
                    reason = "The surrounding length check or bounded iteration establishes this local collection index."
                )]
                ids[i] = id;
            }
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
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The surrounding length check or bounded iteration establishes this local collection index."
                    )]
                    conn.execute(
                        "UPDATE DeliveryReceipt SET internal_msgid=? WHERE id=?",
                        params![receipts[i].internal_msg_id, id],
                    )?;
                }
            }
            Change::Insert(i) => {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The surrounding length check or bounded iteration establishes this local collection index."
                    )]
                    conn.execute(
                        "INSERT INTO DeliveryReceipt(id,network,target,client,internal_msgid) \
                    VALUES(?,?,?,?,?)",
                        params![
                            if ids[i] == 0 { None } else { Some(ids[i]) },
                            network,
                            receipts[i].target,
                            null_string(client),
                            receipts[i].internal_msg_id
                        ],
                    )?;
                }
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "The surrounding length check or bounded iteration establishes this local collection index."
                    )]
                    if ids[i] == 0 {
                        {
                            #![expect(
                                clippy::indexing_slicing,
                                reason = "The surrounding length check or bounded iteration establishes this local collection index."
                            )]
                            ids[i] = conn.last_insert_rowid();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

impl Database {
    /// Updates only the supplied cursors, preserving all uncached durable rows.
    /// Existing case aliases are updated together without deleting records.
    ///
    /// # Errors
    /// Returns SQLite errors or an unsupported IRC case mapping.
    pub fn upsert_delivery_receipts(
        &self,
        network: i64,
        client: &str,
        receipts: &[DeliveryReceipt],
        cm: crate::xirc::CaseMapping,
    ) -> Result<()> {
        // Validate the mapping before beginning a write transaction.
        receipt_target_expr(cm)?;
        let client_predicate = receipt_client_predicate(client);
        let pending: HashMap<_, _> = receipts
            .iter()
            .map(|receipt| (cm(&receipt.target), receipt))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut seen = HashSet::new();
        {
            // Scan this client's durable rows once, updating matching aliases by
            // primary key. Do not rescan the entire client for every dirty target.
            let mut stmt = tx.prepare(&format!("SELECT id,target,internal_msgid FROM DeliveryReceipt WHERE network=? AND {client_predicate}"))?;
            let mut rows = stmt.query(params![network, null_string(client)])?;
            while let Some(row) = rows.next()? {
                let target = cm(&lossy_string(row, 1)?);
                if let Some(receipt) = pending.get(&target) {
                    let id: i64 = row.get(0)?;
                    if lossy_string(row, 2)? != receipt.internal_msg_id {
                        tx.execute(
                            "UPDATE DeliveryReceipt SET internal_msgid=? WHERE id=?",
                            params![receipt.internal_msg_id, id],
                        )?;
                    }
                    seen.insert(target);
                }
            }
        }
        for (target, receipt) in pending {
            if !seen.contains(&target) {
                tx.execute("INSERT INTO DeliveryReceipt(network,target,client,internal_msgid) VALUES(?,?,?,?)", params![network, target, null_string(client), receipt.internal_msg_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Loads the newest row among case aliases of a conversation/client.
    ///
    /// # Errors
    /// Returns SQLite errors or an unsupported IRC case mapping.
    pub fn load_delivery_id(
        &self,
        network: i64,
        target: &str,
        client: &str,
        cm: crate::xirc::CaseMapping,
    ) -> Result<String> {
        let expr = receipt_target_expr(cm)?;
        let client_predicate = receipt_client_predicate(client);
        Ok(self.conn().query_row(
            &format!("SELECT internal_msgid FROM DeliveryReceipt WHERE network=? AND {client_predicate} AND {expr}=? ORDER BY id DESC LIMIT 1"),
            params![network, null_string(client), cm(target)], |r| lossy_string(r, 0),
        ).optional()?.unwrap_or_default())
    }

    /// Enumerates at most 128 canonical conversations for a client. Keyset paging
    /// keeps Rust reload memory bounded; the newest legacy alias wins.
    ///
    /// # Errors
    /// Returns SQLite errors or an unsupported IRC case mapping.
    pub fn delivery_receipt_page(
        &self,
        network: i64,
        client: &str,
        cm: crate::xirc::CaseMapping,
        after: Option<&str>,
    ) -> Result<Vec<DeliveryReceipt>> {
        let expr = receipt_target_expr(cm)?;
        let client_predicate = receipt_client_predicate(client);
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT page.id, page.canonical, receipt.internal_msgid FROM
             (SELECT MAX(id) AS id, {expr} AS canonical FROM DeliveryReceipt
              WHERE network=? AND {client_predicate} AND (? IS NULL OR {expr}>?)
              GROUP BY canonical ORDER BY canonical LIMIT 128) AS page
             JOIN DeliveryReceipt AS receipt ON receipt.id=page.id ORDER BY page.canonical"
        ))?;
        Ok(stmt
            .query_map(params![network, null_string(client), after, after], |r| {
                Ok(DeliveryReceipt {
                    id: r.get(0)?,
                    target: lossy_string(r, 1)?,
                    client: client.to_owned(),
                    internal_msg_id: lossy_string(r, 2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Initializes a previously unseen target for every historical client in one
    /// atomic statement, without loading all names or overwriting existing IDs.
    ///
    /// # Errors
    /// Returns SQLite errors or an unsupported IRC case mapping.
    pub fn initialize_delivery_target(
        &self,
        network: i64,
        target: &str,
        id: &str,
        cm: crate::xirc::CaseMapping,
    ) -> Result<()> {
        let expr = receipt_target_expr(cm)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM DeliveryReceipt WHERE network=? AND {expr}=?)"),
            params![network, cm(target)],
            |row| row.get(0),
        )?;
        if !exists {
            tx.execute(
                "INSERT INTO DeliveryReceipt(network,target,client,internal_msgid)
                 SELECT ?,?,clients.client,? FROM (SELECT DISTINCT NULLIF(soju_lossy(client),'') AS client FROM DeliveryReceipt WHERE network=?) AS clients",
                params![network, cm(target), id, network],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Tests whether any client already tracks a canonical target.
    ///
    /// # Errors
    /// Returns SQLite errors or an unsupported IRC case mapping.
    pub fn has_delivery_target(
        &self,
        network: i64,
        target: &str,
        cm: crate::xirc::CaseMapping,
    ) -> Result<bool> {
        let expr = receipt_target_expr(cm)?;
        Ok(self.conn().query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM DeliveryReceipt WHERE network=? AND {expr}=?)"),
            params![network, cm(target)],
            |r| r.get(0),
        )?)
    }

    /// Counts canonical targets for the existing client-supplied-target cap.
    ///
    /// # Errors
    /// Returns SQLite errors or an unsupported IRC case mapping.
    pub fn delivery_target_count(
        &self,
        network: i64,
        cm: crate::xirc::CaseMapping,
    ) -> Result<usize> {
        let expr = receipt_target_expr(cm)?;
        let count: i64 = self.conn().query_row(
            &format!("SELECT COUNT(DISTINCT {expr}) FROM DeliveryReceipt WHERE network=?"),
            [network],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
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

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
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
        let ids = {
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

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn get_read_receipt(&self, network_id: i64, name: &str) -> Result<Option<ReadReceipt>> {
        let conn = self.conn();
        let res = conn
            .query_row(
                "SELECT id, timestamp FROM ReadReceipt WHERE network = :network AND target = \
                    :target",
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

    ///
    /// # Errors
    ///
    /// Returns SQLite errors, constraint failures, or errors decoding stored records.
    pub fn store_read_receipt(&self, network_id: i64, receipt: &mut ReadReceipt) -> Result<()> {
        let conn = self.conn();
        let ts = time_to_sql(receipt.timestamp);
        if receipt.id != 0 {
            conn.execute(
                "UPDATE ReadReceipt SET timestamp = :timestamp WHERE id = :id AND (timestamp IS \
                    NOT :timestamp)",
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
