//! Server metrics, exposed in the Prometheus text format.

use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub downstreams: AtomicI64,
    pub upstreams: AtomicI64,
    pub upstream_out_messages_total: AtomicU64,
    pub upstream_in_messages_total: AtomicU64,
    pub downstream_out_messages_total: AtomicU64,
    pub downstream_in_messages_total: AtomicU64,
    pub upstream_connect_errors_total: AtomicU64,
    pub worker_panics_total: AtomicU64,
}

pub fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Counts a running operation until completion, cancellation or unwinding.
pub(crate) async fn with_gauge<F: std::future::Future>(gauge: &AtomicI64, f: F) -> F::Output {
    struct Guard<'a>(&'a AtomicI64);

    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    gauge.fetch_add(1, Ordering::Relaxed);
    let _guard = Guard(gauge);
    f.await
}

impl Metrics {
    /// Renders all metrics. users is the current number of active users.
    pub fn render(&self, users: usize) -> String {
        let mut s = String::new();
        let mut gauge = |name: &str, help: &str, v: f64| {
            writeln!(s, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}")
                .expect("writing metrics into a String cannot fail");
        };
        gauge(
            "soju_users_active",
            "Current number of active users",
            users as f64,
        );
        gauge(
            "soju_downstreams_active",
            "Current number of downstream connections",
            self.downstreams.load(Ordering::Relaxed) as f64,
        );
        gauge(
            "soju_upstreams_active",
            "Current number of upstream connections",
            self.upstreams.load(Ordering::Relaxed) as f64,
        );
        let counters = [
            (
                "soju_upstream_out_messages_total",
                "Total number of outgoing messages sent to upstream servers",
                &self.upstream_out_messages_total,
            ),
            (
                "soju_upstream_in_messages_total",
                "Total number of incoming messages received from upstream servers",
                &self.upstream_in_messages_total,
            ),
            (
                "soju_downstream_out_messages_total",
                "Total number of outgoing messages sent to downstream clients",
                &self.downstream_out_messages_total,
            ),
            (
                "soju_downstream_in_messages_total",
                "Total number of incoming messages received from downstream clients",
                &self.downstream_in_messages_total,
            ),
            (
                "soju_upstream_connect_errors_total",
                "Total number of upstream connection errors",
                &self.upstream_connect_errors_total,
            ),
            (
                "soju_worker_panics_total",
                "Total number of panics in worker goroutines",
                &self.worker_panics_total,
            ),
        ];
        for (name, help, v) in counters {
            writeln!(
                s,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
                v.load(Ordering::Relaxed)
            )
            .expect("writing metrics into a String cannot fail");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gauge_preserves_results_and_existing_count() {
        let gauge = AtomicI64::new(3);
        let result = with_gauge(&gauge, async {
            assert_eq!(gauge.load(Ordering::Relaxed), 4);
            Ok::<_, &str>(42)
        })
        .await;
        assert_eq!(result, Ok(42));
        assert_eq!(gauge.load(Ordering::Relaxed), 3);
        assert_eq!(
            with_gauge(&gauge, async { Err::<(), _>("failure") }).await,
            Err("failure")
        );
        assert_eq!(gauge.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn cancelling_downstream_restores_gauge() {
        let srv = crate::server::Server::new(
            crate::database::Database::open_in_memory().unwrap(),
            crate::logger::Logger::new(false),
        );
        srv.metrics.downstreams.store(3, Ordering::Relaxed);
        let mut future = Box::pin(crate::metrics_guard(&srv, std::future::pending::<()>()));
        assert!(futures_util::poll!(future.as_mut()).is_pending());
        assert_eq!(srv.metrics.downstreams.load(Ordering::Relaxed), 4);
        drop(future);
        assert_eq!(srv.metrics.downstreams.load(Ordering::Relaxed), 3);
    }

    #[cfg(panic = "unwind")]
    #[tokio::test]
    #[expect(
        clippy::panic,
        reason = "The intentional panic exercises gauge cleanup while unwinding."
    )]
    async fn unwinding_restores_gauge() {
        use futures_util::FutureExt;

        let gauge = AtomicI64::new(3);
        let result = std::panic::AssertUnwindSafe(with_gauge(&gauge, async {
            assert_eq!(gauge.load(Ordering::Relaxed), 4);
            panic!("simulated session failure");
        }))
        .catch_unwind()
        .await;
        assert!(result.is_err());
        assert_eq!(gauge.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn render() {
        let m = Metrics::default();
        inc(&m.worker_panics_total);
        m.downstreams.fetch_add(2, Ordering::Relaxed);
        let s = m.render(3);
        assert!(s.contains("soju_users_active 3\n"));
        assert!(s.contains("soju_downstreams_active 2\n"));
        assert!(s.contains("soju_worker_panics_total 1\n"));
    }
}
