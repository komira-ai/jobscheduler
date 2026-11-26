//! The registry as a cache of the store's `node/<host>` rows: a session another server's
//! hello wrote is adopted (`Registry::adopt`), a hello admitted here is numbered by the store
//! (`Registry::renumber`) or given up (`Registry::abandon`), and a hello from the process that
//! holds a live host replaces its session. The registry is driven on a manual clock.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_agent_wire::Taint;
use js_registry::{
    Adopted, Binding, DEFAULT_LOST_RETENTION_MS, HostMark, HostState, HostView, NodeSession,
    Placer, Refusal, session_digest,
};
use js_types::cordon::Cordon;
use js_types::{HostId, Shape};
use support::*;

fn view(fx: &Fixture, name: &str) -> HostView {
    fx.registry
        .hosts()
        .into_iter()
        .find(|h| h.id == host(name))
        .unwrap()
}

/// A session of epoch `epoch` whose secret is `secret`, in pool `builders`, heard just now,
/// bound to the credential `t`.
fn node(epoch: u64, secret: &str) -> NodeSession {
    NodeSession {
        epoch,
        session: Some(session_digest(secret)),
        pool: "builders".into(),
        claim: Some(hello("a").to_shape().unwrap()),
        labels: map(&[("rack", "r9")]),
        phase: js_agent_wire::Phase::Serving,
        agent_process: Some("proc-1".into()),
        agent_version: Some("2.4.6".into()),
        silent_for: Duration::ZERO,
        binding: Some(Binding::new(cred("t"), None)),
    }
}

fn hello_from(process: Option<&str>) -> js_agent_wire::Hello {
    let mut hello = hello("a");
    hello.agent_process = process.map(str::to_owned);
    hello
}

/// Catches a takeover that refuses the process that already holds the host (it lost its
/// session and says hello again), and one that lets any other process, or one that sent no
/// id, replace a live session.
#[test]
fn the_same_agent_process_takes_over_its_live_host_and_no_other_does() {
    let fx = fixture(2);
    let first = fx.hello(&hello_from(Some("p1")), &cred("t")).unwrap();
    assert_eq!(first.epoch, 1);
    let other = fx.hello(&hello_from(Some("p2")), &cred("t"));
    assert_eq!(other.unwrap_err(), Refusal::HostLive);
    let absent = fx.hello(&hello_from(None), &cred("t"));
    assert_eq!(absent.unwrap_err(), Refusal::HostLive);
    let again = fx.hello(&hello_from(Some("p1")), &cred("t")).unwrap();
    assert_eq!(again.epoch, 2);
    let old = registry_beat(&fx, &first.session, 1);
    assert_eq!(old, Err(Refusal::StaleEpoch));
    assert!(registry_beat(&fx, &again.session, 2).is_ok());
}

/// Catches a record that forgets the process of a hello that sent none, so a later hello
/// naming one takes over a host a process of unknown identity holds.
#[test]
fn a_host_admitted_without_a_process_id_is_not_taken_over_by_a_named_process() {
    let fx = fixture(2);
    fx.hello(&hello_from(None), &cred("t")).unwrap();
    let named = fx.hello(&hello_from(Some("p1")), &cred("t"));
    assert_eq!(named.unwrap_err(), Refusal::HostLive);
}

fn registry_beat(
    fx: &Fixture,
    secret: &str,
    epoch: u64,
) -> Result<js_agent_wire::HeartbeatReply, Refusal> {
    let beat = js_agent_wire::Heartbeat {
        session: secret.into(),
        ..heartbeat("a", epoch)
    };
    fx.registry.heartbeat(&beat, &cred("t"))
}

/// Catches an adopted session that no heartbeat matches: the epoch and the secret whose
/// digest was adopted are accepted, another secret is a session mismatch and an older epoch
/// is stale; and the labels, phase and epoch of the row are what the registry reports.
#[test]
fn an_adopted_session_takes_the_heartbeat_that_carries_its_secret() {
    let fx = fixture(2);
    let a = host("a");
    assert_eq!(fx.registry.adopt(&a, node(5, "sekret")), Adopted::Installed);
    let adopted = view(&fx, "a");
    assert_eq!(adopted.epoch, 5);
    assert_eq!(adopted.state, HostState::Live);
    assert_eq!(adopted.labels, map(&[("rack", "r9")]));
    assert_eq!(
        registry_beat(&fx, "other", 5),
        Err(Refusal::SessionMismatch)
    );
    assert_eq!(registry_beat(&fx, "sekret", 4), Err(Refusal::StaleEpoch));
    assert!(registry_beat(&fx, "sekret", 5).is_ok());
}

