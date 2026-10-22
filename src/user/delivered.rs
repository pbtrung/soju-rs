//! Delivery receipts: the last message each client received.

use std::collections::HashMap;

use super::User;
use crate::database as db;
use crate::logf;
use crate::xirc::{CaseMapping, CaseMappingMap};

/// Delivery receipts: target -> client name -> internal message ID.
pub struct DeliveredStore {
    pub m: CaseMappingMap<HashMap<String, String>>,
}

impl DeliveredStore {
    pub fn new(cm: CaseMapping) -> DeliveredStore {
        DeliveredStore {
            m: CaseMappingMap::new(cm),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.m.is_empty()
    }

    pub fn has_target(&self, target: &str) -> bool {
        self.m.has(target)
    }

    /// The number of targets with receipts.
    pub fn len(&self) -> usize {
        self.m.len()
    }

    pub fn load_id(&self, target: &str, client: &str) -> String {
        self.m
            .get(target)
            .and_then(|c| c.get(client))
            .cloned()
            .unwrap_or_default()
    }

    /// Stores the ID of the last message a client received from a target.
    /// This runs for each message delivered to clients without
    /// chathistory: existing entries are updated in place.
    pub fn store_id(&mut self, target: &str, client: &str, msg_id: &str) {
        let Some(clients) = self.m.get_mut(target) else {
            let clients = HashMap::from([(client.to_owned(), msg_id.to_owned())]);
            self.m.set(target, clients);
            return;
        };
        match clients.get_mut(client) {
            Some(id) => {
                id.clear();
                id.push_str(msg_id);
            }
            None => {
                clients.insert(client.to_owned(), msg_id.to_owned());
            }
        }
    }

    pub fn targets(&self) -> Vec<String> {
        self.m.keys().map(str::to_owned).collect()
    }

    pub fn clients(&self) -> Vec<String> {
        let mut clients: Vec<String> = Vec::new();
        for c in self.m.values() {
            for name in c.keys() {
                if !clients.contains(name) {
                    clients.push(name.clone());
                }
            }
        }
        clients
    }
}

impl User {
    pub fn store_client_delivery_receipts(&mut self, ni: usize, client: &str) {
        if !self.has_persistent_msg_store() {
            return;
        }
        let net = &self.networks[ni];
        let mut receipts: Vec<db::DeliveryReceipt> = net
            .delivered
            .m
            .iter()
            .filter_map(|(target, clients)| {
                let msg_id = clients.get(client).filter(|id| !id.is_empty())?;
                Some(db::DeliveryReceipt {
                    target: target.to_owned(),
                    internal_msg_id: msg_id.clone(),
                    ..Default::default()
                })
            })
            .collect();
        if let Err(e) = self
            .srv
            .db
            .store_client_delivery_receipts(net.id(), client, &mut receipts)
        {
            logf!(
                net.logger,
                "failed to store delivery receipts for client {client:?}: {e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xirc::CASEMAPPING_RFC1459;

    #[test]
    fn store() {
        let mut d = DeliveredStore::new(CASEMAPPING_RFC1459);
        d.store_id("#Chan", "phone", "1");
        d.store_id("#chan", "phone", "22");
        d.store_id("#chan", "laptop", "3");
        assert_eq!(d.len(), 1);
        assert_eq!(d.load_id("#CHAN", "phone"), "22");
        assert_eq!(d.load_id("#chan", "laptop"), "3");
        assert_eq!(d.load_id("#chan", "tablet"), "");
        let mut clients = d.clients();
        clients.sort();
        assert_eq!(clients, ["laptop", "phone"]);
    }
}
