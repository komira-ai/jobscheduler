mod host;

use super::{AgentHarness, Answer, Delivery, Flaw};
use crate::Verdict;

/// How many times the scenario resends a Cancel while the lease is still listed.
const CANCEL_ROUNDS: u32 = 3;

/// One behaviour every host agent must have, named in the suite's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    /// A Start repeated for the same lease id launches the job once.
    StartIsIdempotent,
    /// A Cancel that never arrived changes nothing; the scheduler resends it while the
    /// lease is listed, and the lease leaves the listing, after which a further Cancel
    /// is harmless and a Start for the cancelled id does not run it again.
    CancelUntilAbsent,
    /// Silence of the agent's fence window, counted from the last scheduler contact,
    /// makes the agent stop its leases; one tick less does not. An agent with no
    /// self-fence fails this scenario, as the legacy agent does today.
    SelfFencesAfterSilence,
    /// A result the scheduler has not acknowledged is sent again in every heartbeat,
    /// including the first on a new connection, and one that was acknowledged is not.
    UnackedResultIsResent,
    /// A fenced start repeated while its job runs launches nothing more; a start whose
    /// fence (the job with its allocation) the agent retired, because the job ended and its
    /// result was acknowledged, launches nothing; the same job under a new allocation is a
    /// new fence and runs.
    RetiredFenceIsRefused,
    /// A result answered with any verdict the agent knows (accepted, duplicate, stale, a
    /// mismatch) is not sent again; one answered with a verdict it does not
    /// know is; an answer for a lease that has no result, or that the agent does not hold,
    /// changes nothing.
    AnsweredResultIsDropped,
    /// A job's result carries what the job used however the job ended: on its own, with a
    /// failure, or killed by a stop.
    UsageIsReportedOnEveryEnd,
    /// A start that breaks a rule of the start contract (no wall time, a reserved label, a
    /// fence with no allocation, a mount outside the agent's roots, a GPU on an agent with
    /// none, a VM on an agent with no VM backend) is refused and launches nothing, and a
    /// well-formed start after it runs.
    FlawedStartIsRefused,
    /// After a run the agent reports its container gone and its scratch gone before its
    /// slot free; a container that cannot be removed frees no slot and taints the host.
    CleanupFreesTheSlotLast,
    /// A start whose job may change the host is refused, and launches nothing, until the
    /// operator consents to such jobs; after the consent it runs.
    HostJobNeedsConsent,
    /// A job that may change the host leaves it tainted however it ended: on its own, with
    /// a failure, or stopped by a Cancel.
    HostJobTaintsWhateverItsEnd,
    /// Each post-job check taints the host on its own and keeps the run's slot: a process
    /// of the job's user left behind, and a check that could not run, which is also a
    /// finding.
    TaintingCheckTaintsOnItsOwn,
    /// A taint is in every heartbeat until it is acknowledged; the acknowledgement stops the
    /// report and not the taint; a restart reports it again and the answer to the restarted
    /// agent's first heartbeat does not clear it; a tainted host takes no start.
    TaintIsKept,
    /// After a restart the agent's first heartbeat lists the run that kept running, which is
    /// not launched again, and repeats the result not yet acknowledged.
    RestartKeepsWhatItHolds,
    /// A host held whole for a lease is not reported ready while anything is held on it,
    /// starts no run of the lease before it is ready and no run of another lease at all,
    /// and is ready, running the lease's runs, once nothing is held.
    WholeNodeWaitsForQuiet,
}

impl Scenario {
    /// Every scenario, in the order [`super::run`] runs them.
    pub const ALL: [Scenario; 15] = [
        Scenario::StartIsIdempotent,
        Scenario::CancelUntilAbsent,
        Scenario::SelfFencesAfterSilence,
        Scenario::UnackedResultIsResent,
        Scenario::RetiredFenceIsRefused,
        Scenario::AnsweredResultIsDropped,
        Scenario::UsageIsReportedOnEveryEnd,
        Scenario::FlawedStartIsRefused,
        Scenario::CleanupFreesTheSlotLast,
        Scenario::HostJobNeedsConsent,
        Scenario::HostJobTaintsWhateverItsEnd,
        Scenario::TaintingCheckTaintsOnItsOwn,
        Scenario::TaintIsKept,
        Scenario::RestartKeepsWhatItHolds,
        Scenario::WholeNodeWaitsForQuiet,
    ];