/// Catches an adopted session listed without the agent version its row keeps (a server
/// restarted since the hello lists every host's version as unknown until the host says hello
/// again), a heartbeat that drops it, and a version invented for a row that keeps none: the
/// row's version is listed after the adoption and after a heartbeat, and a later session whose
/// row keeps none lists none.
#[test]
fn an_adopted_session_lists_the_agent_version_its_row_keeps() {
    let fx = fixture(2);
    assert_eq!(
        fx.registry.adopt(&host("a"), node(5, "sekret")),
        Adopted::Installed
    );
    assert_eq!(view(&fx, "a").agent_version.as_deref(), Some("2.4.6"));
    assert!(registry_beat(&fx, "sekret", 5).is_ok());
    assert_eq!(view(&fx, "a").agent_version.as_deref(), Some("2.4.6"));

    let unversioned = NodeSession {
        agent_version: None,
        ..node(6, "later")
    };
    assert_eq!(
        fx.registry.adopt(&host("a"), unversioned),
        Adopted::Installed
    );
    assert_eq!(view(&fx, "a").agent_version, None);
}

/// Catches an adopted host whose last-heard ignores the row: a row silent past the grace
/// reads lost, one heard just now reads live, and a silence longer than any clock reading
/// saturates at lost rather than wrapping to live.
#[test]
fn an_adopted_host_counts_as_heard_when_the_row_was() {
    let fx = fixture(3);
    let late = Duration::from_millis(DEFAULT_GRACE_MS + 1);
    let edge = Duration::from_millis(DEFAULT_GRACE_MS);
    for (name, silent_for, state) in [
        ("a", late, HostState::Lost),
        ("b", edge, HostState::Live),
        ("c", Duration::MAX, HostState::Lost),
    ] {
        let session = NodeSession {
            silent_for,
            ..node(1, "s")
        };
        assert_eq!(fx.registry.adopt(&host(name), session), Adopted::Installed);
        assert_eq!(view(&fx, name).state, state, "silent {silent_for:?}");
    }
}

/// Catches an adoption that drops what the registry knows of a host that no row holds: its
/// taint, cordon, mark and binding survive a newer session, and the row's binding is not
/// put over it.
#[test]
fn adopting_a_newer_session_keeps_taint_cordon_mark_and_binding() {
    let fx = fixture(2);
    fx.join("a", "t");
    let a = host("a");
    let taint = Taint {
        reason: "disk".into(),
        run_id: None,
        check: None,
    };
    assert!(fx.registry.taint_host(&a, taint.clone()));
    let cordon = Cordon {
        until: Duration::from_secs(600),
        holder: None,
    };
    fx.registry.set_cordon(&a, Some(&cordon)).unwrap();
    fx.registry.drain(&a).unwrap();
    let mut newer = node(2, "sekret");
    newer.binding = Some(Binding::new(cred("other"), None));
    assert_eq!(fx.registry.adopt(&a, newer), Adopted::Installed);
    let after = view(&fx, "a");
    assert_eq!(after.epoch, 2);
    assert_eq!(after.taint, Some(taint));
    assert_eq!(after.mark, Some(HostMark::Draining));
    assert!(after.cordon_until.is_some());
    let bindings = fx.registry.bindings();
    assert_eq!(bindings[&a].credential(), &cred("t"));
    assert!(registry_beat(&fx, "sekret", 2).is_ok());
}

/// Catches a cache that moves back: a row of the same or an older epoch changes nothing, and
/// the session the registry holds keeps taking its beats.
#[test]
fn adopting_the_same_or_an_older_epoch_changes_nothing() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.pass_grace();
    let current = fx.join("a", "t");
    assert_eq!(current, 2);
    let before = fx.registry.hosts();
    let a = host("a");
    assert_eq!(fx.registry.adopt(&a, node(2, "x")), Adopted::Kept);
    assert_eq!(fx.registry.adopt(&a, node(1, "x")), Adopted::Kept);
    assert_eq!(fx.registry.hosts(), before);
    fx.beat("a", 2);
}

