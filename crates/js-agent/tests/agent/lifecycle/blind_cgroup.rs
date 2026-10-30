//! A runtime that puts every container in a control group the agent can read, and a run
//! whose group the agent never learned: the agent cannot see that container, so the host is
//! tainted as unchecked when the run ends, and a runtime that makes no such promise (the
//! engines) is not tainted for lacking one. The runtime is the in-memory fake; the end of a
//! run is a call the test makes, not a wait.

use js_agent::lifecycle::code;
use js_agent::lifecycle::taint::reason;
use js_agent_wire::Taint;

use super::{Harness, job, names};

/// The taint of a run the agent could not see.
fn blind(run_id: &str) -> Taint {
    Taint {
        reason: reason::UNCHECKED.to_owned(),
        run_id: Some(run_id.to_owned()),
        check: Some(code::HOST_NOT_VISIBLE.to_owned()),
    }
}

/// Asserts that a run of a runtime that places its containers in readable groups, whose
/// group was never named, taints the host as unchecked when it ends (the same taint a start
/// the agent cannot see gets), that the next start is then refused, and that the result of
/// the run is still reported. It catches a lease that is never sampled and never checked
/// for its group and passes silently.
#[test]
fn a_run_whose_group_was_never_seen_taints_the_host() {
    let mut h = Harness::new("blind-cgroup");
    h.fake.promise_cgroups(true);
    h.start(job("r1"));
    assert!(!h.life.is_tainted(), "nothing is said while it runs");
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"tainted r1 unchecked".to_owned()),
        "{events:?}"
    );
    assert!(events.contains(&"result r1".to_owned()), "{events:?}");
    assert_eq!(h.life.pending_taint(), Some(blind("r1")));
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
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
