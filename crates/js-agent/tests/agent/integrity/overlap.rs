//! Runs that overlap, as the lifecycle runs their checks: the real [`Integrity`] over the
//! in-memory host, with a run whose create is under way off the worker (held by
//! [`Deferring`]) beside a run that ends. Under the direct runner every job runs as the one
//! job user, so a uid does not tell two runs apart; the lease's control group
//! (`<actions>/js-<agent tag>-<run id>/ctr`) does. The create of a held run has made that
//! group and started its processes before the lifecycle takes its answer, so they are on
//! the host while the run is still `starting`. Nothing waits: the settle wait is off and the
//! clock is the test's.

use js_agent::integrity::{FakeHost, Integrity};
use js_agent_wire::{HeldRun, HeldState};

use super::{JOB_UID, process, process_named, started};
use crate::lifecycle::deferring::{Control, Deferring, lease};
use crate::lifecycle::{Harness, cleanup, job, names};

/// The agent's group, beside `actions/` as the delegated tree makes it; short, so that a
/// detail naming two processes fits the finding whole.
const AGENT: &str = "0::/js/agent\n";

/// The control groups of a process in the container of the lease of run `run`.
fn ctr_of(run: &str) -> String {
    format!("0::/js/actions/{}/ctr", lease(run))
}

/// A host whose agent is in [`AGENT`], and a harness over a [`Deferring`] runtime whose runs
/// are checked by the real checks over it, with `r1` running and the create of `r2` under
/// way (held) beside it. `r2`'s create has started its container: its `js-init` (pid 400)
/// and the job's process (pid 401) run as the job user in `r2`'s lease group.
fn overlapping(name: &str) -> (FakeHost, Harness, Control) {
    let host = FakeHost::quiet();
    host.set_file("/proc/self/cgroup", AGENT);
    let checks: Integrity = started(&host, vec![]);
    let (mut h, gate) = Deferring::harness_checked(name, Some(Box::new(checks)));
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    gate.hold("r2");
    assert!(h.start(job("r2")).is_empty());
    let init = process(400, 399, JOB_UID, 900, &ctr_of("r2"));
    host.start_process(process_named(init, "js-init", 'S'));
    let task = process(401, 400, JOB_UID, 901, &ctr_of("r2"));
    host.start_process(process_named(task, "kbf-task", 'R'));
    (host, h, gate)
}

/// Asserts that when a run ends while another run's create is under way, the processes of
/// the other run (its `js-init` and its job, of the same job user, in its own lease group)
/// are not the ended run's leftover: the check passes, the host is not tainted, the ended
/// run's slot is freed (its result waits for the server), and the other run is still
/// `starting`. Catches a check that is told
/// only of the containers the lifecycle holds as running or exited, which on a busy host
/// taints it as `process-as-job-user` for a neighbour that is starting.
#[test]
fn a_run_whose_create_is_under_way_leaves_no_residue_for_a_run_that_ends() {
    let (_host, mut h, _gate) = overlapping("overlap-clean");
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert!(!h.life.is_tainted());
    assert!(h.life.findings().is_empty());
    assert_eq!(h.life.busy_slots(), [1], "r1's slot is free, r2's is held");
    let held: Vec<(String, HeldState)> = h
        .life
        .held()
        .into_iter()
        .map(|HeldRun { run_id, state }| (run_id, state))
        .collect();
    assert_eq!(
        held,
        [
            ("r1".to_owned(), HeldState::ExitedUnacked),
            ("r2".to_owned(), HeldState::Starting)
        ]
    );
}

/// Asserts that a process of the job user outside every live run's group still taints the
/// host when a run ends beside a run whose create is under way: one left in the ended run's
/// own lease group and one in the lease group of a run the agent does not hold are both
/// named, the neighbour's two processes are not, and the finding is the whole detail by
/// exact equality (it fits the bound). Catches an exemption of every lease group under `actions/`, or of every
/// process of the job user, which would let a real leftover pass.
#[test]
fn a_leftover_outside_every_live_run_still_taints_beside_a_create_under_way() {
    let (host, mut h, _gate) = overlapping("overlap-leftover");
    host.start_process(process(500, 499, JOB_UID, 902, &ctr_of("r1")));
    host.start_process(process(501, 499, JOB_UID, 903, &ctr_of("r9")));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "tainted r1 residue",
            "finding r1 process-as-job-user"
        ]
    );
    assert_eq!(
        h.life.findings()[0].message,
        format!(
            "pid 500 (stray, state S, cgroup {}, owner {JOB_UID}) is running after job r1, \
             pid 501 (stray, state S, cgroup {}, owner {JOB_UID}) is running after job r1",
            ctr_of("r1"),
            ctr_of("r9")
        )
    );
}

/// Asserts that the clean check beside a create under way does not take the neighbour's
/// processes into the baseline: once that run has started, run and been removed, its
/// processes still on the host are its own leftover, named against it. Catches a baseline
/// retaken with a starting neighbour's processes in it, which would hide whatever that run
/// leaves behind.
#[test]
fn the_neighbours_processes_stay_out_of_the_baseline_and_count_against_it() {
    let (_host, mut h, gate) = overlapping("overlap-baseline");
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.life.is_tainted());
    gate.release("r2");
    assert_eq!(names(&h.life.step()), ["started r2"]);
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "finding r2 process-as-job-user");
    assert_eq!(
        h.life.findings()[0].message,
        format!(
            "pid 400 (js-init, state S, cgroup {0}, owner {JOB_UID}) is running after job r2, \
             pid 401 (kbf-task, state R, cgroup {0}, owner {JOB_UID}) is running after job r2",
            ctr_of("r2")
        )
    );
}
