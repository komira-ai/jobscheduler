//! A runtime that puts every container in a control group the agent can read, and a run
//! whose group the agent never learned: the agent cannot see that container, so the host is
//! tainted as unchecked while the run runs, and a runtime that makes no such promise (the
//! engines) is not tainted for lacking one. The runtime is the in-memory fake; the end of a
//! run is a call the test makes, not a wait.

use js_agent::lifecycle::code;
use js_agent::lifecycle::taint::reason;
use js_agent_wire::Taint;

use super::{Harness, job, names};

/// The wait of a lifecycle that was not given one: `DEFAULT_SETTLE`, one second.
const SETTLE_MS: u64 = 1000;

/// The taint of a run the agent could not see.
fn blind(run_id: &str) -> Taint {
    Taint {
        reason: reason::UNCHECKED.to_owned(),
        run_id: Some(run_id.to_owned()),
        check: Some(code::HOST_NOT_VISIBLE.to_owned()),
    }
}

/// Asserts that a run of a runtime that places its containers in readable groups, whose
/// group was never named, taints the host as unchecked while it runs (the same taint a start
/// the agent cannot see gets), and that the next start is then refused. It catches a lease that is never sampled and never checked
/// for its group and passes silently.
#[test]
fn a_run_whose_group_was_never_seen_taints_the_host() {
    let mut h = Harness::new("blind-cgroup");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    assert!(!h.life.is_tainted(), "the start alone says nothing");
    let first = names(&h.life.step());
    assert!(
        !first.iter().any(|event| event.starts_with("tainted")),
        "one sighting is not blindness: {first:?}"
    );
    assert!(!h.life.is_tainted());
    h.clock.advance(SETTLE_MS);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"tainted r1 unchecked".to_owned()),
        "{events:?}"
    );
    assert_eq!(h.life.pending_taint(), Some(blind("r1")));
    let again = names(&h.life.step());
    assert!(
        !again.iter().any(|event| event.starts_with("tainted")),
        "{again:?}"
    );
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.pending_taint(), Some(blind("r1")), "told once");
}

/// Asserts that a run whose group is known when it ends taints nothing, whether the runtime
/// named it when the run started or only at a later window (`find_cgroups`). It catches a
/// taint decided by the first lookup alone.
#[test]
fn a_run_whose_group_is_known_taints_nothing() {
    let mut h = Harness::new("seen-cgroup");
    h.fake.promise_cgroups(true);
    h.fake
        .set_cgroup("r1", "/sys/fs/cgroup/actions/lease-r1/ctr");
    h.start(job("r1"));
    h.life.step();
    assert!(!h.life.is_tainted(), "watched while it runs");
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.life.is_tainted());

    h.start(job("r2"));
    assert!(!h.life.find_cgroups().is_empty(), "r2 has none yet");
    h.fake
        .set_cgroup("r2", "/sys/fs/cgroup/actions/lease-r2/ctr");
    assert!(h.life.find_cgroups().is_empty());
    h.fake.exit("r2", 0);
    h.life.step();
    assert!(!h.life.is_tainted());
}

/// Asserts that a run whose group the runtime names only after the start, while the run
/// still runs, is not blind: the look the lifecycle makes when it watches the run finds it.
/// It catches a taint decided by the start's lookup alone.
#[test]
fn a_group_named_while_the_run_runs_is_found() {
    let mut h = Harness::new("late-cgroup");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    h.fake
        .set_cgroup("r1", "/sys/fs/cgroup/actions/lease-r1/ctr");
    h.life.step();
    assert!(!h.life.is_tainted());
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.life.is_tainted());
}

/// Asserts that a run that ended before the agent looked at it, with no group (the runtime
/// has none to give for a run that is over), taints nothing. It catches a taint that fires
/// for an ended run.
#[test]
fn an_ended_run_with_no_group_is_not_blind() {
    let mut h = Harness::new("ended-no-cgroup");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
}

