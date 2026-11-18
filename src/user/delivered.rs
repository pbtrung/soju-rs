//! Delivery receipts: bounded dirty caches backed by lazy durable cursors.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::User;
use crate::database::{Database, DeliveryReceipt};
use crate::logf;
use crate::xirc::{CaseMapping, CaseMappingMap};

const CACHE_ENTRIES: usize = 4096;
const CACHE_BYTES: usize = 4 * 1024 * 1024;

/// Accounted dirty-cache usage shared across a user's persistent networks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiptUsage {
    pub entries: usize,
    pub bytes: usize,
}

#[derive(Default)]
pub(crate) struct ReceiptBudget(Mutex<ReceiptUsage>);

#[derive(Clone)]
struct Backend {
    db: Arc<Database>,
    network: i64,
}

/// Target/client cursors. Persistent stores share a bounded dirty cache and
/// lazily read uncached rows. Memory-only receipts retain their existing lifetime
/// semantics; they are not part of the memory-history byte budget.
pub struct DeliveredStore {
    m: CaseMappingMap<HashMap<String, String>>,
    cm: CaseMapping,
    backend: Option<Backend>,
    budget: Arc<ReceiptBudget>,
    usage: ReceiptUsage,
}

impl DeliveredStore {
    pub fn new(cm: CaseMapping) -> Self {
        Self {
            m: CaseMappingMap::new(cm),
            cm,
            backend: None,
            budget: Arc::default(),
            usage: ReceiptUsage::default(),
        }
    }

    pub(crate) fn attach(&mut self, db: Arc<Database>, network: i64, budget: Arc<ReceiptBudget>) {
        // Network construction attaches before any delivery. Updated networks
        // already carry the old store and must keep its shared accounting.
        if self.backend.is_none() {
            self.backend = Some(Backend { db, network });
            self.budget = budget;
        }
    }

    #[must_use]
    pub fn usage(&self) -> ReceiptUsage {
        self.usage
    }

