use super::{AgentHarness, Delivery};
use crate::Verdict;

/// How many times the scenario resends a Cancel while the lease is still listed.
const CANCEL_ROUNDS: u32 = 3;

/// The validity of a Start that never goes stale.
const UNBOUNDED: u64 = 0;

/// The validity the late-Start scenario gives its Starts, in milliseconds.
const VALIDITY: u64 = 1_000;

/// One behaviour every host agent must have, named in the suite's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    /// A Start repeated for the same lease id launches the job once.
    StartIsIdempotent,
    /// A Cancel that never arrived changes nothing; the scheduler resends it while the
    /// lease is listed, and the lease leaves the listing, after which a further Cancel
    /// is harmless and a Start for the cancelled id does not run it again.
    CancelUntilAbsent,
    /// A Start that has waited longer than its validity is dropped and reported as never
    /// started; one that waited exactly its validity, and one with no validity bound,
    /// run.
    LateStartIsDropped,
    /// Silence of the agent's fence window, counted from the last scheduler contact,
    /// makes the agent stop its leases; one tick less does not. An agent with no
    /// self-fence fails this scenario, as the legacy agent does today.
    SelfFencesAfterSilence,
    /// A result the scheduler has not acknowledged is sent again in every heartbeat,
    /// including the first on a new connection, and one that was acknowledged is not.
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

fn start_is_idempotent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("lease", UNBOUNDED, 0);
    harness.start("lease", UNBOUNDED, 0);
    let seen = harness.observed();
    notes.expect(
        seen.launches.get("lease") == Some(&1),
        "the job is launched once for two Starts",
    );
    notes.expect(seen.running.contains("lease"), "the lease is running");
}

fn cancel_until_absent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("lease", UNBOUNDED, 0);
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
    harness.start("lease", UNBOUNDED, 0);
    let seen = harness.observed();
    notes.expect(
        !seen.running.contains("lease"),
        "a cancelled lease id is retired: a later Start does not run it",
    );
    notes.expect(
        seen.launches.get("lease") == Some(&1),
        "a Cancel and a later Start launch nothing more",
    );
}

fn late_start_is_dropped<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("late", VALIDITY, VALIDITY + 1);
    harness.start("at-limit", VALIDITY, VALIDITY);
    harness.start("unbounded", UNBOUNDED, u64::MAX);
    let seen = harness.observed();
    notes.expect(
        seen.never_started.contains("late"),
        "a Start one tick past its validity is reported as never started",
    );
    notes.expect(
        !seen.running.contains("late"),
        "a Start past its validity is not running",
    );
    notes.expect(
        !seen.launches.contains_key("late"),
        "a Start past its validity launches nothing",
    );
    notes.expect(
        seen.running.contains("at-limit"),
        "a Start at exactly its validity runs",
    );
    notes.expect(
        seen.running.contains("unbounded"),
        "a Start with no validity bound runs however long it waited",
    );
    notes.expect(
        seen.never_started.len() == 1,
        "only the stale Start is reported as never started",
    );
}

fn self_fences_after_silence<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let Some(window) = harness.fence_window() else {
        notes.expect(
            false,
            "the agent has no self-fence, so it cannot stop its leases by itself",
        );
        return;
    };
    harness.start("lease", UNBOUNDED, 0);
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
    harness.start("lease", UNBOUNDED, 0);
    harness.complete("lease");
    notes.expect(
        harness.observed().results_sent == ["lease"],
        "the result is sent when the job ends",
    );
    harness.heartbeat();
    notes.expect(
        harness.observed().results_sent == ["lease", "lease"],
        "an unacknowledged result is repeated in the next heartbeat",
    );
    harness.disconnect();
    harness.heartbeat();
    harness.reconnect();
    notes.expect(
        harness.observed().results_sent == ["lease", "lease", "lease"],
        "an unacknowledged result is sent again on a new connection, and not while it is down",
    );
    notes.expect(
        harness.ack("lease") == Delivery::Delivered,
        "the acknowledgement is delivered",
    );
    harness.heartbeat();
    harness.disconnect();
    harness.reconnect();
    notes.expect(
        harness.observed().results_sent == ["lease", "lease", "lease"],
        "an acknowledged result is not sent again",
    );
}
