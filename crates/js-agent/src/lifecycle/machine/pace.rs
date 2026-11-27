//! How often the lifecycle asks a mount provider again after a call it could not serve
//! (`docs/design/mount-providers.md`, section 4).
//!
//! A provider answers a call it cannot serve now `UNAVAILABLE` (or `RESOURCE_EXHAUSTED`, or
//! not within the call's bound) at once, and every such call is safe to ask again. Asked
//! again at every step, a provider in trouble is asked hundreds of times a second by one host.
//! So each call is paced twice:
//!
//! - **Per source** ([`Pace`]): after a failed `Prepare`, `Mount` or `Release` of a source,
//!   its next call waits [`wait_ms`] of the failures in a row so far, from the moment the
//!   failed call was sent: 1 s, then 2, 4, 8 and 16 s, and 16 s from then on, each between
//!   half of that and all of it. A success starts the count again. Measured from the send, a
//!   `Mount` that the provider held until its own bound (25 s for tierstore) is asked again at
//!   once.
//! - **Per provider** ([`Gate`]): once a provider has failed a `Prepare`, the `Prepare`s of
//!   every source that is not out yet go to it one at a time, each after the wait of the
//!   provider's failures in a row, until one succeeds. A new start's `Prepare` waits its turn
//!   with the rest, so a host with many starts waiting sends a provider that turns them away
//!   one `Prepare` per wait, not one for each start.
//!
//! The jitter is not random: it is taken from FNV-1a of the source (or the provider) and the
//! count, so sources that failed together are spread apart and a test sees the same waits
//! every run.

use crate::lifecycle::ProviderAnswer;

/// The ceiling of the first wait, in milliseconds.
pub(super) const FIRST_MS: u64 = 1_000;

/// The highest ceiling of a wait, in milliseconds: below the 25 s a tierstore `Mount` holds
/// a tree that is not ready, so a provider that holds its calls is never waited on longer
/// than it held.
pub(super) const CEILING_MS: u64 = 16_000;

/// The doublings after which [`FIRST_MS`] is at [`CEILING_MS`].
const MAX_DOUBLINGS: u32 = 4;

/// FNV-1a of `text` and `count`, for the jitter.
fn spread(text: &str, count: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes().chain(count.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The wait after a failure that follows `failures` failures in a row, of the source or
/// provider `seed` names: a ceiling of [`FIRST_MS`] doubled once for each of them, at most
/// [`CEILING_MS`], and somewhere from half the ceiling to the ceiling.
pub(super) fn wait_ms(failures: u32, seed: &str) -> u64 {
    let ceiling = (FIRST_MS << failures.min(MAX_DOUBLINGS)).min(CEILING_MS);
    let half = ceiling / 2;
    half + spread(seed, failures) % (half + 1)
}

/// What a failed answer did to a source's pace: the failures in a row it now has, and how
/// long after it the next call waits from now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Step {
    /// The failures in a row, this one included.
    pub(super) failures: u32,
    /// How long from now until the next call may be sent.
    pub(super) wait_ms: u64,
}

/// The pace of one source's calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Pace {
    /// The failed answers in a row.
    failures: u32,
    /// When the call that is out was sent; `None` when none is out.
    sent_ms: Option<u64>,
    /// When the next call may be sent.
    again_ms: u64,
}

impl Pace {
    /// Whether asking now is allowed: a call is out (asking it sends nothing and takes its
    /// answer), or the wait after the last failure has passed.
    pub(super) fn due(&self, now_ms: u64) -> bool {
        self.sent_ms.is_some() || now_ms >= self.again_ms
    }

    /// Whether an ask now sends a call: none is out.
    pub(super) fn idle(&self) -> bool {
        self.sent_ms.is_none()
    }

    /// The failed answers in a row so far.
    pub(super) fn failures(&self) -> u32 {
        self.failures
    }

    /// When the call that is out was sent.
    pub(super) fn sent_at(&self) -> Option<u64> {
        self.sent_ms
    }

    /// Notes an ask at `now_ms`: the moment a call is sent when none was out.
    pub(super) fn asked(&mut self, now_ms: u64) {
        self.sent_ms.get_or_insert(now_ms);
    }

