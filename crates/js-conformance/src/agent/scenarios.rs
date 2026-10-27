use super::{AgentHarness, Delivery};
use crate::Verdict;

/// How many times the scenario resends a Cancel while the lease is still listed.
const CANCEL_ROUNDS: u32 = 3;

/// One behaviour every host agent must have, named in the suite's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    /// A Start repeated for the same lease id launches the job once and is acknowledged.
    StartIsIdempotent,
    /// A Cancel that never arrived changes nothing; the scheduler resends it while the
    /// lease is listed, and the lease leaves the listing, after which a further Cancel
    /// is harmless.
    CancelUntilAbsent,
    /// A Start that arrives after its validity bound is dropped and reported as never
    /// started, and a Start inside the bound beside it runs.
    LateStartIsDropped,
    /// Silence of the fence window, counted from the last scheduler contact, makes the
    /// agent stop its leases; one tick less does not.
    SelfFencesAfterSilence,
    /// A result the scheduler has not acknowledged is sent again on every new
    /// connection, and one that was acknowledged is not.
    UnackedResultIsResent,
}

impl Scenario {
    /// Every scenario, in the order [`super::run`] runs them.
    pub const ALL: [Scenario; 5] = [
        Scenario::StartIsIdempotent,
        Scenario::CancelUntilAbsent,
        Scenario::LateStartIsDropped,
        Scenario::SelfFencesAfterSilence,
        Scenario::UnackedResultIsResent,
    ];

    /// Runs this scenario on a fresh harness.
    pub(crate) fn run<H: AgentHarness>(&self, harness: &mut H) -> Verdict {
        let mut notes = Notes(Vec::new());
        match self {
            Self::StartIsIdempotent => start_is_idempotent(harness, &mut notes),
            Self::CancelUntilAbsent => cancel_until_absent(harness, &mut notes),
            Self::LateStartIsDropped => late_start_is_dropped(harness, &mut notes),
            Self::SelfFencesAfterSilence => self_fences_after_silence(harness, &mut notes),
            Self::UnackedResultIsResent => unacked_result_is_resent(harness, &mut notes),
        }
        notes.verdict()
    }
}

/// The observations that did not hold, in the order they were made. A scenario makes
/// every observation, so the verdict names everything that was wrong, not the first.
struct Notes(Vec<String>);

impl Notes {
    fn expect(&mut self, held: bool, what: &str) {
        if !held {
            self.0.push(what.to_string());
        }
    }

    fn verdict(self) -> Verdict {
        match self.0.is_empty() {
            true => Verdict::Passed,
            false => Verdict::Failed(self.0.join("; ")),
        }
    }
}

/// The time up to which a Start made now is valid.
fn bound<H: AgentHarness>(harness: &H) -> u64 {
    harness.now() + harness.fence_window()
}

fn start_is_idempotent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let until = bound(harness);
    notes.expect(
        harness.start("lease", until) == Delivery::Delivered,
        "the first Start is delivered",
    );
    notes.expect(
        harness.start("lease", until) == Delivery::Delivered,
        "the repeated Start is delivered",
    );
    let seen = harness.observed();
    notes.expect(
        seen.launches.get("lease") == Some(&1),
        "the job is launched once for two Starts",
    );
    notes.expect(seen.running.contains("lease"), "the lease is running");
}

fn cancel_until_absent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let until = bound(harness);
    harness.start("lease", until);
    harness.disconnect();
    notes.expect(
        harness.cancel("lease") == Delivery::Lost,
        "a Cancel on a dropped connection is not delivered",
    );
    harness.reconnect();
    notes.expect(
        harness.observed().running.contains("lease"),
        "a Cancel that never arrived left the lease running",
    );
    for _ in 0..CANCEL_ROUNDS {
        if harness.observed().running.contains("lease") {
            harness.cancel("lease");
        }
        harness.advance(1_000);
    }
    notes.expect(
        !harness.observed().running.contains("lease"),
        "the lease left the listing after the Cancel was resent",
    );
    notes.expect(
        harness.cancel("lease") == Delivery::Delivered,
        "a Cancel for an absent lease is acknowledged",
    );
    let seen = harness.observed();
    notes.expect(
        !seen.running.contains("lease"),
        "a Cancel for an absent lease does not bring it back",
    );
    notes.expect(
        seen.launches.get("lease") == Some(&1),
        "a Cancel for an absent lease launches nothing",
    );
}

fn late_start_is_dropped<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let late = bound(harness);
    harness.advance(harness.fence_window() + 1);
    harness.start("late", late);
    let until = bound(harness);
    harness.start("timely", until);
    let seen = harness.observed();
    notes.expect(
        seen.never_started.contains("late"),
        "a Start past its bound is reported as never started",
    );
    notes.expect(
        !seen.running.contains("late"),
        "a Start past its bound is not running",
    );
    notes.expect(
        !seen.launches.contains_key("late"),
        "a Start past its bound launches nothing",
    );
    notes.expect(
        seen.running.contains("timely"),
        "a Start inside its bound runs",
    );
    notes.expect(
        !seen.never_started.contains("timely"),
        "a Start inside its bound is not reported as never started",
    );
}

fn self_fences_after_silence<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let window = harness.fence_window();
    let until = bound(harness);
    harness.start("lease", until);
    harness.advance(window - 1);
    notes.expect(
        harness.observed().running.contains("lease"),
        "the lease runs one tick short of the window",
    );
    harness.contact();
    harness.disconnect();
    harness.advance(window - 1);
    notes.expect(
        harness.observed().running.contains("lease"),
        "the window counts from the last contact, not from the Start",
    );
    harness.advance(1);
    let seen = harness.observed();
    notes.expect(
        !seen.running.contains("lease"),
        "the lease is stopped after the full window of silence",
    );
    notes.expect(
        seen.fenced.contains("lease"),
        "the lease is reported fenced after the full window of silence",
    );
}

fn unacked_result_is_resent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let until = bound(harness);
    harness.start("lease", until);
    harness.complete("lease");
    notes.expect(
        harness.observed().results_sent == ["lease"],
        "the result is sent once when the job ends",
    );
    harness.disconnect();
    harness.reconnect();
    notes.expect(
        harness.observed().results_sent == ["lease", "lease"],
        "an unacknowledged result is sent again after a reconnect",
    );
    notes.expect(
        harness.ack("lease") == Delivery::Delivered,
        "the acknowledgement is delivered",
    );
    harness.disconnect();
    harness.reconnect();
    notes.expect(
        harness.observed().results_sent == ["lease", "lease"],
        "an acknowledged result is not sent again",
    );
}
