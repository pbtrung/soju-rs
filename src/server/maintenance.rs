//! Inactive-account handling, message expiry and SQLite file maintenance.

use super::Server;
use crate::config::MsgStoreDriver;
use crate::database as db;
use crate::user::{Event, UserHandle};
use crate::{logf, xirc};
use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

/// Message expiry deletes this many messages at a time, pausing for
/// EXPIRY_CHUNK_PAUSE in between, so that users waiting on the database
/// aren't blocked for the whole deletion.
const EXPIRY_CHUNK_SIZE: usize = 2000;
const EXPIRY_CHUNK_PAUSE: Duration = Duration::from_millis(10);

impl Server {
    /// Deletes expired messages and vacuums local SQLite, before start:
    /// rewriting the whole database holds it for a while, and no user must
    /// wait on it, nor the only runtime thread on a single CPU.
    pub async fn startup_maintenance(self: &Arc<Self>) {
        self.run_message_expiry(true).await;
    }

    pub(super) async fn run_message_expiry(self: &Arc<Self>, vacuum: bool) {
        // Deleting and vacuuming can take a while: keep it off the async
        // runtime's workers
        let s = self.clone();
        match tokio::task::spawn_blocking(move || s.delete_expired_messages(vacuum)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => logf!(self.logger, "failed to delete expired messages: {e}"),
            Err(e) => logf!(self.logger, "failed to delete expired messages: {e}"),
        }
    }

    pub(super) async fn disable_inactive_users(&self) -> Result<()> {
        let delay = self.config().basic.disable_inactive_users_delay;
        if delay.is_zero() {
            return Ok(());
        }
        let limit = Utc::now() - chrono::Duration::from_std(delay)?;
        let usernames = db::run(&self.db, move |db| db.list_inactive_usernames(limit))
            .await
            .map_err(|e| anyhow!("failed to list inactive users: {e}"))?;
        if usernames.is_empty() {
            return Ok(());
        }

        // Filter out users with active downstream connections
        let users: Vec<(String, UserHandle)> = {
            let map = self.users.lock().unwrap();
            usernames
                .into_iter()
                .filter_map(|name| {
                    // TODO: disable the user in the DB if it isn't running
                    let h = map.get(&name)?;
                    if h.num_downstream_conns.load(Ordering::SeqCst) > 0 {
                        return None;
                    }
                    Some((name, h.clone()))
                })
                .collect()
        };
        if users.is_empty() {
            return Ok(());
        }

        logf!(self.logger, "found {} inactive users", users.len());
        for (name, h) in users {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = h.send(Event::UserUpdate {
                password: None,
                admin: None,
                enabled: Some(false),
                max_networks: None,
                done: tx,
            });
            if !sent {
                // The user stopped meanwhile, e.g. it was deleted
                continue;
            }
            match tokio::time::timeout(Duration::from_secs(60), rx).await {
                Ok(Ok(Ok(()))) => logf!(self.logger, "disabled inactive user {name:?}"),
                Ok(Ok(Err(e))) => bail!(e),
                _ => bail!("context deadline exceeded"),
            }
        }
        Ok(())
    }

    /// Deletes expired messages, and vacuums local SQLite if asked: on
    /// startup, before users start, and when message-expiry changes, which
    /// may leave at least 25% of the database free. Otherwise, the space freed each
    /// day is reused by the next day's messages, and rewriting the database
    /// would hold it meanwhile.
    pub(crate) fn delete_expired_messages(&self, vacuum: bool) -> Result<()> {
        let cfg = self.config();
        let expiry = cfg.basic.message_expiry;
        if expiry.is_zero() {
            return Ok(());
        }
        let before = Utc::now() - chrono::Duration::from_std(expiry)?;
        let before_str = xirc::format_server_time(before);
        logf!(
            self.logger,
            "soju-rs v{}: running message expiry",
            crate::VERSION
        );
        match cfg.basic.msg_store.driver {
            MsgStoreDriver::Db => {
                let n = self
                    .db
                    .expire_messages(before, EXPIRY_CHUNK_SIZE, EXPIRY_CHUNK_PAUSE, &|| {
                        self.stop.is_cancelled()
                    })
                    .map_err(|e| anyhow!("failed to delete expired messages from db: {e:#}"))?;
                logf!(
                    self.logger,
                    "deleted {n} expired messages older than {before_str} from db"
                );
                if n > 0 {
                    let start = Instant::now();
                    self.db
                        .optimize()
                        .map_err(|e| anyhow!("failed to optimize db: {e}"))?;
                    logf!(
                        self.logger,
                        "optimized db in {:.1}s",
                        start.elapsed().as_secs_f64()
                    );
                }
                if vacuum {
                    let free = self
                        .db
                        .free_page_ratio()
                        .map_err(|e| anyhow!("failed to get db free space: {e}"))?;
                    if free >= 0.25 {
                        let start = Instant::now();
                        self.db
                            .vacuum()
                            .map_err(|e| anyhow!("failed to vacuum db: {e}"))?;
                        logf!(
                            self.logger,
                            "vacuumed db in {:.1}s ({:.0}% of it was free)",
                            start.elapsed().as_secs_f64(),
                            free * 100.0
                        );
                    }
                }
                {
                    self.db
                        .shrink_memory()
                        .map_err(|e| anyhow!("failed to shrink db memory: {e}"))?;
                }
            }
            MsgStoreDriver::Memory => {}
        }
        Ok(())
    }
}
