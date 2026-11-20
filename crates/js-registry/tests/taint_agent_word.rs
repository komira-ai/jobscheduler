//! The agent's own word about its taint (`Heartbeat::tainted`) and the taint the registry
//! names in its reply (`HeartbeatReply::host_taint`). The registry is driven directly, on a
//! manual clock, with hand-built heartbeats.

mod support;

use js_agent_wire::{Heartbeat, Taint, refusal};
use js_registry::HostView;
use support::*;

fn taint(reason: &str) -> Taint {
    Taint {
        reason: reason.into(),
        run_id: Some("run-1".into()),
        check: Some("daemonised-child".into()),
    }
}

/// The taint the registry gives a host whose agent says it is tainted while the registry
/// holds none: it names no run and no check.
fn mirror() -> Taint {
    Taint {
        reason: refusal::HOST_TAINTED.into(),
        run_id: None,
        check: None,
    }
}

fn view(fx: &Fixture) -> HostView {
    let id = host("a");
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|view| view.id == id).unwrap()
}

/// A fixture with host `a` joined; returns the epoch.
fn joined() -> (Fixture, u64) {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    (fx, epoch)
}

/// A fixture with host `a` joined and tainted with `held`, as the lease server taints a host
/// it makes the taint for.
fn holding(held: &Taint) -> (Fixture, u64) {
    let (fx, epoch) = joined();
    assert!(fx.registry.taint_host(&host("a"), held.clone()));
    (fx, epoch)
}

/// A beat of host `a` that says whether the agent is tainted.
fn saying(fx: &Fixture, epoch: u64, tainted: Option<bool>) -> Heartbeat {
    Heartbeat {
        tainted,
        ..fx.heartbeat("a", epoch)
    }
}

/// Checks `beat`, then sends it, and asserts the check named what the beat then did: its
/// `cleared` flag says whether the taint held before ended, and the taint it names is the
/// one the host holds after when the beat gave it one. Returns the reply.
fn beat_agreeing(fx: &Fixture, beat: &Heartbeat) -> js_agent_wire::HeartbeatReply {
    let prior = view(fx).taint;
    let check = fx.registry.check_heartbeat(beat, &cred("t")).unwrap();
    assert_eq!(view(fx).taint, prior, "a check changes nothing");
    let reply = fx.registry.heartbeat(beat, &cred("t")).unwrap();
    let kept = prior.filter(|_| !check.cleared);
    let expected = check.taint.or(kept);
    assert_eq!(view(fx).taint, expected);
    assert_eq!(
        fx.registry.bindings()[&host("a")].taint(),
        expected.as_ref()
    );
    assert_eq!(reply.host_taint, expected);
    reply
}

/// Asserts that a beat that says the agent holds none (`tainted` false) ends every taint the
/// agent made, whatever its reason or fields (residue, unchecked, cleanup-failed,
/// marker-unreadable, check-owed and the `host-tainted` mirror): the check names the
/// clearing and no new taint, the reply names no taint and is no clear acknowledgement, and
/// the view and the binding hold none. It catches a registry that keeps a taint for good
/// after the agent that made it holds none, as after the agent's restart.
#[test]
fn a_beat_that_says_the_agent_holds_none_ends_an_agent_made_taint() {
    let reasons = [
        "residue",
        "unchecked",
        "cleanup-failed",
        "marker-unreadable",
        "check-owed",
        refusal::HOST_TAINTED,
    ];
    for reason in reasons {
        let held = taint(reason);
        let (fx, epoch) = holding(&held);
        let beat = saying(&fx, epoch, Some(false));
        let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
        assert_eq!((check.taint, check.cleared), (None, true), "{reason}");
        let reply = beat_agreeing(&fx, &beat);
        assert_eq!(reply.host_taint, None, "{reason}");
        assert!(!reply.taint_cleared_ack && !reply.taint_ack, "{reason}");
        assert_eq!(view(&fx).taint, None, "{reason}");
        assert_eq!(fx.registry.bindings()[&host("a")].taint(), None, "{reason}");
    }
}

/// Asserts that the taints the server makes itself outlive the agent's word that it holds
/// none: a host job's taint (`host-job`) and a refused whole-host prepare's (`host-unsafe`)
/// stay, the check names no clearing, and the reply names the taint still held; only the
/// operator's clear ends them. It catches an agent's word that ends a taint the agent's
/// marker never held.
#[test]
fn a_beat_that_says_the_agent_holds_none_leaves_a_host_job_and_a_host_unsafe_taint() {
    let (fx, epoch) = joined();
    assert!(fx.registry.taint_after_prepare(&host("a")));
    let unsafe_taint = view(&fx).taint.unwrap();
    assert_eq!(unsafe_taint.reason, "host-unsafe");
    let job_taint = Taint {
        reason: "host-job".into(),
        run_id: None,
        check: None,
    };
    let (fx_job, epoch_job) = holding(&job_taint);
    for (fx, epoch, held) in [(fx, epoch, unsafe_taint), (fx_job, epoch_job, job_taint)] {
        let beat = saying(&fx, epoch, Some(false));
        let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
        assert_eq!((check.taint, check.cleared), (None, false));
        let reply = beat_agreeing(&fx, &beat);
        assert_eq!(reply.host_taint, Some(held.clone()));
        assert_eq!(view(&fx).taint, Some(held.clone()));
        assert_eq!(fx.registry.clear_taint(&host("a")), Some(held));
        assert_eq!(view(&fx).taint, None);
    }
}

