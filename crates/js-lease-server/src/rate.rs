//! The limit on hellos per credential: a token bucket on the injected clock.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use js_lease::Clock;
use js_registry::Credential;

use crate::adapt::SharedClock;

/// How many hellos one credential may say in a burst, and how often one more is allowed
/// once the burst is spent (`--agent-hello-burst`, `--agent-hello-interval`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HelloLimit {
    pub(crate) burst: u32,
    pub(crate) interval: Duration,
}

/// What one credential has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bucket {
    /// Hellos allowed right now.
    available: u32,
    /// The instant the next hello is counted from: the moment the bucket last had its
    /// latest whole hello added, so a part of an interval already waited is kept.
    since: u64,
    /// The instant a refusal of this credential was last announced (logged), if ever.
    announced: Option<u64>,
}

impl Bucket {
    fn full(limit: HelloLimit, now: u64) -> Self {
        Self {
            available: limit.burst,
            since: now,
            announced: None,
        }
    }

    /// Spends one hello at `now` (milliseconds), or says in how many milliseconds the
    /// next one is allowed. A clock that reads earlier than the last use adds nothing.
    fn take(&mut self, limit: HelloLimit, now: u64) -> Result<(), u64> {
        let interval = u64::try_from(limit.interval.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let waited = now.saturating_sub(self.since);
        let earned = waited / interval;
        let burst = u64::from(limit.burst);
        let refilled = u64::from(self.available).saturating_add(earned).min(burst);
        self.since = match refilled == burst {
            true => now.max(self.since),
            false => self.since.saturating_add(earned.saturating_mul(interval)),
        };
        match refilled.checked_sub(1) {
            Some(left) => {
                self.available = u32::try_from(left).unwrap_or(u32::MAX);
                Ok(())
            }
            None => {
                self.available = 0;
                Err(interval
                    .saturating_sub(now.saturating_sub(self.since))
                    .max(1))
            }
        }
    }
}

/// A hello that was held to the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    /// How long until the credential may say hello again.
    pub(crate) wait: Duration,
    /// Whether this refusal is the one to log: the first of its credential, then at most
    /// one in each interval, so a holder of a token cannot fill the log at the speed of
    /// its requests.
    pub(crate) announce: bool,
}

/// A bucket for each credential that has said hello. Only a credential the transport
/// authenticated reaches it, so the map holds at most one entry per join token in the
/// file ([`HelloLimiter::retain`] drops those of tokens that left it).
pub(crate) struct HelloLimiter {
    limit: HelloLimit,
    clock: SharedClock,
    buckets: Mutex<HashMap<Credential, Bucket>>,
}