    /// Runs this scenario on a fresh harness from `make`, and on more fresh ones where the
    /// scenario needs them.
    pub(crate) fn run<H: AgentHarness>(&self, make: &impl Fn() -> H) -> Verdict {
        let mut notes = Notes(Vec::new());
        let harness = &mut make();
        match self {
            Self::StartIsIdempotent => start_is_idempotent(harness, &mut notes),
            Self::CancelUntilAbsent => cancel_until_absent(harness, &mut notes),
            Self::SelfFencesAfterSilence => self_fences_after_silence(harness, &mut notes),
            Self::UnackedResultIsResent => unacked_result_is_resent(harness, &mut notes),
            Self::RetiredFenceIsRefused => retired_fence_is_refused(harness, &mut notes),
            Self::AnsweredResultIsDropped => answered_result_is_dropped(harness, &mut notes),
            Self::UsageIsReportedOnEveryEnd => usage_is_reported(harness, &mut notes),
            Self::FlawedStartIsRefused => flawed_start_is_refused(harness, &mut notes),
            Self::CleanupFreesTheSlotLast => host::cleanup_frees_the_slot_last(harness, &mut notes),
            Self::HostJobNeedsConsent => host::host_job_needs_consent(harness, &mut notes),
            Self::HostJobTaintsWhateverItsEnd => host::host_job_taints(make, &mut notes),
            Self::TaintingCheckTaintsOnItsOwn => host::tainting_checks(make, &mut notes),
            Self::TaintIsKept => host::taint_is_kept(harness, &mut notes),
            Self::RestartKeepsWhatItHolds => host::restart_keeps(harness, &mut notes),
            Self::WholeNodeWaitsForQuiet => host::whole_node_waits(harness, &mut notes),
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
    harness.start("lease");
    harness.start("lease");
    let seen = harness.observed();
    notes.expect(
        seen.launches.get("lease") == Some(&1),
        "the job is launched once for two Starts",
    );
    notes.expect(seen.running.contains("lease"), "the lease is running");
}

fn cancel_until_absent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("lease");
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
    harness.start("lease");
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

fn self_fences_after_silence<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    let Some(window) = harness.fence_window() else {
        notes.expect(
            false,
            "the agent has no self-fence, so it cannot stop its leases by itself",
        );
        return;
    };
    harness.start("lease");
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
    harness.start("lease");
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

fn retired_fence_is_refused<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start_fenced("job", "first");
    harness.start_fenced("job", "first");
    let seen = harness.observed();
    notes.expect(
        seen.running.contains("job"),
        "the job runs under its first allocation",
    );
    notes.expect(
        seen.launches.get("job") == Some(&1),
        "a fenced start repeated while the job runs launches it once",
    );
    harness.complete("job");
    notes.expect(
        harness.ack("job") == Delivery::Delivered,
        "the acknowledgement of the result is delivered",
    );
    harness.heartbeat();
    harness.start_fenced("job", "first");
    let seen = harness.observed();
    notes.expect(
        !seen.running.contains("job"),
        "a start whose fence is retired does not run",
    );
    notes.expect(
        seen.launches.get("job") == Some(&1),
        "a start whose fence is retired launches nothing",
    );
    harness.start_fenced("job", "second");
    let seen = harness.observed();
    notes.expect(
        seen.running.contains("job"),
        "the same job under a new allocation is a new fence and runs",
    );
    notes.expect(
        seen.launches.get("job") == Some(&2),
        "the new fence launches the job once more",
    );
}

/// How many times `lease` is among the results the harness saw sent.
fn sends(harness: &impl AgentHarness, lease: &str) -> usize {
    let sent = harness.observed().results_sent;
    sent.iter().filter(|sent| *sent == lease).count()
}

fn answered_result_is_dropped<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    for answer in Answer::KNOWN {
        let lease = format!("{answer:?}");
        harness.start(&lease);
        harness.complete(&lease);
        notes.expect(
            harness.answer(&lease, answer) == Delivery::Delivered,
            &format!("the answer {answer:?} is delivered"),
        );
        harness.heartbeat();
        notes.expect(
            sends(harness, &lease) == 1,
            &format!("a result answered {answer:?} is not sent again"),
        );
    }
    harness.start("unrecognised");
    harness.complete("unrecognised");
    harness.answer("unrecognised", Answer::Unrecognised);
    harness.heartbeat();
    notes.expect(
        sends(harness, "unrecognised") == 2,
        "a result answered with a verdict the agent does not know is sent again",
    );
    harness.start("running");
    harness.answer("running", Answer::Duplicate);
    notes.expect(
        harness.answer("stranger", Answer::Accepted) == Delivery::Delivered,
        "an answer for a lease the agent does not hold is delivered",
    );
    let seen = harness.observed();
    notes.expect(
        seen.running.contains("running"),
        "an answer for a lease that has no result leaves it running",
    );
    notes.expect(
        seen.launches.get("running") == Some(&1),
        "an answer for a lease that has no result launches nothing more",
    );
    notes.expect(
        !seen.results_sent.iter().any(|sent| sent == "stranger"),
        "an answer for a lease the agent does not hold makes no result",
    );
}

fn usage_is_reported<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start_fenced("ends", "first");
    harness.complete("ends");
    harness.start_fenced("fails", "first");
    harness.fail("fails");
    harness.start_fenced("killed", "first");
    harness.cancel("killed");
    let usage = harness.observed().usage;
    notes.expect(
        usage.contains("ends"),
        "the result of a job that ended carries its usage",
    );
    notes.expect(
        usage.contains("fails"),
        "the result of a job that failed carries its usage",
    );
    notes.expect(
        usage.contains("killed"),
        "the result of a job killed by a stop carries its usage",
    );
}

fn flawed_start_is_refused<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    for flaw in Flaw::ALL {
        let lease = format!("{flaw:?}");
        notes.expect(
            harness.start_flawed(&lease, flaw) == Delivery::Delivered,
            &format!("the start with {flaw:?} is delivered"),
        );
        let seen = harness.observed();
        notes.expect(
            !seen.launches.contains_key(&lease),
            &format!("a start with {flaw:?} launches nothing"),
        );
        notes.expect(
            seen.refused.contains(&lease),
            &format!("a start with {flaw:?} is refused"),
        );
    }
    harness.start("sound");
    let seen = harness.observed();
    notes.expect(
        seen.running.contains("sound"),
        "a well-formed start after the refused ones runs",
    );
}
