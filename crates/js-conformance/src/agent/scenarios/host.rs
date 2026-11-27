//! The scenarios of what a run leaves on its host: the cleanup, the checks and the taint,
//! a restart of the agent, and a host held whole for one lease.

use super::{Notes, sends};
use crate::agent::{AgentHarness, Cleanup, Delivery, HostCheck};

/// The steps of the cleanup the agent reported for `lease`, in order.
fn cleanup_of(harness: &impl AgentHarness, lease: &str) -> Vec<Cleanup> {
    let cleanup = harness.observed().cleanup.into_iter();
    let of_lease = cleanup.filter(|(of, _)| of == lease);
    of_lease.map(|(_, step)| step).collect()
}

/// The ways a job ends that [`host_job_taints`] tries, each on a fresh host.
#[derive(Debug, Clone, Copy)]
enum End {
    Completes,
    Fails,
    IsCancelled,
}

pub(super) fn cleanup_frees_the_slot_last<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("clean");
    harness.complete("clean");
    notes.expect(
        cleanup_of(harness, "clean")
            == [
                Cleanup::ContainerGone,
                Cleanup::ScratchGone,
                Cleanup::SlotFreed,
            ],
        "the container and the scratch are gone before the slot is freed",
    );
    notes.expect(
        !harness.observed().tainted,
        "a run whose cleanup passed leaves the host untainted",
    );
    harness.remove_fails("stuck");
    harness.start("stuck");
    harness.complete("stuck");
    let steps = cleanup_of(harness, "stuck");
    notes.expect(
        !steps.contains(&Cleanup::SlotFreed),
        "a container that could not be removed frees no slot",
    );
    notes.expect(
        harness.observed().tainted,
        "a container that could not be removed taints the host",
    );
}

pub(super) fn host_job_needs_consent<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    notes.expect(
        harness.start_host("unconsented") == Delivery::Delivered,
        "the host start is delivered",
    );
    let seen = harness.observed();
    notes.expect(
        !seen.launches.contains_key("unconsented"),
        "a host start without the operator's consent launches nothing",
    );
    notes.expect(
        seen.refused.contains("unconsented"),
        "a host start without the operator's consent is refused",
    );
    notes.expect(
        !seen.tainted,
        "a refused host start leaves the host untainted",
    );
    harness.consent_to_host_jobs();
    harness.start_host("consented");
    let seen = harness.observed();
    notes.expect(
        seen.launches.get("consented") == Some(&1),
        "a host start after the operator's consent is launched",
    );
}

pub(super) fn host_job_taints<H: AgentHarness>(make: &impl Fn() -> H, notes: &mut Notes) {
    for end in [End::Completes, End::Fails, End::IsCancelled] {
        let harness = &mut make();
        harness.consent_to_host_jobs();
        harness.start_host("host");
        match end {
            End::Completes => harness.complete("host"),
            End::Fails => harness.fail("host"),
            End::IsCancelled => {
                harness.cancel("host");
            }
        }
        notes.expect(
            harness.observed().tainted,
            &format!("a host job that {end:?} leaves the host tainted"),
        );
    }
}

pub(super) fn tainting_checks<H: AgentHarness>(make: &impl Fn() -> H, notes: &mut Notes) {
    for check in [HostCheck::LeftoverProcess, HostCheck::CannotRun] {
        let harness = &mut make();
        harness.start("job");
        harness.check_finds("job", check);
        harness.complete("job");
        let seen = harness.observed();
        notes.expect(
            seen.tainted,
            &format!("a check that finds {check:?} taints the host on its own"),
        );
        notes.expect(
            seen.findings.contains("job"),
            &format!("a check that finds {check:?} is a finding of the run"),
        );
        notes.expect(
            !cleanup_of(harness, "job").contains(&Cleanup::SlotFreed),
            &format!("a check that finds {check:?} keeps the run's slot"),
        );
    }
}

pub(super) fn taint_is_kept<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.consent_to_host_jobs();
    harness.start_host("host");
    harness.complete("host");
    let reports = harness.observed().taint_reports;
    harness.heartbeat();
    notes.expect(
        harness.observed().taint_reports == reports + 1,
        "an unacknowledged taint is in every heartbeat",
    );
    notes.expect(
        harness.ack_taint() == Delivery::Delivered,
        "the acknowledgement of the taint is delivered",
    );
    harness.heartbeat();
    let seen = harness.observed();
    notes.expect(
        seen.taint_reports == reports + 1,
        "an acknowledged taint is not reported again",
    );
    notes.expect(
        seen.tainted,
        "the acknowledgement stops the report, not the taint",
    );
    harness.restart();
    let seen = harness.observed();
    notes.expect(
        seen.taint_reports == reports + 2,
        "the first heartbeat after a restart reports the taint again",
    );
    notes.expect(
        seen.tainted,
        "a restart, and the answer to its first heartbeat, leave the host tainted",
    );
    harness.start("after");
    let seen = harness.observed();
    notes.expect(
        !seen.launches.contains_key("after"),
        "a tainted host launches no start",
    );
    notes.expect(
        seen.refused.contains("after"),
        "a tainted host refuses a start",
    );
}

pub(super) fn restart_keeps<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("running");
    harness.start("ended");
    harness.complete("ended");
    let before = sends(harness, "ended");
    harness.restart();
    let seen = harness.observed();
    notes.expect(
        seen.running.contains("running"),
        "the first heartbeat after a restart lists the run that kept running",
    );
    notes.expect(
        seen.launches.get("running") == Some(&1),
        "a run that kept running through a restart is not launched again",
    );
    notes.expect(
        sends(harness, "ended") == before + 1,
        "the first heartbeat after a restart repeats the unacknowledged result",
    );
}

pub(super) fn whole_node_waits<H: AgentHarness>(harness: &mut H, notes: &mut Notes) {
    harness.start("before");
    notes.expect(
        harness.hold_whole("whole") == Delivery::Delivered,
        "the hold of the host is delivered",
    );
    notes.expect(
        !harness.whole_ready(),
        "a host is not ready for a whole-node lease while a run is held on it",
    );
    harness.start_whole("early");
    harness.start("beside");
    let seen = harness.observed();
    notes.expect(
        !seen.launches.contains_key("early"),
        "a run of the whole-node lease does not start before the host is ready",
    );
    notes.expect(
        !seen.launches.contains_key("beside"),
        "a run of another lease does not start on a host held whole",
    );
    harness.complete("before");
    for lease in ["before", "early", "beside"] {
        harness.ack(lease);
    }
    notes.expect(
        harness.whole_ready(),
        "a host is ready for its whole-node lease once nothing is held on it",
    );
    harness.start_whole("whole-run");
    harness.start("other");
    let seen = harness.observed();
    notes.expect(
        seen.running.contains("whole-run"),
        "a run of the whole-node lease starts once the host is ready",
    );
    notes.expect(
        !seen.launches.contains_key("other"),
        "a run of another lease does not start on a host that is ready for its lease",
    );
}