impl HelloLimiter {
    pub(crate) fn new(limit: HelloLimit, clock: SharedClock) -> Self {
        Self {
            limit,
            clock,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one hello of `who`: `Ok` when it may go on, else the refusal.
    pub(crate) fn admit(&self, who: &Credential) -> Result<(), Held> {
        let now = self.clock.now().millis();
        let interval = u64::try_from(self.limit.interval.as_millis()).unwrap_or(u64::MAX);
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = buckets
            .entry(who.clone())
            .or_insert_with(|| Bucket::full(self.limit, now));
        bucket.take(self.limit, now).map_err(|wait| {
            let quiet = bucket
                .announced
                .is_some_and(|last| now.saturating_sub(last) < interval);
            if !quiet {
                bucket.announced = Some(now);
            }
            Held {
                wait: Duration::from_millis(wait),
                announce: !quiet,
            }
        })
    }

    /// Drops the buckets of every credential not in `keep`: the join tokens that have
    /// left the token file. A token that comes back starts with a full bucket.
    pub(crate) fn retain(&self, keep: &[Credential]) {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        buckets.retain(|who, _| keep.contains(who));
    }

    /// How many credentials have a bucket.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use js_lease::{ManualClock, Timestamp};

    use super::*;

    const LIMIT: HelloLimit = HelloLimit {
        burst: 3,
        interval: Duration::from_secs(10),
    };

    /// A bucket lets a burst through and then refuses until an interval has passed; it
    /// says how long to wait, keeps the part of an interval already waited, never holds
    /// more than the burst however long it idles, and a clock that steps back adds
    /// nothing. It catches a limiter that never refuses, one that forgets the partial
    /// wait (so a client polling each interval minus a moment is starved), one that
    /// stores up credit without bound, and one that panics or refunds on a clock step.
    #[test]
    fn a_bucket_spends_a_burst_then_refills_one_interval_at_a_time() {
        let mut bucket = Bucket::full(LIMIT, 1_000);
        for _ in 0..3 {
            assert_eq!(bucket.take(LIMIT, 1_000), Ok(()));
        }
        assert_eq!(bucket.take(LIMIT, 1_000), Err(10_000));
        assert_eq!(bucket.take(LIMIT, 4_000), Err(7_000));
        assert_eq!(bucket.take(LIMIT, 11_000), Ok(()));
        assert_eq!(bucket.take(LIMIT, 11_000), Err(10_000));
        // Two intervals and a half later: two hellos, and the half interval is kept.
        assert_eq!(bucket.take(LIMIT, 36_000), Ok(()));
        assert_eq!(bucket.take(LIMIT, 36_000), Ok(()));
        assert_eq!(bucket.take(LIMIT, 36_000), Err(5_000));
        // A long idle is capped at the burst.
        for _ in 0..3 {
            assert_eq!(bucket.take(LIMIT, 10_000_000), Ok(()));
        }
        assert_eq!(bucket.take(LIMIT, 10_000_000), Err(10_000));
        // A clock stepped back earns nothing and does not move the count back.
        assert_eq!(bucket.take(LIMIT, 5), Err(10_000));
    }

    /// An interval of under a millisecond is read as one millisecond and a burst of one
    /// is a bucket that holds a single hello: the arithmetic neither divides by zero nor
    /// overflows at the edges.
    #[test]
    fn extreme_limits_do_not_divide_by_zero_or_overflow() {
        let tiny = HelloLimit {
            burst: 1,
            interval: Duration::ZERO,
        };
        let mut bucket = Bucket::full(tiny, 0);
        assert_eq!(bucket.take(tiny, 0), Ok(()));
        assert_eq!(bucket.take(tiny, 0), Err(1));
        assert_eq!(bucket.take(tiny, 1), Ok(()));
        let huge = HelloLimit {
            burst: u32::MAX,
            interval: Duration::MAX,
        };
        let mut bucket = Bucket::full(huge, u64::MAX);
        assert_eq!(bucket.take(huge, u64::MAX), Ok(()));
    }

    /// The limiter keeps one bucket per credential on the injected clock: one credential
    /// spending its burst leaves another's whole, and moving the clock refills. It
    /// catches a single global bucket (one noisy token starving the others) and a limiter
    /// that reads the system clock.
    #[test]
    fn each_credential_has_its_own_bucket_on_the_injected_clock() {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let limiter = HelloLimiter::new(LIMIT, SharedClock::new(clock.clone()));
        let (a, b) = (Credential::of_token("a"), Credential::of_token("b"));
        for _ in 0..3 {
            assert_eq!(limiter.admit(&a), Ok(()));
        }
        let wait = |limiter: &HelloLimiter, who| limiter.admit(who).map_err(|held| held.wait);
        assert_eq!(wait(&limiter, &a), Err(Duration::from_secs(10)));
        assert_eq!(limiter.admit(&b), Ok(()));
        clock.advance(Duration::from_secs(10));
        assert_eq!(limiter.admit(&a), Ok(()));
    }

    /// A refusal is announced for its credential the first time and then at most once in
    /// each interval, however many follow, and each credential has its own count. It
    /// catches a limiter that announces every refusal (a token holder fills the log at
    /// the speed of its requests), one that never announces, and one that shares the
    /// announcement between credentials (a noisy token hides another's).
    #[test]
    fn a_refusal_is_announced_once_per_credential_per_interval() {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let limiter = HelloLimiter::new(LIMIT, SharedClock::new(clock.clone()));
        let (a, b) = (Credential::of_token("a"), Credential::of_token("b"));
        let announced = |who| {
            limiter
                .admit(who)
                .map(|()| false)
                .unwrap_or_else(|h| h.announce)
        };
        for _ in 0..3 {
            assert!(!announced(&a));
            assert!(!announced(&b));
        }
        assert!(announced(&a));
        for _ in 0..5 {
            assert!(!announced(&a));
        }
        assert!(announced(&b));
        clock.advance(Duration::from_secs(9));
        assert!(!announced(&a));
        clock.advance(Duration::from_secs(1));
        // One hello is earned back and spent; the next refusal is a new interval's.
        assert_eq!(limiter.admit(&a), Ok(()));
        assert!(announced(&a));
        assert!(!announced(&a));
    }

    /// Dropping the credentials a token file no longer holds drops their buckets and
    /// keeps the rest, so a token that is rotated out does not leave a bucket behind and
    /// one that comes back starts full. It catches a limiter that only ever grows and a
    /// retain that drops the buckets still in use.
    #[test]
    fn buckets_of_credentials_that_left_are_dropped() {
        let clock = ManualClock::new(Timestamp::from_millis(0));
        let limiter = HelloLimiter::new(LIMIT, SharedClock::new(clock));
        let (a, b) = (Credential::of_token("a"), Credential::of_token("b"));
        for _ in 0..3 {
            limiter.admit(&a).unwrap();
        }
        limiter.admit(&b).unwrap();
        assert_eq!(limiter.tracked(), 2);
        limiter.retain(std::slice::from_ref(&b));
        assert_eq!(limiter.tracked(), 1);
        for _ in 0..3 {
            assert_eq!(limiter.admit(&a), Ok(()));
        }
        assert_eq!(limiter.tracked(), 2);
        assert!(limiter.admit(&b).is_ok());
        assert!(limiter.admit(&b).is_ok());
        assert!(limiter.admit(&b).is_err());
    }
}