    /// Whether the in-memory cache has no target rows; durable rows may exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.m.is_empty()
    }

    /// Number of cached targets; this excludes uncached durable targets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.m.len()
    }

    /// Tests cached and durable targets.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn has_target(&self, target: &str) -> Result<bool> {
        if self.m.has(target) {
            return Ok(true);
        }
        self.backend.as_ref().map_or(Ok(false), |b| {
            b.db.has_delivery_target(b.network, target, self.cm)
        })
    }

    /// Enforces the existing client-supplied target cap across cached/durable rows.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn can_track_target(&mut self, target: &str, limit: usize) -> Result<bool> {
        if self.has_target(target)? {
            return Ok(true);
        }
        self.flush_all()?;
        let count = self.backend.as_ref().map_or(Ok(self.m.len()), |b| {
            b.db.delivery_target_count(b.network, self.cm)
        })?;
        Ok(count < limit)
    }

    /// Reads cached dirty state first, then an uncached durable cursor.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn load_id(&self, target: &str, client: &str) -> Result<String> {
        if let Some(id) = self.m.get(target).and_then(|c| c.get(client)) {
            return Ok(id.clone());
        }
        self.backend.as_ref().map_or_else(
            || Ok(String::new()),
            |b| b.db.load_delivery_id(b.network, target, client, self.cm),
        )
    }

    // Conservatively charge each entry for its own two target spellings and
    // map/string metadata, even when a target is shared by several clients.
    fn entry_bytes(target: &str, client: &str, id: &str) -> usize {
        256usize
            .saturating_add(target.len().saturating_mul(2))
            .saturating_add(client.len())
            .saturating_add(id.len())
    }

    /// Stores the last delivered ID. Persistent pressure flushes before release;
    /// an entry too large for cache is written through. A failed write preserves
    /// all previously cached cursors and returns an error to the caller.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn store_id(&mut self, target: &str, client: &str, msg_id: &str) -> Result<()> {
        let bytes = Self::entry_bytes(target, client, msg_id);
        let old = self
            .m
            .get(target)
            .and_then(|c| c.get(client))
            .map(|id| Self::entry_bytes(target, client, id));
        if self.backend.is_some() && !self.fits(old, bytes) {
            self.flush_all()?;
            if !self.fits(None, bytes) {
                if let Some(b) = &self.backend {
                    b.db.upsert_delivery_receipts(
                        b.network,
                        client,
                        &[DeliveryReceipt {
                            target: target.to_owned(),
                            internal_msg_id: msg_id.to_owned(),
                            ..Default::default()
                        }],
                        self.cm,
                    )?;
                }
                return Ok(());
            }
        }
        // A flush may have removed the original entry.
        let old = self
            .m
            .get(target)
            .and_then(|c| c.get(client))
            .map(|id| Self::entry_bytes(target, client, id));
        self.charge(old, bytes);
        if let Some(clients) = self.m.get_mut(target) {
            clients.insert(client.to_owned(), msg_id.to_owned());
        } else {
            self.m.set(
                target,
                HashMap::from([(client.to_owned(), msg_id.to_owned())]),
            );
        }
        Ok(())
    }

    fn fits(&self, old: Option<usize>, bytes: usize) -> bool {
        let used = crate::state_lock::lock(&self.budget.0);
        used.entries + usize::from(old.is_none()) <= CACHE_ENTRIES
            && used
                .bytes
                .saturating_sub(old.unwrap_or(0))
                .saturating_add(bytes)
                <= CACHE_BYTES
    }

    fn charge(&mut self, old: Option<usize>, bytes: usize) {
        let entries = usize::from(old.is_none());
        self.usage.entries += entries;
        self.usage.bytes = self.usage.bytes - old.unwrap_or(0) + bytes;
        if self.backend.is_some() {
            let mut used = crate::state_lock::lock(&self.budget.0);
            used.entries += entries;
            used.bytes = used.bytes - old.unwrap_or(0) + bytes;
        }
    }

    fn release(&mut self, usage: ReceiptUsage) {
        self.usage.entries -= usage.entries;
        self.usage.bytes -= usage.bytes;
        if self.backend.is_some() {
            let mut used = crate::state_lock::lock(&self.budget.0);
            used.entries -= usage.entries;
            used.bytes -= usage.bytes;
        }
    }

    fn receipts_for(&self, client: &str) -> Vec<DeliveryReceipt> {
        self.m
            .iter()
            .filter_map(|(target, clients)| {
                Some(DeliveryReceipt {
                    target: target.to_owned(),
                    internal_msg_id: clients.get(client)?.clone(),
                    ..Default::default()
                })
            })
            .collect()
    }

    /// Flushes and releases one client's cached rows, never replacing its full
    /// durable receipt set. Failure leaves the cache available for retry.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn flush_client(&mut self, client: &str) -> Result<()> {
        let Some(b) = &self.backend else {
            return Ok(());
        };
        let receipts = self.receipts_for(client);
        if receipts.is_empty() {
            return Ok(());
        }
        b.db.upsert_delivery_receipts(b.network, client, &receipts, self.cm)?;
        for receipt in receipts {
            let empty = if let Some(clients) = self.m.get_mut(&receipt.target) {
                clients.remove(client);
                clients.is_empty()
            } else {
                false
            };
            if empty {
                self.m.del(&receipt.target);
            }
            self.release(ReceiptUsage {
                entries: 1,
                bytes: Self::entry_bytes(&receipt.target, client, &receipt.internal_msg_id),
            });
        }
        Ok(())
    }

    /// Flushes only bounded cached rows; uncached durable cursors are untouched.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn flush_all(&mut self) -> Result<()> {
        if self.backend.is_some() {
            for client in self.clients() {
                self.flush_client(&client)?;
            }
        }
        Ok(())
    }

    /// Changes mapping after persisting dirty rows. Lazy queries merge legacy
    /// aliases using the newest row, as startup loading previously did.
    ///
    /// # Errors
    /// Returns underlying receipt database errors, leaving the old cache intact
    /// for the failed flush.
    pub fn set_casemapping(&mut self, cm: CaseMapping) -> Result<()> {
        self.flush_all()?;
        self.m.set_casemapping(cm);
        self.cm = cm;
        if self.backend.is_none() {
            self.usage = ReceiptUsage::default();
            for (target, clients) in self.m.iter() {
                for (client, id) in clients {
                    self.usage.entries += 1;
                    self.usage.bytes += Self::entry_bytes(target, client, id);
                }
            }
        }
        Ok(())
    }

    /// Initializes a new target for all historical clients without loading their names.
    ///
    /// # Errors
    /// Returns underlying receipt database errors.
    pub fn initialize_target(&mut self, target: &str, id: &str) -> Result<()> {
        self.flush_all()?;
        if let Some(b) = &self.backend {
            b.db.initialize_delivery_target(b.network, target, id, self.cm)?;
        } else {
            for client in self.clients() {
                self.store_id(target, &client, id)?;
            }
        }
        Ok(())
    }

    /// Returns owned, bounded pages for reconnect playback. Iteration does not
    /// borrow the user actor or hold a SQLite lock across playback.
    ///
    /// # Errors
    /// Returns underlying receipt database errors while flushing cached rows.
    pub fn receipts(&mut self, client: &str) -> Result<ReceiptIter> {
        self.flush_client(client)?;
        Ok(ReceiptIter {
            backend: self.backend.clone(),
            cm: self.cm,
            client: client.to_owned(),
            after: None,
            pending: if self.backend.is_none() {
                self.receipts_for(client)
            } else {
                Vec::new()
            }
            .into_iter(),
            done: false,
        })
    }

    /// Distinct cached client names in first-encounter order, using linear-time
    /// set membership instead of repeatedly scanning an ever-growing vector.
    #[must_use]
    pub fn clients(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.m
            .values()
            .flat_map(HashMap::keys)
            .filter(|name| seen.insert(name.as_str()))
            .cloned()
            .collect()
    }
}