/// Asserts that a run that is read as running with no group at one step, and as ended at the
/// next, taints nothing: a container that exits at once has no group left by the time the agent
/// looks, and its end is read a moment after. It catches a host tainted as unchecked by the first
/// sighting of a run that has already ended.
#[test]
fn a_run_that_ends_after_one_look_with_no_group_is_not_blind() {
    let mut h = Harness::new("ends-after-one-look");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    let first = names(&h.life.step());
    assert!(
        !first.iter().any(|event| event.starts_with("tainted")),
        "{first:?}"
    );
    h.fake.exit("r1", 1);
    h.life.step();
    h.life.step();
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
}

/// Asserts that a run that has ended but whose end the runtime holds (a failed run whose
/// cloned sources are still being checked: `poll` says running, the group is gone) taints
/// nothing however many steps the hold lasts. It catches a host tainted as unchecked by a run
/// that ended, because the held end was read as a running run with no group twice.
#[test]
fn a_run_that_ended_and_whose_end_is_held_is_never_blind() {
    let mut h = Harness::new("held-end-no-cgroup");
    h.fake.promise_cgroups(true);
    h.fake.hold_end("r1", true);
    h.start(job("r1"));
    h.fake.exit("r1", 1);
    for _ in 0..4 {
        let events = names(&h.life.step());
        assert!(
            !events.iter().any(|event| event.starts_with("tainted")),
            "{events:?}"
        );
    }
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
    h.fake.hold_end("r1", false);
    h.life.step();
    assert!(!h.life.is_tainted());
}

/// Asserts that a runtime that does not place its containers in readable groups (the
/// default: the engines, which may run with no `--actions-cgroup`) is not tainted for a run
/// with no group. It catches a taint that would hit every run of every such agent.
#[test]
fn a_runtime_with_no_groups_of_its_own_is_never_blind_for_lack_of_one() {
    let mut h = Harness::new("no-cgroup-promise");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
}

/// Asserts that steps that follow one another with no time between them are not blindness:
/// a run seen running with no group at ten steps in a row at one instant taints nothing, one
/// millisecond short of the wait still taints nothing, and the step at the wait taints once.
/// It catches a blindness rule that counts steps: the worker steps again at once after every
/// order, so a burst of starts made the host "blind" to a run that had only just ended.
#[test]
fn steps_with_no_time_between_them_are_not_blindness() {
    let mut h = Harness::new("blind-steps-at-once");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    for _ in 0..10 {
        let events = names(&h.life.step());
        assert!(
            !events.iter().any(|event| event.starts_with("tainted")),
            "{events:?}"
        );
    }
    h.clock.advance(SETTLE_MS - 1);
    h.life.step();
    assert!(!h.life.is_tainted(), "a millisecond short of the wait");
    h.clock.advance(1);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"tainted r1 unchecked".to_owned()),
        "{events:?}"
    );
    assert_eq!(h.life.pending_taint(), Some(blind("r1")));
}

/// Asserts that the wait is the one the lifecycle was given (`--settle-ms`): with five
/// seconds, four seconds after the first sighting taint nothing and five do. It catches a
/// wait that is a constant of the code.
#[test]
fn the_blindness_wait_is_the_one_given() {
    let mut h = Harness::new("blind-wait-given");
    h.life = h.life.with_blind_wait(std::time::Duration::from_secs(5));
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    h.life.step();
    h.clock.advance(4_999);
    h.life.step();
    assert!(!h.life.is_tainted());
    h.clock.advance(1);
    h.life.step();
    assert!(h.life.is_tainted());
}

/// Asserts that a run that ends within the wait taints nothing however many steps saw it
/// without a group: it is the ended run of a container that exited at once, read as running
/// until its end is read. It catches the taint of the host on the one-second-old run that was
/// done before the agent could name its group.
#[test]
fn a_run_that_ends_within_the_wait_is_not_blind_however_often_it_was_seen() {
    let mut h = Harness::new("ends-within-wait");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    for _ in 0..5 {
        h.life.step();
    }
    h.clock.advance(SETTLE_MS - 1);
    h.fake.exit("r1", 0);
    h.life.step();
    h.life.step();
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
}
