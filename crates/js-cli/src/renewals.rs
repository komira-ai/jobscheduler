//! When `lease run` renews and when it gives the lease up, read off the expiry the server
//! last answered with and the injected clock, and the thread that does the waiting and
//! the renewing so that the supervising loop never blocks on either.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use crate::events::{Event, Waiter};
use crate::failure::Failure;

/// The shortest renewal period. A lease with less than a few seconds left is given up
/// before it is renewed, so this only keeps a very short ttl from making a busy loop.
const MIN_PERIOD: Duration = Duration::from_secs(1);

/// What the renewing thread does once its wait is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Then {
    /// Renew, with a request that must be answered within this long (so that it is over by
    /// the deadline).
    Renew(Duration),
    /// Report that the deadline has come.
    Deadline,
}

/// A wait through the waiter, and what follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pause {
    pub(crate) interval: Duration,
    pub(crate) then: Then,
}

/// The server's last word on how long the lease lasts, and what `js` makes of it: the
/// time a renewal is tried at, and the deadline by which one must have succeeded.
///
/// A renewal is tried when a third of the time left (at most `--renew-every`, at least a
/// second) has passed, so a ttl the server clamps is handled: only its answers count. The
/// lease is given up at the deadline, one such period before the server expires it, with
/// time still to stop the command; the period is the one the last answer set. Every time
/// is the injected clock's, in milliseconds since the epoch, as the server's are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Schedule {
    expires_at_ms: u64,
    period: Duration,
    renew_every: Option<Duration>,
}

impl Schedule {
    /// The schedule of a lease that expires at `expires_at_ms`, read at `now_ms`.
    pub(crate) fn new(expires_at_ms: u64, now_ms: u64, renew_every: Option<Duration>) -> Self {
        let mut schedule = Self {
            expires_at_ms,
            period: MIN_PERIOD,
            renew_every,
        };
        schedule.renewed(expires_at_ms, now_ms);
        schedule
    }

    /// The server renewed the lease to `expires_at_ms`, and it is `now_ms`.
    pub(crate) fn renewed(&mut self, expires_at_ms: u64, now_ms: u64) {
        let third = Duration::from_millis(expires_at_ms.saturating_sub(now_ms) / 3);
        self.expires_at_ms = expires_at_ms;
        self.period = self.renew_every.map_or(third, |cap| third.min(cap));
        self.period = self.period.max(MIN_PERIOD);
    }

    /// The deadline: one period short of the server's expiry.
    fn deadline_ms(&self) -> u64 {
        let period = u64::try_from(self.period.as_millis()).unwrap_or(u64::MAX);
        self.expires_at_ms.saturating_sub(period)
    }

    /// Whether the deadline has come at `now_ms`.
    pub(crate) fn lapsed(&self, now_ms: u64) -> bool {
        now_ms >= self.deadline_ms()
    }

    /// The next wait at `now_ms` (before the deadline): a period then a renewal, unless
    /// that would not end before the deadline, and then the time to the deadline and
    /// the report of it.
    pub(crate) fn pause(&self, now_ms: u64) -> Pause {
        let to_deadline = Duration::from_millis(self.deadline_ms().saturating_sub(now_ms));
        match self.period < to_deadline {
            true => Pause {
                interval: self.period,
                then: Then::Renew(to_deadline - self.period),
            },
            false => Pause {
                interval: to_deadline,
                then: Then::Deadline,
            },
        }
    }
}

/// Starts the thread that waits through `waiter` for `first.interval`, then renews with
/// `attempt` (given how long it may take) or reports the deadline, as `first.then` says.
/// A renewal's result goes to `events`, and the thread waits for the loop's answer on the
/// returned sender, the wait that follows, before it waits again: so there is one wait
/// at a time and the loop has dealt with a result before the next period starts. Dropping
/// the sender ends the thread once it is not inside a wait or a request; it is detached.
pub(crate) fn start(
    waiter: Arc<dyn Waiter>,
    first: Pause,
    events: Sender<Event>,
    attempt: impl FnMut(Duration) -> Result<u64, Failure> + Send + 'static,
) -> Sender<Pause> {
    let (answer, answers) = channel();
    thread::spawn(move || renew_until_told_to_stop(&*waiter, first, &events, &answers, attempt));
    answer
}