impl Drop for DeliveredStore {
    fn drop(&mut self) {
        self.release(self.usage);
    }
}

/// Lazy durable receipt pages, or the existing memory-only receipt snapshot.
pub struct ReceiptIter {
    backend: Option<Backend>,
    cm: CaseMapping,
    client: String,
    after: Option<String>,
    pending: std::vec::IntoIter<DeliveryReceipt>,
    done: bool,
}
impl Iterator for ReceiptIter {
    type Item = Result<DeliveryReceipt>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(row) = self.pending.next() {
            return Some(Ok(row));
        }
        if self.done {
            return None;
        }
        let Some(b) = &self.backend else {
            self.done = true;
            return None;
        };
        match b
            .db
            .delivery_receipt_page(b.network, &self.client, self.cm, self.after.as_deref())
        {
            Ok(rows) => {
                self.done = rows.len() < 128;
                self.after = rows.last().map(|r| r.target.clone());
                self.pending = rows.into_iter();
                self.pending.next().map(Ok)
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl User {
    pub fn store_client_delivery_receipts(&mut self, ni: usize, client: &str) {
        if let Some(net) = self.networks.get_mut(ni)
            && let Err(error) = net.delivered.flush_client(client)
        {
            logf!(
                net.logger,
                "failed to store delivery receipts for client {client:?}: {error}"
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::xirc::CASEMAPPING_RFC1459;

    #[test]
    fn store() -> Result<()> {
        let mut d = DeliveredStore::new(CASEMAPPING_RFC1459);
        d.store_id("#Chan", "phone", "1")?;
        d.store_id("#chan", "phone", "22")?;
        d.store_id("#chan", "laptop", "3")?;
        assert_eq!(d.len(), 1);
        assert_eq!(d.load_id("#CHAN", "phone")?, "22");
        assert_eq!(d.load_id("#chan", "laptop")?, "3");
        assert_eq!(d.load_id("#chan", "tablet")?, "");
        let mut clients = d.clients();
        clients.sort();
        assert_eq!(clients, ["laptop", "phone"]);
        Ok(())
    }
    #[test]
    fn shared_budget_flushes_before_release_and_reloads_lazily() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("soju-receipts-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path)?;
        let file = path.join("receipts.db");
        let source = file
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 fixture path"))?;
        let db = Arc::new(Database::open(source)?);
        let mut user = crate::database::User::new("receipt-budget");
        db.store_user(&mut user)?;
        let mut a = crate::database::Network::new("a.example");
        let mut b = crate::database::Network::new("b.example");
        db.store_network(user.id, &mut a)?;
        db.store_network(user.id, &mut b)?;
        let budget = Arc::new(ReceiptBudget::default());
        let mut first = DeliveredStore::new(CASEMAPPING_RFC1459);
        let mut second = DeliveredStore::new(CASEMAPPING_RFC1459);
        first.attach(db.clone(), a.id, budget.clone());
        second.attach(db.clone(), b.id, budget.clone());
        for i in 0..CACHE_ENTRIES {
            first.store_id(&format!("#target{i:05}"), "phone", &format!("cursor{i}"))?;
        }
        assert_eq!(crate::state_lock::lock(&budget.0).entries, CACHE_ENTRIES);
        // Another network owns the entire budget: write through without growing
        // cache or evicting the other network's dirty cursors.
        for i in 0..256 {
            second.store_id(&format!("#target{i:05}"), "laptop", "other-network")?;
        }
        assert_eq!(second.usage(), ReceiptUsage::default());
        for i in CACHE_ENTRIES..10_000 {
            first.store_id(&format!("#target{i:05}"), "phone", &format!("cursor{i}"))?;
            let usage = *crate::state_lock::lock(&budget.0);
            assert!(usage.entries <= CACHE_ENTRIES && usage.bytes <= CACHE_BYTES);
        }
        first.flush_client("phone")?;
        assert_eq!(*crate::state_lock::lock(&budget.0), ReceiptUsage::default());
        drop((first, second, db));
        let db = Arc::new(Database::open(source)?);
        let mut reopened = DeliveredStore::new(CASEMAPPING_RFC1459);
        reopened.attach(db.clone(), a.id, Arc::default());
        assert_eq!(reopened.usage(), ReceiptUsage::default());
        let mut count = 0;
        for row in reopened.receipts("phone")? {
            let row = row?;
            assert_eq!(row.internal_msg_id, format!("cursor{count}"));
            count += 1;
            assert_eq!(reopened.usage(), ReceiptUsage::default());
        }
        assert_eq!(count, 10_000);
        assert_eq!(reopened.load_id("#TARGET00017", "phone")?, "cursor17");
        assert_eq!(reopened.load_id("#target00017", "laptop")?, "");
        // One cached update must not replace the client's 9,999 uncached rows.
        reopened.store_id("#target00017", "phone", "advanced")?;
        reopened.flush_client("phone")?;
        assert_eq!(
            reopened
                .receipts("phone")?
                .collect::<Result<Vec<_>>>()?
                .len(),
            10_000
        );
        reopened.initialize_target("#new", "before-first-message")?;
        assert_eq!(reopened.load_id("#new", "phone")?, "before-first-message");
        assert_eq!(reopened.load_id("#new", "laptop")?, "");
        assert!(!reopened.can_track_target("#beyond-cap", 10_000)?);
        assert!(reopened.can_track_target("#new", 10_000)?);
        drop((reopened, db));
        std::fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn byte_pressure_and_oversized_write_through_preserve_cursors() -> Result<()> {
        let db = Arc::new(Database::open_in_memory()?);
        let mut user = crate::database::User::new("receipt-bytes");
        db.store_user(&mut user)?;
        let mut network = crate::database::Network::new("irc.example");
        db.store_network(user.id, &mut network)?;
        let budget = Arc::new(ReceiptBudget::default());
        let mut store = DeliveredStore::new(CASEMAPPING_RFC1459);
        store.attach(db.clone(), network.id, budget.clone());
        for i in 0..1000 {
            store.store_id(&format!("#target{i}"), "phone", &"x".repeat(8192))?;
            let usage = *crate::state_lock::lock(&budget.0);
            assert!(usage.entries <= CACHE_ENTRIES && usage.bytes <= CACHE_BYTES);
        }
        store.store_id("#huge", "phone", &"z".repeat(CACHE_BYTES + 1))?;
        assert_eq!(store.usage(), ReceiptUsage::default());
        assert_eq!(store.load_id("#huge", "phone")?.len(), CACHE_BYTES + 1);
        assert_eq!(
            store.receipts("phone")?.collect::<Result<Vec<_>>>()?.len(),
            1001
        );
        store.store_id("#drop", "phone", "pending")?;
        drop(store);
        assert_eq!(*crate::state_lock::lock(&budget.0), ReceiptUsage::default());
        Ok(())
    }
}