/// Catches an adoption that overwrites a host put back after a restart with nothing, or
/// refuses it: the awaiting host takes the row's session, keeps the binding it was put back
/// with, and a second adoption of that epoch is kept.
#[test]
fn adopting_over_an_awaiting_host_installs_and_keeps_its_binding() {
    let fx = fixture(2);
    let a = host("a");
    let taint = Taint {
        reason: "disk".into(),
        run_id: None,
        check: None,
    };
    let bound = Binding::new(cred("kept"), None).with_taint(Some(taint.clone()));
    fx.registry.restore_tainted(&a, bound);
    assert_eq!(view(&fx, "a").state, HostState::Awaiting);
    assert_eq!(fx.registry.adopt(&a, node(1, "s")), Adopted::Installed);
    assert_eq!(view(&fx, "a").state, HostState::Live);
    assert_eq!(view(&fx, "a").taint, Some(taint));
    assert_eq!(fx.registry.bindings()[&a].credential(), &cred("kept"));
    assert_eq!(fx.registry.adopt(&a, node(1, "s")), Adopted::Kept);
}

/// Catches an adoption of a pool the registry does not serve that installs a record anyway.
#[test]
fn adopting_a_session_of_an_unknown_pool_changes_nothing() {
    let fx = fixture(2);
    let session = NodeSession {
        pool: "elsewhere".into(),
        ..node(1, "s")
    };
    assert_eq!(fx.registry.adopt(&host("a"), session), Adopted::UnknownPool);
    assert!(fx.registry.hosts().is_empty());
}

/// Catches an adoption that no admission check may stop but that also skips the pool's
/// limit by accident: a pool at its machine limit still takes the session another server
/// admitted.
#[test]
fn adopting_runs_no_admission_check() {
    let fx = fixture(1);
    fx.join("b", "t");
    assert_eq!(
        fx.registry.adopt(&host("a"), node(1, "s")),
        Adopted::Installed
    );
    assert_eq!(fx.registry.hosts().len(), 2);
}

/// Catches an adoption that fills the registry but leaves the placer unaware of the host.
#[test]
fn adopting_lists_the_host_for_the_placer() {
    #[derive(Default)]
    struct Told(Mutex<Vec<String>>);
    impl Placer for Told {
        fn host_up(&self, host: &HostId, _shape: &Shape) {
            self.0.lock().unwrap().push(host.as_str().to_owned());
        }
    }
    let told = Arc::new(Told::default());
    let fx = with_placer(config(2), told.clone());
    fx.registry.adopt(&host("a"), node(1, "s"));
    assert_eq!(*told.0.lock().unwrap(), vec!["a".to_owned()]);
}

/// Catches an adopted host that escapes a release the scaler had under way before the
/// adoption: it is marked releasing as an admitted hello is.
#[test]
fn an_adopted_host_marked_ending_is_releasing() {
    let fx = fixture(2);
    fx.registry.mark_ending(&host("a"));
    fx.registry.adopt(&host("a"), node(1, "s"));
    assert_eq!(view(&fx, "a").mark, Some(HostMark::Releasing));
}

/// Catches a renumber that changes a record of another epoch, or none: the record at `from`
/// takes `to` and its beats then carry `to`.
#[test]
fn renumber_moves_the_record_at_from_and_nothing_else() {
    let fx = fixture(2);
    let a = host("a");
    assert_eq!(fx.join("a", "t"), 1);
    assert!(!fx.registry.renumber(&a, 4, 9));
    assert!(!fx.registry.renumber(&host("none"), 1, 9));
    assert_eq!(view(&fx, "a").epoch, 1);
    assert!(fx.registry.renumber(&a, 1, 7));
    assert_eq!(view(&fx, "a").epoch, 7);
    fx.beat("a", 7);
}