/// Asserts that a beat without the field (an agent that predates it, or a session that runs
/// no jobs) ends no taint, agent-made or not, and records the mirror of none: the host's
/// taint and the reply's `host_taint` are what they were. It catches a registry that reads a
/// missing field as an agent that holds none.
#[test]
fn a_beat_without_the_field_ends_nothing_and_records_nothing() {
    let held = taint("residue");
    let (fx, epoch) = holding(&held);
    let beat = saying(&fx, epoch, None);
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, false));
    let reply = beat_agreeing(&fx, &beat);
    assert_eq!(reply.host_taint, Some(held.clone()));
    assert_eq!(view(&fx).taint, Some(held));

    let (fx, epoch) = joined();
    let beat = saying(&fx, epoch, None);
    let reply = beat_agreeing(&fx, &beat);
    assert_eq!(reply.host_taint, None);
    assert_eq!(view(&fx).taint, None);
}

/// Asserts that a beat that says the agent is tainted (`tainted` true) while the registry
/// holds no taint records the mirror, reason `host-tainted` naming no run and no check, in
/// the view and the binding and in the check; that the next beats, which report a taint of
/// their own or say the same again, do not replace it (the first taint is kept); and that the
/// agent's clean beat then ends it. It catches a host the agent says is tainted that the
/// registry offers jobs, as after the operator cleared the server's taint while the marker
/// stayed.
#[test]
fn a_beat_that_says_the_agent_is_tainted_records_the_mirror_on_an_untainted_host() {
    let (fx, epoch) = joined();
    let beat = saying(&fx, epoch, Some(true));
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (Some(mirror()), false));
    let reply = beat_agreeing(&fx, &beat);
    assert_eq!(reply.host_taint, Some(mirror()));
    assert_eq!(view(&fx).taint, Some(mirror()));
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), Some(&mirror()));

    let mut report = saying(&fx, epoch, Some(true));
    report.taint = Some(taint("residue"));
    let check = fx.registry.check_heartbeat(&report, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, false));
    let reply = beat_agreeing(&fx, &report);
    assert_eq!(reply.host_taint, Some(mirror()));
    let again = saying(&fx, epoch, Some(true));
    assert_eq!(beat_agreeing(&fx, &again).host_taint, Some(mirror()));

    let clean = saying(&fx, epoch, Some(false));
    assert_eq!(beat_agreeing(&fx, &clean).host_taint, None);
    assert_eq!(view(&fx).taint, None);
}

/// Asserts that a beat that reports a taint cleared records no mirror, though its `tainted`
/// is true until the clear is acknowledged: on a host that holds no taint (a retry of a clear
/// taken already) the host stays untainted, and on a host that holds the taint it ends it,
/// the check and the reply agreeing. It catches a clear the agent's own `tainted` undoes.
#[test]
fn a_beat_that_reports_a_clear_records_no_mirror() {
    let cleared = taint("residue");
    let (fx, epoch) = joined();
    let mut beat = saying(&fx, epoch, Some(true));
    beat.taint_cleared = Some(cleared.clone());
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, false));
    let reply = beat_agreeing(&fx, &beat);
    assert!(reply.taint_cleared_ack);
    assert_eq!(reply.host_taint, None);
    assert_eq!(view(&fx).taint, None);

    let (fx, epoch) = holding(&cleared);
    let mut beat = saying(&fx, epoch, Some(true));
    beat.taint_cleared = Some(cleared);
    let check = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!((check.taint, check.cleared), (None, true));
    let reply = beat_agreeing(&fx, &beat);
    assert!(reply.taint_cleared_ack);
    assert_eq!(reply.host_taint, None);
    assert_eq!(view(&fx).taint, None);
}

/// Asserts that the reply names the taint the host holds once the beat is taken: the taint
/// the beat reports (the reply that acknowledges it already names it), a taint held before the
/// beat, and none for a clean host
/// and for a host the beat cleared. It catches a reply that names the taint held before the
/// beat, or none.
#[test]
fn the_reply_names_the_taint_held_after_the_beat() {
    let (fx, epoch) = joined();
    let clean = fx.heartbeat("a", epoch);
    assert_eq!(beat_agreeing(&fx, &clean).host_taint, None);

    let held = taint("residue");
    let mut report = fx.heartbeat("a", epoch);
    report.taint = Some(held.clone());
    let reply = beat_agreeing(&fx, &report);
    assert!(reply.taint_ack);
    assert_eq!(reply.host_taint, Some(held.clone()));

    let quiet = fx.heartbeat("a", epoch);
    assert_eq!(beat_agreeing(&fx, &quiet).host_taint, Some(held.clone()));

    let mut cleared = fx.heartbeat("a", epoch);
    cleared.taint_cleared = Some(held);
    assert_eq!(beat_agreeing(&fx, &cleared).host_taint, None);
}