    /// Takes the answer of the ask: a success starts the count again; a failure (refused,
    /// failed or unavailable) counts one more and sets the next call after the wait from the send;
    /// `Pending` leaves the call out. A failure gives the [`Step`] it took; `seed` names the
    /// source, for the jitter.
    pub(super) fn answered(
        &mut self,
        answer: &ProviderAnswer,
        now_ms: u64,
        seed: &str,
    ) -> Option<Step> {
        match answer {
            ProviderAnswer::Pending => None,
            ProviderAnswer::Done(_) => {
                *self = Self::default();
                None
            }
            ProviderAnswer::Refused(_)
            | ProviderAnswer::Failed(_)
            | ProviderAnswer::Unavailable(_) => {
                let sent = self.sent_ms.take().unwrap_or(now_ms);
                let wait = wait_ms(self.failures, seed);
                self.failures = self.failures.saturating_add(1);
                self.again_ms = sent.saturating_add(wait);
                Some(Step {
                    failures: self.failures,
                    wait_ms: self.again_ms.saturating_sub(now_ms),
                })
            }
        }
    }
}

/// The pace of the `Prepare`s to one provider while it fails them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Gate {
    /// The `Prepare`s it failed in a row (answered unavailable); 0 while it serves them.
    failures: u32,
    /// When the next `Prepare` may be sent, while it fails them.
    again_ms: u64,
    /// When the gate last let a `Prepare` through while the provider fails them; `None`
    /// once that one's failure was counted.
    turn_ms: Option<u64>,
}

impl Gate {
    /// Whether a `Prepare` may be sent now: the provider has not failed one since its last
    /// success, or the wait after the last one sent or failed has passed.
    pub(super) fn open(&self, now_ms: u64) -> bool {
        self.failures == 0 || now_ms >= self.again_ms
    }

    /// A `Prepare` is sent at `now_ms` to the provider `name`: while it fails them, the next
    /// one waits from now, so one goes at a time.
    pub(super) fn sent(&mut self, now_ms: u64, name: &str) {
        if self.failures > 0 {
            let wait = wait_ms(self.failures - 1, name);
            self.again_ms = now_ms.saturating_add(wait);
            self.turn_ms = Some(now_ms);
        }
    }

