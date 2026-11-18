//! Actor-owned mention coalescing and durable stop-time draining.

use std::time::Duration;

use jiff::Timestamp;
use tokio::time::Instant;

use super::User;
use crate::{database, logf};

const FLUSH_DELAY: Duration = Duration::from_secs(1);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const STOP_ATTEMPTS: usize = 3;

impl User {
    pub(crate) fn record_mention(&mut self, ni: usize, target: String, time: Timestamp) {
        let Some(network) = self.networks.get_mut(ni) else {
            return;
        };
        let pending = network.pending_mentions.entry(target).or_insert(time);
        *pending = (*pending).max(time);
        network
            .mention_deadline
            .get_or_insert(Instant::now() + FLUSH_DELAY);
    }

    pub(super) fn next_mention_deadline(&self) -> Option<Instant> {
        self.networks
            .iter()
            .filter_map(|network| network.mention_deadline)
            .min()
    }

    pub(super) async fn flush_mentions(&mut self, all: bool) {
        for network in &mut self.networks {
            if network.pending_mentions.is_empty() {
                network.mention_deadline = None;
                continue;
            }
            if !all
                && network
                    .mention_deadline
                    .is_none_or(|deadline| deadline > Instant::now())
            {
                continue;
            }
            let mentions = network.pending_mentions.clone();
            let id = network.id();
            let result = database::run(&self.srv.db, move |db| {
                db.store_message_target_highlights(
                    id,
                    mentions
                        .iter()
                        .map(|(target, time)| (target.as_str(), *time)),
                )
            })
            .await;
            // Only this actor can modify the map, and no events are processed
            // across this await. Clear only after the blocking batch has joined.
            match result {
                Ok(()) => {
                    network.pending_mentions.clear();
                    network.mention_deadline = None;
                }
                Err(error) => {
                    crate::metrics::inc(&self.srv.metrics.mention_flush_errors_total);
                    logf!(
                        network.logger,
                        "failed to record mentions; retaining batch for retry: {error}"
                    );
                    network.mention_deadline = Some(Instant::now() + RETRY_DELAY);
                }
            }
        }
    }

