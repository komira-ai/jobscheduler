//! `Registry::taint_host`: the taint a caller puts on a host the registry holds, which the
//! first taint of a host outranks like any other.

mod support;

use js_agent_wire::Taint;
use js_registry::HostView;
use support::*;

fn taint(reason: &str, run: Option<&str>) -> Taint {
    Taint {
        reason: reason.into(),
        run_id: run.map(str::to_owned),
        check: None,
    }
}

fn view(fx: &Fixture, name: &str) -> HostView {
    let id = host(name);
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|view| view.id == id).unwrap()
}

/// Asserts that a call taints a held host once: it reports that it set the taint, the
/// host's view and the bindings a caller persists carry it, and a second call with another
/// reason reports `false` and leaves the first taint in both. It catches a call that
/// overwrites the first taint, and one that reports a taint it did not set.
#[test]
fn a_taint_is_set_once_and_the_first_reason_stands() {
    let fx = fixture(2);
    fx.join("a", "t");
    let first = taint("host-job", Some("job-1"));
    assert!(fx.registry.taint_host(&host("a"), first.clone()));
    assert_eq!(view(&fx, "a").taint, Some(first.clone()));
    let bound = fx.registry.bindings();
    assert_eq!(bound[&host("a")].taint(), Some(&first));

    assert!(!fx.registry.taint_host(&host("a"), taint("residue", None)));
    assert_eq!(view(&fx, "a").taint, Some(first.clone()));
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), Some(&first));
}

/// Asserts that a host the registry does not hold is not made up: the call reports `false`
/// and the registry still lists no host. It catches a call that inserts a record.
#[test]
fn an_unknown_host_is_not_tainted() {
    let fx = fixture(2);
    assert!(
        !fx.registry
            .taint_host(&host("a"), taint("host-job", Some("job-1")))
    );
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts that a heartbeat reporting another taint after the call keeps the first one in
/// the host's view and the bindings. It catches a heartbeat that replaces a taint set by
/// the server.
#[test]
fn a_heartbeat_taint_does_not_replace_a_set_taint() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let first = taint("host-job", Some("job-1"));
    assert!(fx.registry.taint_host(&host("a"), first.clone()));
    let mut beat = fx.heartbeat("a", epoch);
    beat.taint = Some(taint("residue", None));
    fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(view(&fx, "a").taint, Some(first.clone()));
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), Some(&first));
}

/// Asserts that `clear_taint` ends a held taint whoever set it: it returns the taint it held,
/// the host's view and the bindings a caller persists carry none, a second call returns
/// `None`, and a taint set afterwards stands as the first of a clean host. It catches a clear
/// that leaves the taint in the view or the bindings, a repeat that reports a clear it did
/// not make, and a clear that leaves the host unable to be tainted again.
#[test]
fn a_cleared_taint_is_returned_and_gone_from_the_view_and_the_bindings() {
    let fx = fixture(2);
    fx.join("a", "t");
    let first = taint("host-job", Some("job-1"));
    assert!(fx.registry.taint_host(&host("a"), first.clone()));

    assert_eq!(fx.registry.clear_taint(&host("a")), Some(first));
    assert_eq!(view(&fx, "a").taint, None);
    assert_eq!(fx.registry.bindings()[&host("a")].taint(), None);
    assert_eq!(fx.registry.clear_taint(&host("a")), None);

    let second = taint("residue", None);
    assert!(fx.registry.taint_host(&host("a"), second.clone()));
    assert_eq!(view(&fx, "a").taint, Some(second));
}

/// Asserts that `clear_taint` of a host the registry does not hold returns `None` and makes
/// no host up. It catches a clear that inserts a record.
#[test]
fn clearing_an_unknown_host_changes_nothing() {
    let fx = fixture(2);
    assert_eq!(fx.registry.clear_taint(&host("a")), None);
    assert!(fx.registry.hosts().is_empty());
}
