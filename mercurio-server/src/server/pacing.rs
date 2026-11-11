//! How fast clients may publish and connect: token buckets, and the pacing that
//! holds a client to its publish rate.

use tokio::time::{Duration, Instant};

use super::{Limits, Rate};

/// A token bucket holding up to `burst` tokens, refilled at `per_second`.
pub(super) struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub(super) fn new(rate: Rate) -> Self {
        let burst = f64::from(rate.burst.max(1));
        Bucket {
            rate: f64::from(rate.per_second),
            burst,
            tokens: burst,
            last: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.last = now;
    }

    /// Takes `amount` tokens, going into debt if there are fewer; how long
    /// until the debt is paid back, if there is one.
    fn take(&mut self, amount: f64) -> Option<Duration> {
        self.refill();
        self.tokens -= amount;
        if self.tokens >= 0.0 {
            return None;
        }
        if self.rate == 0.0 {
            return Some(Duration::MAX);
        }
        Some(Duration::from_secs_f64(-self.tokens / self.rate))
    }

    /// Takes one token if there is one.
    pub(super) fn try_take(&mut self) -> bool {
        self.refill();
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

/// How fast one client may publish.
pub(super) struct Pacing {
    messages: Option<Bucket>,
    bytes: Option<Bucket>,
    /// How long the client may stay over its rate
    grace: Option<Duration>,
    /// Since when the client is over its rate, if it is
    over_since: Option<Instant>,
}

impl Pacing {
    pub(super) fn new(limits: &Limits) -> Self {
        Pacing {
            messages: limits.max_publish_rate.map(Bucket::new),
            bytes: limits.max_publish_bytes_rate.map(Bucket::new),
            grace: limits.publish_rate_grace,
            over_since: None,
        }
    }

    /// Counts a PUBLISH of `bytes`; how long to wait before reading the
    /// next packet, if the client went over its rate.
    pub(super) fn publish(&mut self, bytes: usize) -> Option<Duration> {
        let messages = self.messages.as_mut().and_then(|b| b.take(1.0));
        let bytes = self.bytes.as_mut().and_then(|b| b.take(bytes as f64));
        let wait = messages.max(bytes);
        match wait {
            Some(_) => {
                self.over_since.get_or_insert_with(Instant::now);
            }
            None => self.over_since = None,
        }
        wait
    }

    /// Records that the client had nothing waiting once its rate allowed
    /// reading from it again, so it is no longer over its rate.
    pub(super) fn caught_up(&mut self) {
        self.over_since = None;
    }

    /// Whether the client has been over its rate for longer than allowed.
    pub(super) fn past_grace(&self) -> bool {
        match (self.grace, self.over_since) {
            (Some(grace), Some(since)) => since.elapsed() > grace,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_buckets_allow_a_burst_then_the_rate() {
        let mut bucket = Bucket::new(Rate::new(10, 3));
        for _ in 0..3 {
            assert_eq!(bucket.take(1.0), None);
        }
        // In debt by one, paid back in a tenth of a second
        assert_eq!(bucket.take(1.0), Some(Duration::from_millis(100)));
        time::advance(Duration::from_millis(100)).await;
        assert_eq!(bucket.take(1.0), Some(Duration::from_millis(100)));
        // A long quiet spell refills no more than the burst
        time::advance(Duration::from_secs(60)).await;
        assert!(bucket.try_take() && bucket.try_take() && bucket.try_take());
        assert!(!bucket.try_take());
        time::advance(Duration::from_millis(100)).await;
        assert!(bucket.try_take());
    }

    #[tokio::test(start_paused = true)]
    async fn test_pacing_waits_for_the_slower_of_messages_and_bytes() {
        let limits = Limits {
            max_publish_rate: Some(Rate::new(100, 1)),
            max_publish_bytes_rate: Some(Rate::new(1000, 1000)),
            ..Default::default()
        };
        let mut pacing = Pacing::new(&limits);
        assert_eq!(pacing.publish(500), None);
        // One message too many, and 1000 bytes too many
        assert_eq!(pacing.publish(1500), Some(Duration::from_secs(1)));
        assert_eq!(Pacing::new(&Limits::default()).publish(usize::MAX), None);
    }
}