    pub(super) async fn flush_mentions_on_stop(&mut self) {
        for attempt in 0..STOP_ATTEMPTS {
            self.flush_mentions(true).await;
            if self
                .networks
                .iter()
                .all(|network| network.pending_mentions.is_empty())
            {
                return;
            }
            if attempt + 1 < STOP_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        crate::metrics::inc(&self.srv.metrics.mention_shutdown_failures_total);
        logf!(
            self.logger,
            "ERROR: stopping with unpersisted mentions after {STOP_ATTEMPTS} attempts; durable mention state may be incomplete"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, Network, User as UserRecord};
    use crate::logger::Logger;
    use crate::server::Server;
    use crate::user::{Network as UserNetwork, UserHandle};
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn fixture() -> anyhow::Result<(User, rusqlite::Connection, std::path::PathBuf)> {
        let path =
            std::env::temp_dir().join(format!("soju-mentions-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path)?;
        let file = path.join("mentions.db");
        let server = Server::new(
            Database::open(
                file.to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?,
            )?,
            Logger::new(false),
        );
        let mut record = UserRecord::new("mentions");
        server.db.store_user(&mut record)?;
        let mut network = Network::new("irc.example");
        server.db.store_network(record.id, &mut network)?;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = UserHandle {
            tx: tx.clone(),
            done: CancellationToken::new(),
            num_downstream_conns: Arc::new(AtomicI64::new(0)),
            record: Arc::new(Mutex::new(record.clone())),
        };
        let mut actor = User::new(server, record, tx, &handle);
        actor
            .networks
            .push(UserNetwork::new(&actor.logger, network, Vec::new()));
        Ok((actor, rusqlite::Connection::open(file)?, path))
    }

    #[tokio::test]
    async fn failed_batch_is_retained_merged_and_atomically_retried() -> anyhow::Result<()> {
        let (mut actor, connection, path) = fixture()?;
        connection.execute_batch("CREATE TRIGGER fail_mention BEFORE INSERT ON TargetHighlight WHEN NEW.target='#fail' BEGIN SELECT RAISE(ABORT,'injected mention failure'); END;")?;
        let first = Timestamp::from_second(10)?;
        let latest = Timestamp::from_second(20)?;
        actor.record_mention(0, "#a".into(), first);
        actor.record_mention(0, "#fail".into(), first);
        actor.flush_mentions(true).await;
        assert_eq!(
            actor
                .networks
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing network"))?
                .pending_mentions
                .len(),
            2
        );
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM TargetHighlight", [], |r| r.get(0))?;
        assert_eq!(count, 0, "a failed batch must roll back every target");
        actor.record_mention(0, "#fail".into(), latest);
        actor.record_mention(0, "#fail".into(), first);
        connection.execute_batch("DROP TRIGGER fail_mention;")?;
        actor.flush_mentions_on_stop().await;
        assert!(
            actor
                .networks
                .iter()
                .all(|network| network.pending_mentions.is_empty())
        );
        let time: String = connection.query_row(
            "SELECT time FROM TargetHighlight WHERE target='#fail'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(crate::xirc::parse_server_time(&time), Some(latest));
        actor.record_mention(0, "#fail".into(), first);
        actor.flush_mentions_on_stop().await;
        let stored: String = connection.query_row(
            "SELECT time FROM TargetHighlight WHERE target='#fail'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(crate::xirc::parse_server_time(&stored), Some(latest));
        assert_eq!(
            actor
                .srv
                .metrics
                .mention_flush_errors_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            actor
                .srv
                .metrics
                .mention_shutdown_failures_total
                .load(Ordering::Relaxed),
            0
        );
        actor.notifications.shutdown().await;
        drop((actor, connection));
        std::fs::remove_dir_all(path)?;
        Ok(())
    }

    #[tokio::test]
    async fn deletion_while_batch_waits_cannot_resurrect_mentions() -> anyhow::Result<()> {
        let (mut actor, connection, path) = fixture()?;
        let network = actor
            .networks
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing network"))?
            .id();
        actor.record_mention(0, "#deleted".into(), Timestamp::UNIX_EPOCH);
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        let flush = tokio::spawn(async move {
            actor.flush_mentions(true).await;
            actor
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !flush.is_finished(),
            "writer lock must keep the batch in flight"
        );
        connection.execute("DELETE FROM Network WHERE id=?", [network])?;
        connection.execute_batch("COMMIT;")?;
        let mut actor = flush.await?;
        assert_eq!(
            actor
                .srv
                .metrics
                .mention_flush_errors_total
                .load(Ordering::Relaxed),
            1
        );
        actor.delete_network(network)?;
        actor.flush_mentions_on_stop().await;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM TargetHighlight", [], |r| r.get(0))?;
        assert_eq!(count, 0);
        actor.notifications.shutdown().await;
        drop((actor, connection));
        std::fs::remove_dir_all(path)?;
        Ok(())
    }

    #[tokio::test]
    async fn permanent_shutdown_failure_is_reported_after_bounded_attempts() -> anyhow::Result<()> {
        let (mut actor, connection, path) = fixture()?;
        connection.execute_batch("CREATE TRIGGER fail_mention BEFORE INSERT ON TargetHighlight BEGIN SELECT RAISE(ABORT,'persistent failure'); END;")?;
        actor.record_mention(0, "#pending".into(), Timestamp::UNIX_EPOCH);
        actor.flush_mentions_on_stop().await;
        assert_eq!(
            actor
                .srv
                .metrics
                .mention_flush_errors_total
                .load(Ordering::Relaxed),
            3
        );
        assert_eq!(
            actor
                .srv
                .metrics
                .mention_shutdown_failures_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            actor
                .networks
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing network"))?
                .pending_mentions
                .len(),
            1
        );
        actor.notifications.shutdown().await;
        drop((actor, connection));
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
