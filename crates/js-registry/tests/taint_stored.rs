//! `Registry::take_stored_taint`: the taint a caller takes from the store a server shares with
//! others, after its own conditional commit lost to another server's change.

mod support;

use js_agent_wire::Taint;
use support::*;

fn taint(reason: &str, run: Option<&str>) -> Taint {
    Taint {
        reason: reason.into(),
        run_id: run.map(str::to_owned),
        check: None,
    }
}

fn held(fx: &Fixture, name: &str) -> Option<Taint> {
    let id = host(name);
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|view| view.id == id).unwrap().taint
}

/// Asserts that the stored taint replaces the one an agent made, and is taken by a host that
/// holds none: each call reports a change, the host's view and the bindings a caller persists
/// carry the stored taint, and a repeat of the same call reports no change. It catches a call
/// that keeps the registry's own taint over the store's, and one that reports a change it did
/// not make.
#[test]
fn the_stored_taint_replaces_the_held_one_and_fills_an_empty_host() {
    let fx = fixture(2);
    fx.join("a", "t");
    let a = host("a");
    let stored = taint("residue", Some("r1"));
    assert!(fx.registry.take_stored_taint(&a, Some(stored.clone())));
    assert_eq!(held(&fx, "a"), Some(stored.clone()));
    assert!(!fx.registry.take_stored_taint(&a, Some(stored.clone())));

    let newer = taint("residue", Some("r2"));
    assert!(fx.registry.take_stored_taint(&a, Some(newer.clone())));
    assert_eq!(held(&fx, "a"), Some(newer.clone()));
    assert_eq!(fx.registry.bindings()[&a].taint(), Some(&newer));
}

/// Asserts that a store that holds no taint ends the one an agent made: the call reports a
/// change, the host's view and the bindings carry none, and a repeat, and a call on a host
/// with none, report no change. It catches a call that leaves a taint the store no longer
/// holds, and one that reports an end it did not make.
#[test]
fn a_store_that_holds_none_ends_the_held_taint() {
    let fx = fixture(2);
    fx.join("a", "t");
    let a = host("a");
    assert!(!fx.registry.take_stored_taint(&a, None));
    assert!(fx.registry.taint_host(&a, taint("residue", Some("r1"))));
    assert!(fx.registry.take_stored_taint(&a, None));
    assert_eq!(held(&fx, "a"), None);
    assert_eq!(fx.registry.bindings()[&a].taint(), None);
    assert!(!fx.registry.take_stored_taint(&a, None));
}

/// Asserts that a taint the server made itself, a host job's and a refused prepare's, is
/// neither ended by a store that holds none nor replaced by one that holds another taint, and
/// that each such call reports no change. It catches a resync that drops a taint the agent's
/// own word does not end, which the store need not hold yet.
#[test]
fn a_taint_the_server_made_is_never_taken_from_the_store() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "u");
    let (a, b) = (host("a"), host("b"));
    let job = taint("host-job", Some("job-1"));
    assert!(fx.registry.taint_host(&a, job.clone()));
    assert!(!fx.registry.take_stored_taint(&a, None));
    let other = Some(taint("residue", Some("r1")));
    assert!(!fx.registry.take_stored_taint(&a, other.clone()));
    assert_eq!(held(&fx, "a"), Some(job));

    assert!(fx.registry.taint_after_prepare(&b));
    let prepared = held(&fx, "b");
    assert!(prepared.is_some());
    assert!(!fx.registry.take_stored_taint(&b, None));
    assert!(!fx.registry.take_stored_taint(&b, other));
    assert_eq!(held(&fx, "b"), prepared);
}

/// Asserts that a host the registry does not hold is not made up: the call reports no change,
/// with a taint or without, and the registry still lists no host. It catches a call that
/// inserts a record.
#[test]
fn an_unknown_host_takes_no_stored_taint() {
    let fx = fixture(2);
    let ghost = host("ghost");
    assert!(!fx.registry.take_stored_taint(&ghost, None));
    let stored = Some(taint("residue", None));
    assert!(!fx.registry.take_stored_taint(&ghost, stored));
    assert!(fx.registry.hosts().is_empty());
}
