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

    pub fn store_id(&mut self, target: &str, client: &str, msg_id: &str) {
        if !self.m.has(target) {
            self.m.set(target, HashMap::new());
        }
        self.m
            .get_mut(target)
            .unwrap()
            .insert(client.to_owned(), msg_id.to_owned());
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
            .targets()
            .into_iter()
            .filter_map(|target| {
                let msg_id = net.delivered.load_id(&target, client);
                if msg_id.is_empty() {
                    return None;
                }
                Some(db::DeliveryReceipt {
                    target,
                    internal_msg_id: msg_id,
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