/// Catches an abandon that leaves the session usable: the beat is refused as an unknown
/// host, a hello is not host-live, and retention counts from the abandon, so the unheard
/// host is forgotten after the grace and retention and not before.
#[test]
fn abandon_ends_the_session_as_if_no_hello_since_a_restart() {
    let fx = fixture(2);
    let a = host("a");
    let welcome = fx.hello(&hello("a"), &cred("t")).unwrap();
    assert!(!fx.registry.abandon(&a, 2));
    assert!(!fx.registry.abandon(&host("none"), 1));
    fx.beat("a", 1);
    assert!(fx.registry.abandon(&a, 1));
    assert!(!fx.registry.abandon(&a, 1), "the session is already ended");
    assert_eq!(
        registry_beat(&fx, &welcome.session, 1),
        Err(Refusal::UnknownHost)
    );
    assert_eq!(view(&fx, "a").state, HostState::Awaiting);
    let kept = DEFAULT_GRACE_MS + DEFAULT_LOST_RETENTION_MS;
    fx.clock.advance(Duration::from_millis(kept));
    assert_eq!(fx.registry.hosts().len(), 1);
    fx.clock.advance(Duration::from_millis(1));
    assert!(fx.registry.hosts().is_empty());
    assert_eq!(fx.join("a", "t"), 1);
}

/// Catches an abandon that makes the next hello wait out the grace, or drops what the
/// registry keeps of the host: it is admitted at once, one epoch on, with the taint and
/// binding kept.
#[test]
fn abandon_keeps_the_taint_and_binding_and_the_next_hello_is_admitted() {
    let fx = fixture(2);
    let a = host("a");
    fx.join("a", "t");
    let taint = Taint {
        reason: "disk".into(),
        run_id: None,
        check: None,
    };
    assert!(fx.registry.taint_host(&a, taint.clone()));
    assert!(fx.registry.abandon(&a, 1));
    assert_eq!(view(&fx, "a").taint, Some(taint));
    assert_eq!(fx.registry.bindings()[&a].credential(), &cred("t"));
    assert_eq!(fx.join("a", "t"), 2);
}

/// Catches a tick that does not run the registry's housekeeping: an adopted host silent past
/// the grace and the retention is forgotten by `advance`, with no other call made.
#[test]
fn advance_forgets_an_adopted_host_silent_past_the_retention() {
    let fx = fixture(2);
    // The manual clock starts near zero, so move it past the silence the row reports.
    fx.clock
        .advance(Duration::from_millis(DEFAULT_LOST_RETENTION_MS * 2));
    let session = NodeSession {
        silent_for: Duration::from_millis(DEFAULT_GRACE_MS + DEFAULT_LOST_RETENTION_MS + 1),
        ..node(1, "s")
    };
    assert_eq!(fx.registry.adopt(&host("a"), session), Adopted::Installed);
    fx.registry.advance();
    assert!(fx.registry.hosts().is_empty());
}

/// Catches a window that differs from the one the registry forgets a silent host by (a server
/// that deletes a binding another server still serves, or keeps one past its retention): the
/// window of the pool named is its grace and lost retention, and a pool the registry does not
/// have has none.
#[test]
fn a_pool_keeps_a_silent_host_for_its_grace_and_retention() {
    let fx = fixture(2);
    assert_eq!(
        fx.registry.kept_for("builders"),
        Some(Duration::from_millis(
            DEFAULT_GRACE_MS + DEFAULT_LOST_RETENTION_MS
        ))
    );
    assert_eq!(fx.registry.kept_for("elsewhere"), None);
}

/// Asserts that a session adopted from another server's row starts its beat numbering over:
/// after beat 5 of the first session, the adopted newer session's beat 1 ends the taint the
/// record kept. It catches an adopted record that carries the old session's count and holds
/// the new session's first beats late.
#[test]
fn an_adopted_session_starts_the_beat_numbering_over() {
    let fx = fixture(2);
    let a = host("a");
    assert_eq!(fx.registry.adopt(&a, node(1, "s1")), Adopted::Installed);
    let beat = |secret: &str, epoch: u64, seq: u64, tainted: bool| js_agent_wire::Heartbeat {
        seq: Some(seq),
        tainted: Some(tainted),
        session: secret.into(),
        ..heartbeat("a", epoch)
    };
    fx.registry
        .heartbeat(&beat("s1", 1, 5, true), &cred("t"))
        .unwrap();
    assert!(view(&fx, "a").taint.is_some());

    assert_eq!(fx.registry.adopt(&a, node(2, "s2")), Adopted::Installed);
    assert!(view(&fx, "a").taint.is_some());
    fx.registry
        .heartbeat(&beat("s2", 2, 1, false), &cred("t"))
        .unwrap();
    assert_eq!(view(&fx, "a").taint, None);
}
