//! Exponential backoff and token-bucket rate limiting.

use std::time::Duration;

use rand::RngExt;
use tokio::time::Instant;

/// A simple exponential backoff.
pub struct Backoffer {
    min: Duration,
    max: Duration,
    jitter: Duration,
    n: u32,
}

impl Backoffer {
    pub fn new(min: Duration, max: Duration, jitter: Duration) -> Backoffer {
        Backoffer {
            min,
            max,
            jitter,
            n: 0,
        }
    }

    pub fn reset(&mut self) {
        self.n = 0;
    }

    pub fn next_delay(&mut self) -> Duration {
        if self.n == 0 {
            self.n = 1;
            return Duration::ZERO;
        }

        let mut d = self.min.saturating_mul(self.n);
        if d > self.max {
            d = self.max;
        } else {
            self.n = self.n.saturating_mul(2);
        }

        if !self.jitter.is_zero() {
            let j = rand::rng().random_range(0..self.jitter.as_nanos() as u64);
            d += Duration::from_nanos(j);
        }
        d
    }
}

/// A token-bucket rate limiter, like golang.org/x/time/rate with a limit of
/// one event every `interval` and the given burst size. The bucket starts
/// full.
pub struct RateLimiter {
    interval: Duration,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(interval: Duration, burst: u32) -> RateLimiter {
        RateLimiter {
            interval,
            burst: burst as f64,
            tokens: burst as f64,
            last: Instant::now(),
        }
    }

    /// Waits until an event is allowed.
    pub async fn wait(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed / self.interval.as_secs_f64()).min(self.burst);
        self.tokens -= 1.0;
        if self.tokens < 0.0 {
            let wait = Duration::from_secs_f64(-self.tokens * self.interval.as_secs_f64());
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff() {
        let mut b = Backoffer::new(
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::ZERO,
        );
        let got: Vec<u64> = (0..6).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(got, vec![0, 1, 2, 4, 5, 5]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::ZERO);
        let mut b = Backoffer::new(
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        b.next_delay();
        let d = b.next_delay();
        assert!(d >= Duration::from_secs(1) && d < Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn limiter() {
        let mut rl = RateLimiter::new(Duration::from_secs(2), 3);
        let start = Instant::now();
        for _ in 0..3 {
            rl.wait().await;
        }
        assert!(start.elapsed() < Duration::from_millis(10));
        rl.wait().await;
        assert!(start.elapsed() >= Duration::from_secs(2));
        rl.wait().await;
        assert!(start.elapsed() >= Duration::from_secs(4));
    }
}