    /// A `Prepare` to the provider `name` sent at `sent_ms` was answered unavailable. The
    /// first failure, and the failure of a `Prepare` the gate let through since the last one
    /// counted, count one more failure in a row: the next `Prepare` waits the longer wait
    /// from that send. Any other (a `Prepare` that was already out with others when the
    /// provider began to fail) counts nothing, so `Prepare`s that fail together grow the wait
    /// once, not once each; it still keeps the next `Prepare` from going before the present
    /// wait after its send. (A success removes the gate; a refusal is about the ref, not the
    /// provider, and changes nothing.)
    pub(super) fn failed(&mut self, sent_ms: u64, name: &str) {
        let counts = self.failures == 0 || self.turn_ms.is_some_and(|turn| sent_ms >= turn);
        let wait = match counts {
            true => {
                let wait = wait_ms(self.failures, name);
                self.failures = self.failures.saturating_add(1);
                self.turn_ms = None;
                wait
            }
            false => wait_ms(self.failures - 1, name),
        };
        self.again_ms = self.again_ms.max(sent_ms.saturating_add(wait));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down() -> ProviderAnswer {
        ProviderAnswer::Unavailable("down".to_owned())
    }

    /// Asserts that each wait lies between half its ceiling and the ceiling, the ceiling
    /// doubling from 1 s for each failure before it up to 16 s and staying there, and that
    /// the jitter differs between two sources. Catches a wait that does not grow, one that
    /// grows past the ceiling (or overflows for a long run of failures), and jitter that
    /// sends sources that failed together back together.
    #[test]
    fn the_wait_doubles_to_its_ceiling_and_is_jittered() {
        let ceilings = [1_000, 2_000, 4_000, 8_000, 16_000, 16_000, 16_000];
        for (failures, ceiling) in (0u32..).zip(ceilings) {
            let wait = wait_ms(failures, "tier/r1#0");
            assert!(
                (ceiling / 2..=ceiling).contains(&wait),
                "{failures}: {wait}"
            );
        }
        assert!((8_000..=16_000).contains(&wait_ms(u32::MAX, "tier/r1#0")));
        let apart = (0..8).filter(|n| wait_ms(*n, "tier/r1#0") != wait_ms(*n, "tier/r2#0"));
        assert!(apart.count() > 0, "two sources share every wait");
        assert_eq!(wait_ms(3, "tier/r1#0"), wait_ms(3, "tier/r1#0"));
    }

    /// Asserts that a source's pace is due while a call is out and once its wait has passed
    /// (measured from the send), not before, and starts again at a success. Catches a pace
    /// that holds back the ask that takes an answer, one measured from the answer, and a
    /// count that a success does not end.
    #[test]
    fn a_pace_waits_from_the_send_and_a_success_ends_it() {
        let mut pace = Pace::default();
        assert!(pace.due(0));
        assert!(pace.idle());
        pace.asked(100);
        assert!(!pace.idle());
        pace.asked(150);
        assert_eq!(pace.sent_at(), Some(100), "the first ask sent it");
        assert_eq!(pace.answered(&ProviderAnswer::Pending, 150, "s"), None);
        let first = pace.answered(&down(), 300, "s").expect("a step");
        let wait = wait_ms(0, "s");
        assert_eq!(
            first,
            Step {
                failures: 1,
                wait_ms: 100 + wait - 300
            }
        );
        assert!(!pace.due(100 + wait - 1));
        assert!(pace.due(100 + wait));
        pace.asked(30_000);
        let refused = ProviderAnswer::Refused("no".to_owned());
        let second = pace.answered(&refused, 60_000, "s").expect("a step");
        assert_eq!(
            second,
            Step {
                failures: 2,
                wait_ms: 0
            },
            "held longer than the wait"
        );
        assert_eq!(pace.failures(), 2);
        pace.asked(60_000);
        let failed = ProviderAnswer::Failed("not prepared".to_owned());
        let third = pace.answered(&failed, 60_000, "s").expect("a step");
        assert_eq!(third.failures, 3, "a provider's state counts as a failure");
        pace.asked(60_000);
        assert_eq!(
            pace.answered(&ProviderAnswer::Done(None), 60_001, "s"),
            None
        );
        assert_eq!(pace, Pace::default());
    }

    /// Asserts that a failure handed over with no call noted as out waits from now.
    #[test]
    fn a_failure_with_no_send_noted_waits_from_now() {
        let mut pace = Pace::default();
        let step = pace.answered(&down(), 5_000, "s").expect("a step");
        assert_eq!(step.wait_ms, wait_ms(0, "s"));
        assert!(!pace.due(5_000 + step.wait_ms - 1));
    }

    /// Asserts that a provider's gate is open until a `Prepare` fails, then lets one through
    /// per wait (the send takes the turn), and that each failure grows the wait. Catches a
    /// gate that lets every waiting source through at once, and one that does not grow.
    #[test]
    fn a_gate_lets_one_prepare_through_per_wait() {
        let mut gate = Gate::default();
        assert!(gate.open(0));
        gate.sent(0, "tier");
        assert!(
            gate.open(0),
            "a sent Prepare closes nothing while all is well"
        );
        gate.failed(10, "tier");
        let first = wait_ms(0, "tier");
        assert!(!gate.open(10 + first - 1));
        assert!(gate.open(10 + first));
        gate.sent(10 + first, "tier");
        assert!(!gate.open(10 + first), "the send took the turn");
        let turn = 10 + first + wait_ms(0, "tier");
        assert!(gate.open(turn));
        gate.failed(turn, "tier");
        assert!(!gate.open(turn + wait_ms(1, "tier") - 1));
        assert!(gate.open(turn + wait_ms(1, "tier")));
    }

    /// Asserts that `Prepare`s that were out together and fail together count one failure,
    /// so the gate opens again after the first wait, not the fourth; and that a failure of a
    /// `Prepare` the gate let through counts. Catches a provider that answers a burst of
    /// in-flight `Prepare`s unavailable being held shut for 8 to 16 s at once.
    #[test]
    fn prepares_that_fail_together_count_once() {
        let mut gate = Gate::default();
        for _ in 0..4 {
            gate.failed(10, "tier");
        }
        let first = 10 + wait_ms(0, "tier");
        assert!(!gate.open(first - 1));
        assert!(gate.open(first), "one failure in a row, not four");
        gate.sent(first, "tier");
        gate.failed(first, "tier");
        assert!(!gate.open(first + wait_ms(1, "tier") - 1));
        assert!(gate.open(first + wait_ms(1, "tier")));
    }
}