fn renew_until_told_to_stop(
    waiter: &dyn Waiter,
    first: Pause,
    events: &Sender<Event>,
    answers: &Receiver<Pause>,
    mut attempt: impl FnMut(Duration) -> Result<u64, Failure>,
) {
    let mut pause = first;
    loop {
        waiter.wait(pause.interval);
        let event = match pause.then {
            Then::Deadline => {
                let _ = events.send(Event::Deadline);
                return;
            }
            Then::Renew(limit) => Event::Renewed(attempt(limit)),
        };
        let _ = events.send(event);
        match answers.recv() {
            Ok(next) => pause = next,
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Asserts that the period is a third of what is left, that `renew_every` only caps
    /// it, that it is never under a second, and that the deadline is one period short
    /// of the expiry.
    #[test]
    fn the_period_is_a_third_of_what_is_left() {
        let ms = 1_000_000;
        let plain = Schedule::new(ms + 600_000, ms, None);
        assert_eq!(plain.period, secs(200));
        assert_eq!(plain.deadline_ms(), ms + 400_000);
        let capped = Schedule::new(ms + 600_000, ms, Some(secs(7)));
        assert_eq!(capped.period, secs(7));
        let loose = Schedule::new(ms + 60_000, ms, Some(secs(3600)));
        assert_eq!(loose.period, secs(20));
        let short = Schedule::new(ms + 900, ms, None);
        assert_eq!(short.period, secs(1));
        assert!(short.lapsed(ms));
        let gone = Schedule::new(5, ms, None);
        assert_eq!(gone.deadline_ms(), 0);
    }

    /// Asserts that a renewal sets the period from the new expiry, so that a ttl the
    /// server shortened is followed.
    #[test]
    fn a_renewal_resets_the_period_from_the_new_expiry() {
        let ms = 1_000_000;
        let mut schedule = Schedule::new(ms + 600_000, ms, None);
        schedule.renewed(ms + 200_000 + 120_000, ms + 200_000);
        assert_eq!(schedule.period, secs(40));
        assert_eq!(schedule.deadline_ms(), ms + 200_000 + 80_000);
    }

    /// Asserts that a pause is a period then a renewal while that ends before the
    /// deadline (the request limited to the time left after it), and the time to the
    /// deadline then a report of it when it would not.
    #[test]
    fn a_pause_ends_at_the_deadline_at_the_latest() {
        let ms = 1_000_000;
        let schedule = Schedule::new(ms + 600_000, ms, None);
        assert_eq!(
            schedule.pause(ms),
            Pause {
                interval: secs(200),
                then: Then::Renew(secs(200))
            }
        );
        assert_eq!(
            schedule.pause(ms + 200_000),
            Pause {
                interval: secs(200),
                then: Then::Deadline
            }
        );
        assert_eq!(
            schedule.pause(ms + 300_000),
            Pause {
                interval: secs(100),
                then: Then::Deadline
            }
        );
    }

    /// A waiter that returns at once and records what it was asked.
    struct Recording(Mutex<Vec<Duration>>);

    impl Waiter for Recording {
        fn wait(&self, interval: Duration) {
            self.0.lock().unwrap().push(interval);
        }
    }

    /// Asserts that the thread's loop waits, renews with the limit it is given, reports
    /// the result and waits again as the loop answers, until the deadline is reported
    /// (and nothing is renewed then), and that it ends when the loop is gone.
    #[test]
    fn the_renewing_loop_follows_the_answers_it_gets() {
        let waiter = Recording(Mutex::new(Vec::new()));
        let (events, inbox) = channel();
        let (answer, answers) = channel();
        let first = Pause {
            interval: secs(5),
            then: Then::Renew(secs(9)),
        };
        let deadline = Pause {
            interval: secs(6),
            then: Then::Deadline,
        };
        answer.send(first).unwrap();
        answer.send(deadline).unwrap();
        let mut limits = Vec::new();
        renew_until_told_to_stop(&waiter, first, &events, &answers, |limit| {
            limits.push(limit);
            Ok(42)
        });
        assert_eq!(limits, vec![secs(9), secs(9)]);
        assert_eq!(*waiter.0.lock().unwrap(), vec![secs(5), secs(5), secs(6)]);
        let seen: Vec<String> = inbox.try_iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(seen, vec!["Renewed(Ok(42))", "Renewed(Ok(42))", "Deadline"]);
    }

    /// Asserts that the thread's loop ends after reporting a result when the loop that
    /// reads the results is gone, and reports a failed renewal as it is.
    #[test]
    fn the_renewing_loop_ends_when_nobody_answers() {
        let waiter = Recording(Mutex::new(Vec::new()));
        let (events, inbox) = channel();
        let (answer, answers) = channel::<Pause>();
        drop(answer);
        let first = Pause {
            interval: secs(1),
            then: Then::Renew(secs(1)),
        };
        renew_until_told_to_stop(&waiter, first, &events, &answers, |_| {
            Err(Failure::Server("down".to_owned()))
        });
        let seen: Vec<String> = inbox.try_iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(seen, vec![r#"Renewed(Err(Server("down")))"#]);
    }
}
