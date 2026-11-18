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
    ///
    /// # Errors
    ///
    /// Returns an error if rate intervals or backoff bounds are invalid or unrepresentable.
    pub fn new(min: Duration, max: Duration, jitter: Duration) -> anyhow::Result<Backoffer> {
        if min.is_zero() || max < min {
            anyhow::bail!("backoff requires a positive minimum and maximum at least the minimum");
        }
        Ok(Backoffer {
            min,
            max,
            jitter,
            n: 0,
        })
    }

    pub fn reset(&mut self) {
        self.n = 0;
    }

    ///
    /// # Panics
    ///
    /// Panics only if the documented fixed-data invariant is violated; operational errors are handled separately.
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
            let j = rand::rng().random_range(0..self.jitter.as_nanos());
            #[expect(
                clippy::expect_used,
                reason = "A sample below Duration::MAX fits the seconds range."
            )]
            let secs = u64::try_from(j / 1_000_000_000)
                .expect("a sample below a Duration fits its seconds range");
            #[expect(
                clippy::expect_used,
                reason = "The remainder is strictly below one billion and fits u32."
            )]
            let nanos = u32::try_from(j % 1_000_000_000)
                .expect("nanosecond remainder is below one billion");
            d = d.saturating_add(Duration::new(secs, nanos));
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
    ///
    /// # Errors
    ///
    /// Returns an error if rate intervals or backoff bounds are invalid or unrepresentable.
    pub fn new(interval: Duration, burst: u32) -> anyhow::Result<RateLimiter> {
        if interval.is_zero() || burst == 0 || Instant::now().checked_add(interval).is_none() {
            anyhow::bail!("rate interval and burst must be positive");
        }
        Ok(RateLimiter {
            interval,
            burst: f64::from(burst),
            tokens: f64::from(burst),
            last: Instant::now(),
        })
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
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn backoff() {
        let mut b = Backoffer::new(
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::ZERO,
        )
        .unwrap();
        let got: Vec<u64> = (0..6).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(got, vec![0, 1, 2, 4, 5, 5]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::ZERO);
        let mut b = Backoffer::new(
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
        .unwrap();
        b.next_delay();
        let d = b.next_delay();
        assert!(d >= Duration::from_secs(1) && d < Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    async fn limiter() {
        let mut rl = RateLimiter::new(Duration::from_secs(2), 3).unwrap();
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
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions fail immediately on invalid test data."
    )]
    fn invalid_rates_and_large_jitter_are_checked() {
        assert!(RateLimiter::new(Duration::ZERO, 1).is_err());
        assert!(RateLimiter::new(Duration::from_secs(1), 0).is_err());
        assert!(RateLimiter::new(Duration::MAX, 1).is_err());
        assert!(Backoffer::new(Duration::ZERO, Duration::from_secs(1), Duration::ZERO).is_err());
        assert!(
            Backoffer::new(
                Duration::from_secs(2),
                Duration::from_secs(1),
                Duration::ZERO
            )
            .is_err()
        );
        let mut backoff = Backoffer::new(Duration::MAX, Duration::MAX, Duration::MAX).unwrap();
        assert_eq!(backoff.next_delay(), Duration::ZERO);
        assert_eq!(backoff.next_delay(), Duration::MAX);
    }
}
