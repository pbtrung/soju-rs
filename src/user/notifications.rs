//! Bounded, coalescing notification admission. IRC delivery never waits here.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use jiff::Timestamp;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::irc::Message;
use crate::logger::Logger;
use crate::metrics::PushMetrics;
use crate::server::Server;
use crate::xirc::CaseMappingMap;

const MAX_PENDING: usize = 128;
const MAX_PENDING_BYTES: usize = 1024 * 1024;

pub(super) struct Job {
    pub network: i64,
    pub target: String,
    pub message: Message,
    pub mention_time: Option<Timestamp>,
    pub push_targets: Arc<Mutex<CaseMappingMap<Timestamp>>>,
    pub network_stop: CancellationToken,
    pub logger: Logger,
    deadline: Instant,
    bytes: usize,
}

impl Job {
    pub(super) fn new(
        network: &super::Network,
        target: String,
        message: Message,
        mention_time: Option<Timestamp>,
    ) -> Self {
        let bytes = message
            .retained_size()
            .saturating_add(target.len().saturating_mul(2))
            .saturating_add(512);
        Self {
            network: network.id(),
            target,
            message,
            mention_time,
            push_targets: network.push_targets.clone(),
            network_stop: network.stop.clone(),
            logger: network.logger.clone(),
            deadline: Instant::now()
                + if mention_time.is_some() {
                    Duration::from_secs(5)
                } else {
                    Duration::ZERO
                },
            bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Admission {
    Queued,
    Coalesced,
    Overflow,
    Stopped,
}

#[derive(Default)]
struct Pending {
    jobs: IndexMap<(i64, String), Job>,
    bytes: usize,
}

struct Queue {
    pending: Mutex<Pending>,
    wake: Notify,
    stop: CancellationToken,
    metrics: Arc<PushMetrics>,
}

impl Queue {
    fn admit(&self, mut job: Job) -> Admission {
        let mut pending = crate::state_lock::lock(&self.pending);
        if self.stop.is_cancelled() {
            return Admission::Stopped;
        }
        let key = (job.network, job.target.clone());
        let old = pending.jobs.get(&key);
        let replaced = old.is_some();
        let bytes = pending
            .bytes
            .saturating_sub(old.map_or(0, |job| job.bytes))
            .saturating_add(job.bytes);
        if bytes > MAX_PENDING_BYTES || (!replaced && pending.jobs.len() >= MAX_PENDING) {
            crate::metrics::inc(&self.metrics.overflow_total);
            return Admission::Overflow;
        }
        if let Some(old) = old
            && old.mention_time.is_some()
            && job.mention_time.is_some()
        {
            // Consecutive messages share the first five-second window. A new
            // unread message after MARKREAD starts its own delay.
            job.deadline = old.deadline.min(job.deadline);
        }
        pending.bytes = bytes;
        pending.jobs.insert(key, job);
        if replaced {
            crate::metrics::inc(&self.metrics.coalesced_total);
        } else {
            self.metrics.pending.fetch_add(1, Ordering::Relaxed);
        }
        drop(pending);
        self.wake.notify_one();
        if replaced {
            Admission::Coalesced
        } else {
            Admission::Queued
        }
    }

    fn deadline(&self) -> Option<Instant> {
        crate::state_lock::lock(&self.pending)
            .jobs
            .values()
            .map(|job| job.deadline)
            .min()
    }

    fn take_ready(&self) -> Option<Job> {
        let mut pending = crate::state_lock::lock(&self.pending);
        let index = pending
            .jobs
            .iter()
            .position(|(_, job)| job.deadline <= Instant::now())?;
        let (_, job) = pending.jobs.swap_remove_index(index)?;
        pending.bytes -= job.bytes;
        self.metrics.pending.fetch_sub(1, Ordering::Relaxed);
        Some(job)
    }

    fn cancel_network(&self, network: Option<i64>) {
        let mut pending = crate::state_lock::lock(&self.pending);
        let before = pending.jobs.len();
        pending
            .jobs
            .retain(|(id, _), _| network.is_some_and(|network| *id != network));
        pending.bytes = pending.jobs.values().map(|job| job.bytes).sum();
        // Counts are bounded by MAX_PENDING, so this signed gauge conversion fits.
        if let Ok(removed) = i64::try_from(before - pending.jobs.len()) {
            self.metrics.pending.fetch_sub(removed, Ordering::Relaxed);
        }
    }
}

pub(super) struct Notifications {
    queue: Arc<Queue>,
    task: Option<JoinHandle<()>>,
}

impl Notifications {
    pub(super) fn new(
        srv: Arc<Server>,
        user: i64,
        tx: tokio::sync::mpsc::UnboundedSender<super::Event>,
    ) -> Self {
        let queue = Arc::new(Queue {
            pending: Mutex::default(),
            wake: Notify::new(),
            stop: CancellationToken::new(),
            metrics: srv.metrics.push.clone(),
        });
        let task = tokio::spawn(run_worker(queue.clone(), move |job| {
            let srv = srv.clone();
            let tx = tx.clone();
            async move {
                deliver(&srv, user, &tx, job).await;
            }
        }));
        Self {
            queue,
            task: Some(task),
        }
    }

    pub(super) fn admit(&self, job: Job) -> Admission {
        self.queue.admit(job)
    }
    pub(super) fn stopped(&self) -> CancellationToken {
        self.queue.stop.clone()
    }
    pub(super) fn cancel_network(&self, network: i64) {
        self.queue.cancel_network(Some(network));
    }
    pub(super) fn cancel(&self) {
        self.queue.stop.cancel();
        self.queue.cancel_network(None);
    }
    pub(super) async fn shutdown(&mut self) {
        self.cancel();
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            crate::logf!(Logger::new(false), "notification worker failed: {error}");
        }
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        self.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_worker<F, Fut>(queue: Arc<Queue>, deliver: F)
where
    F: Fn(Job) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    struct DrainOnExit(Arc<Queue>);
    impl Drop for DrainOnExit {
        fn drop(&mut self) {
            self.0.cancel_network(None);
        }
    }
    let _drain = DrainOnExit(queue.clone());
    loop {
        if queue.stop.is_cancelled() {
            return;
        }
        if let Some(job) = queue.take_ready() {
            let network_stop = job.network_stop.clone();
            if network_stop.is_cancelled() {
                continue;
            }
            tokio::select! {
                biased;
                () = queue.stop.cancelled() => return,
                () = network_stop.cancelled() => {},
                () = crate::metrics::with_gauge(&queue.metrics.active, deliver(job)) => {},
            }
            continue;
        }
        let wake = queue.wake.notified();
        if let Some(deadline) = queue.deadline() {
            tokio::select! {
                () = queue.stop.cancelled() => return,
                () = wake => {},
                () = tokio::time::sleep_until(deadline) => {},
            }
        } else {
            tokio::select! {
                () = queue.stop.cancelled() => return,
                () = wake => {},
            }
        }
    }
}

async fn should_notify(
    db: &Arc<crate::database::Database>,
    network: i64,
    target: String,
    timestamp: Timestamp,
) -> anyhow::Result<bool> {
    let receipt = crate::database::run(db, move |db| db.get_read_receipt(network, &target)).await?;
    Ok(receipt
        .and_then(|receipt| receipt.timestamp)
        .is_none_or(|read| read < timestamp))
}

async fn deliver(
    srv: &Arc<Server>,
    user: i64,
    tx: &tokio::sync::mpsc::UnboundedSender<super::Event>,
    job: Job,
) {
    if let Some(timestamp) = job.mention_time {
        match should_notify(&srv.db, job.network, job.target.clone(), timestamp).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                crate::logf!(
                    job.logger,
                    "failed to check notification read receipt: {error}"
                );
                return;
            }
        }
    }
    let subs = super::webpush::web_push_subscriptions(srv, user, job.network, &job.logger).await;
    if subs.is_empty() {
        return;
    }
    if let Some(time) = job.mention_time {
        crate::state_lock::lock(&job.push_targets).set(&job.target, time);
    }
    if super::webpush::send_web_push_to(srv, user, job.network, subs, &job.logger, &job.message)
        .await
        && tx.send(super::Event::WebPushSubscriptionsChanged).is_err()
    {
        crate::debugf!(
            job.logger,
            "notification expiry persisted after actor shutdown"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{database, msg};

    fn queue() -> Arc<Queue> {
        Arc::new(Queue {
            pending: Mutex::default(),
            wake: Notify::new(),
            stop: CancellationToken::new(),
            metrics: Arc::default(),
        })
    }
    fn network(id: i64) -> super::super::Network {
        let mut record = database::Network::new("irc.example");
        record.id = id;
        super::super::Network::new(&Logger::new(false), record, Vec::new())
    }
    fn job(network: &super::super::Network, target: &str, time: Option<Timestamp>) -> Job {
        Job::new(
            network,
            target.into(),
            msg!("PRIVMSG", target, "notification"),
            time,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn slow_endpoint_flood_bounds_queue_tasks_and_active_work() -> anyhow::Result<()> {
        let queue = queue();
        let net = network(1);
        let task = tokio::spawn(run_worker(queue.clone(), |_| async {
            // A mock endpoint that never completes until cancellation.
            std::future::pending::<()>().await;
        }));
        assert_eq!(queue.admit(job(&net, "#first", None)), Admission::Queued);
        tokio::task::yield_now().await;
        assert_eq!(queue.metrics.active.load(Ordering::Relaxed), 1);
        let task_count = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        for i in 0..10_000 {
            queue.admit(job(&net, &format!("#target{i}"), None));
        }
        assert_eq!(
            crate::state_lock::lock(&queue.pending).jobs.len(),
            MAX_PENDING
        );
        assert_eq!(queue.metrics.pending.load(Ordering::Relaxed), 128);
        assert_eq!(
            queue.metrics.overflow_total.load(Ordering::Relaxed),
            10_000 - 128
        );
        assert_eq!(
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
            task_count
        );
        queue.stop.cancel();
        task.await?;
        assert_eq!(queue.metrics.active.load(Ordering::Relaxed), 0);
        assert_eq!(queue.metrics.pending.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_markread_replaces_delayed_notification_with_latest_state()
    -> anyhow::Result<()> {
        let queue = queue();
        let net = network(1);
        assert_eq!(
            queue.admit(job(&net, "#target", Some(Timestamp::UNIX_EPOCH))),
            Admission::Queued
        );
        for i in 0..10_000 {
            let mut marker = job(&net, "#target", None);
            marker.message = msg!("MARKREAD", "#target", i.to_string());
            assert_eq!(queue.admit(marker), Admission::Coalesced);
        }
        let sent = Arc::new(Mutex::new(Vec::new()));
        let output = sent.clone();
        let task = tokio::spawn(run_worker(queue.clone(), move |job| {
            crate::state_lock::lock(&output).push(job.message);
            std::future::ready(())
        }));
        tokio::task::yield_now().await;
        assert_eq!(
            *crate::state_lock::lock(&sent),
            vec![msg!("MARKREAD", "#target", "9999")]
        );
        assert_eq!(
            queue.metrics.coalesced_total.load(Ordering::Relaxed),
            10_000
        );
        queue.stop.cancel();
        task.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn network_deletion_cancels_inflight_and_discards_stale_queued_jobs() -> anyhow::Result<()>
    {
        let queue = queue();
        let net = network(1);
        let other = network(2);
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = started.clone();
        let task = tokio::spawn(run_worker(queue.clone(), move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::pending::<()>()
        }));
        queue.admit(job(&net, "#inflight", None));
        tokio::task::yield_now().await;
        queue.admit(job(&net, "#queued", None));
        queue.admit(job(&other, "#survivor", Some(Timestamp::UNIX_EPOCH)));
        net.stop.cancel();
        queue.cancel_network(Some(net.id()));
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::Relaxed), 1);
        assert_eq!(queue.metrics.active.load(Ordering::Relaxed), 0);
        assert_eq!(queue.metrics.pending.load(Ordering::Relaxed), 1);
        queue.stop.cancel();
        task.await?;
        assert_eq!(queue.metrics.pending.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn byte_pressure_preserves_existing_pending_state() {
        let queue = queue();
        let net = network(1);
        queue.admit(job(&net, "#keep", None));
        let mut huge = job(&net, "#keep", None);
        huge.bytes = MAX_PENDING_BYTES + 1;
        assert_eq!(queue.admit(huge), Admission::Overflow);
        assert_eq!(crate::state_lock::lock(&queue.pending).jobs.len(), 1);
        for i in 0..100 {
            let mut large = job(&net, &format!("#large{i}"), None);
            large.message = msg!("PRIVMSG", "#large", "x".repeat(32 * 1024));
            large.bytes = large.message.retained_size() + 512;
            queue.admit(large);
            assert!(crate::state_lock::lock(&queue.pending).bytes <= MAX_PENDING_BYTES);
        }
        queue.stop.cancel();
        assert_eq!(queue.admit(job(&net, "#stopped", None)), Admission::Stopped);
        queue.cancel_network(None);
        assert_eq!(queue.metrics.pending.load(Ordering::Relaxed), 0);
    }
    #[tokio::test]
    async fn delayed_push_observes_latest_read_marker() -> anyhow::Result<()> {
        let db = Arc::new(database::Database::open_in_memory()?);
        let mut user = database::User::new("push-read-marker");
        db.store_user(&mut user)?;
        let mut net = database::Network::new("irc.example");
        db.store_network(user.id, &mut net)?;
        let mentioned = Timestamp::from_second(10)?;
        assert!(should_notify(&db, net.id, "#target".into(), mentioned).await?);
        let mut receipt = database::ReadReceipt {
            target: "#target".into(),
            timestamp: Some(mentioned),
            ..Default::default()
        };
        db.store_read_receipt(net.id, &mut receipt)?;
        assert!(!should_notify(&db, net.id, "#target".into(), mentioned).await?);
        receipt.timestamp = Some(Timestamp::from_second(20)?);
        db.store_read_receipt(net.id, &mut receipt)?;
        assert!(!should_notify(&db, net.id, "#target".into(), Timestamp::from_second(15)?).await?);
        assert!(should_notify(&db, net.id, "#target".into(), Timestamp::from_second(21)?).await?);
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn unread_message_after_marker_keeps_its_notification_delay() -> anyhow::Result<()> {
        let queue = queue();
        let net = network(1);
        queue.admit(job(&net, "#target", None));
        queue.admit(job(&net, "#target", Some(Timestamp::UNIX_EPOCH)));
        assert!(queue.take_ready().is_none());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(queue.take_ready().is_some());
        Ok(())
    }
}
