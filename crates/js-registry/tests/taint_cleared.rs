//! The taint an agent reports cleared (`Heartbeat::taint_cleared`): one of the two ways the
//! host's own word ends its taint in the registry (the other, `Heartbeat::tainted` false, is in
//! `taint_agent_word.rs`). The registry is driven directly, on a manual clock, with hand-built
//! heartbeats.

mod support;

use js_agent_wire::{Outcome, RunResult, Taint, refusal};
use js_registry::HostView;
use support::*;

fn taint(reason: &str, check: &str) -> Taint {
    Taint {
        reason: reason.into(),
        run_id: Some("run-1".into()),
        check: Some(check.into()),
    }
}

fn view(fx: &Fixture) -> HostView {
    let id = host("a");
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|view| view.id == id).unwrap()
}

/// A start the agent refused as `host-tainted`, in a result not yet acknowledged.
fn refused_start(run: &str) -> RunResult {
    RunResult {
        run_id: run.into(),
        lease_id: "1-1".into(),
        outcome: Outcome::FailedToStart,
        exit_code: None,
        detail: Some(refusal::HOST_TAINTED.into()),
        log_tail: None,
    }
}

/// A fixture with host `a` joined and tainted by its own report of `held`; returns the epoch.
fn tainted(held: &Taint) -> (Fixture, u64) {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint = Some(held.clone());
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(view(&fx).taint, Some(held.clone()));
    (fx, epoch)
}

/// Asserts that a heartbeat reporting the held taint cleared ends it: the reply says
/// `taint_cleared_ack`, the host's view and the bindings a caller persists carry no taint, and a
/// check made before the beat names the clearing and changes nothing. It catches a registry
/// that keeps a taint for good, and a check that does not say the binding changes.
#[test]
fn a_cleared_report_equal_to_the_held_taint_ends_it() {
    let held = taint("residue", "daemonised-child");
    let (fx, epoch) = tainted(&held);
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint_cleared = Some(held.clone());

    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, true));
    assert_eq!(
        view(&fx).taint,
        Some(held.clone()),
        "a check changes nothing"
    );

    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(reply.taint_cleared_ack);
    assert!(
        !reply.taint_ack,
        "the report of a taint is not what is answered"
    );
    assert_eq!(view(&fx).taint, None);
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), None);
}

/// Asserts that a cleared report that differs from the held taint in any field changes
/// nothing, and is still acknowledged so the agent stops sending it: a host job's taint the
/// server set, or a taint the agent never reported, outlives the agent's own clear. It
/// catches a clear by reason alone.
#[test]
fn a_cleared_report_of_another_taint_leaves_the_held_one() {
    let held = taint("host-job", "none");
    let (fx, epoch) = tainted(&held);
    for other in [
        taint("residue", "none"),
        taint("host-job", "other"),
        Taint {
            run_id: None,
            ..held.clone()
        },
    ] {
        let mut beat = fx.heartbeat("a", epoch);
        beat.taint_cleared = Some(other);
        let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
        assert_eq!((check.taint, check.cleared), (None, false));
        let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
        assert!(reply.taint_cleared_ack);
        assert_eq!(view(&fx).taint, Some(held.clone()));
    }
}

/// Asserts that a cleared report from a host that holds no taint is acknowledged and
/// changes nothing (the report was a retry of a clear the registry took already). It
/// catches a registry that invents a taint or refuses the beat.
#[test]
fn a_cleared_report_from_an_untainted_host_is_only_acknowledged() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint_cleared = Some(taint("residue", "daemonised-child"));
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, false));
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(reply.taint_cleared_ack);
    assert_eq!(view(&fx).taint, None);
}

/// Asserts that a beat that reports the taint cleared and a new taint leaves the host with
/// the new one, and that a start refused as `host-tainted` in the same beat (a result from
/// before the clear that is not yet acknowledged) taints nothing. It catches a clear that
/// is undone by the stale refusal that travels with it, which only an operator could then
/// end.
#[test]
fn a_beat_that_clears_takes_the_new_taint_and_ignores_stale_refusals() {
    let held = taint("residue", "daemonised-child");
    let (fx, epoch) = tainted(&held);
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint_cleared = Some(held.clone());
    beat.results = vec![refused_start("r9")];
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, true));
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(view(&fx).taint, None);

    let (fx, epoch) = tainted(&held);
    let again = taint("residue", "process-as-job-user");
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint_cleared = Some(held.clone());
    beat.taint = Some(again.clone());
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (Some(again.clone()), true));
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(view(&fx).taint, Some(again.clone()));
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), Some(&again));
}

/// Asserts that after a clear the host is an ordinary host again: a later start refused as
/// `host-tainted` taints it as it would any untainted host. It catches a clear that leaves
/// the host unable to be tainted again.
#[test]
fn a_host_cleared_can_be_tainted_again() {
    let held = taint("residue", "daemonised-child");
    let (fx, epoch) = tainted(&held);
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint_cleared = Some(held.clone());
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(view(&fx).taint, None);

    let mut beat = fx.heartbeat("a", epoch);
    beat.results = vec![refused_start("r10")];
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(
        view(&fx).taint.map(|taint| taint.reason),
        Some("host-tainted".to_owned())
    );
}

/// Asserts that the taint a refused start made (reason `host-tainted`, run id the refused
/// run; the agent's own report was not held when the start was refused) is ended by the
/// agent's report that it cleared its own taint, whatever that taint's fields: the refusal
/// only mirrors the agent's taint, and nothing else would ever end it, since the agent is
/// clean and reports no taint. The check names the clearing, the reply acknowledges it and
/// the bindings carry no taint. It catches a host left tainted in the server, taking no job,
/// after its agent is clean.
#[test]
fn a_cleared_report_ends_the_taint_a_refused_start_made() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    beat.results = vec![refused_start("r1")];
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    let made = view(&fx).taint.unwrap();
    assert_eq!(made.reason, "host-tainted");

    let mut beat = fx.heartbeat("a", epoch);
    beat.taint_cleared = Some(taint("residue", "daemonised-child"));
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, true));
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(reply.taint_cleared_ack);
    assert_eq!(view(&fx).taint, None);
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), None);
}
