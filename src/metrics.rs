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

impl Metrics {
    /// Renders all metrics. users is the current number of active users.
    pub fn render(&self, users: usize) -> String {
        let mut s = String::new();
        let mut gauge = |name: &str, help: &str, v: f64| {
            let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
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
            let _ = writeln!(
                s,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
                v.load(Ordering::Relaxed)
            );
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
