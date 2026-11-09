//! `Registry::mark_ending`: a host whose release was under way before a restart is
//! admitted already marked when it says hello again, and the shape a host announced is
//! listed beside the one the pool books. The registry is driven directly on a manual
//! clock; nothing sleeps and no test asserts how long anything takes.

mod support;

use js_agent_wire::{HeldRun, HeldState, Taint};
use js_registry::{Binding, Busy, HostMark, HostState, HostView};
use support::*;

fn view(fx: &Fixture, name: &str) -> HostView {
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|h| h.id == host(name)).unwrap()
}

/// Asserts the restart case. A registry that has just started holds no host; the scaler
/// marks the host `a` ending (its records say the release was under way), and only then
/// does `a` say hello again. It is admitted and listed `Releasing`, though it is live and
/// idle. Without the mark the same hello is admitted unmarked, which is the window the mark
/// closes. A host the scaler did not mark, in the same registry, is admitted unmarked, and a
/// drain marks it as it does any quiet host. It catches a mark that is held only on records
/// the registry already has.
#[test]
fn a_host_marked_before_its_hello_is_admitted_releasing() {
    let fx = fixture(2);
    fx.registry.mark_ending(&host("a"));
    assert!(fx.registry.hosts().is_empty());
    fx.join("a", "t");
    fx.join("b", "t");
    assert_eq!(view(&fx, "a").mark, Some(HostMark::Releasing));
    assert_eq!(view(&fx, "a").state, HostState::Live);
    assert_eq!(view(&fx, "b").mark, None);
    fx.registry.drain(&host("b")).unwrap();
    assert_eq!(view(&fx, "b").mark, Some(HostMark::Draining));
}

/// Asserts that the mark survives the host going lost and saying hello again, and that
/// `remove` ends it: after the removal the same name registers unmarked, as a new machine
/// of that name could. It catches a mark that stays forever on a name, and one that a
/// second hello drops.
#[test]
fn the_mark_holds_across_hellos_until_the_host_is_removed() {
    let fx = fixture(1);
    fx.registry.mark_ending(&host("a"));
    fx.join("a", "t");
    fx.pass_grace();
    fx.join("a", "t");
    assert_eq!(view(&fx, "a").mark, Some(HostMark::Releasing));
    assert_eq!(fx.registry.remove(&host("a")), Ok(()));
    fx.join("a", "t");
    assert_eq!(view(&fx, "a").mark, None);
}

/// Asserts that `remove` of a host the registry never held still ends the mark kept for
/// it: the machine was released before its agent said hello, so the name must not stay
/// marked. It catches a remove that returns early for an unknown host and leaves the
/// pre-mark behind.
#[test]
fn removing_a_host_that_never_said_hello_ends_its_mark() {
    let fx = fixture(1);
    fx.registry.mark_ending(&host("a"));
    assert_eq!(fx.registry.remove(&host("a")), Ok(()));
    fx.join("a", "t");
    assert_eq!(view(&fx, "a").mark, None);
}

/// Asserts that marking a host the registry already holds marks it at once, with no quiet
/// check: a host that lists a run is listed `Releasing` and still lists the run, and `remove`
/// drops it, since it asks nothing of the host's runs. It catches a mark that waits for the
/// host to be quiet, and a remove that refuses a host being given back.
#[test]
fn marking_a_known_host_does_not_wait_for_its_runs() {
    let fx = fixture(1);
    fx.join("a", "t");
    let mut listing = fx.heartbeat("a", 1);
    listing.held = vec![HeldRun {
        run_id: "r1".into(),
        state: HeldState::Running,
    }];
    fx.registry.heartbeat(&listing, &cred("t")).unwrap();
    fx.registry.mark_ending(&host("a"));
    assert_eq!(view(&fx, "a").mark, Some(HostMark::Releasing));
    assert!(view(&fx, "a").releasing);
    assert_eq!(fx.registry.remove(&host("a")), Ok(()));
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts the shape a host announced is listed as its claim: after a hello it is the
/// hello's cpu, memory and gpus (not the pool's floor, which `shape` still shows), a
/// heartbeat leaves it, and a host put back after a restart that has not said hello has
/// none. It catches a claim taken from the floor, one lost at the next hello, and one
/// invented for a host that never spoke.
#[test]
fn a_host_lists_the_shape_it_announced() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    fx.beat("a", epoch);
    let a = view(&fx, "a");
    let claim = a.claim.unwrap();
    assert_eq!(claim.resources().cpu_millis(), 8000);
    assert_eq!(claim.resources().memory_bytes(), 16 << 30);
    assert_eq!(a.shape.resources().cpu_millis(), 4000);
    let taint = Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    };
    let binding = Binding::new(cred("t"), None).with_taint(Some(taint));
    fx.registry.restore_tainted(&host("b"), binding);
    let b = view(&fx, "b");
    assert_eq!(b.state, HostState::Awaiting);
    assert_eq!(b.claim, None);
}

/// Asserts that a host the registry does not hear from can be scaled away: a lost host and
/// an awaiting one (put back after a restart, never heard from since), each with nothing
/// booked on it, are marked draining, then releasing, and removed. It catches a mark that
/// is refused for silence, which would leave the scaler repeating `Drain` and
/// `ReleaseHost` for a paid machine that holds nothing.
#[test]
fn a_silent_host_that_books_nothing_is_drained_released_and_removed() {
    let fx = fixture(3);
    fx.join("a", "t");
    fx.pass_grace();
    let taint = Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    };
    let binding = Binding::new(cred("t"), None).with_taint(Some(taint));
    fx.registry.restore_tainted(&host("b"), binding);
    for name in ["a", "b"] {
        let id = host(name);
        assert_ne!(view(&fx, name).state, HostState::Live, "{name}");
        assert_eq!(fx.registry.drain(&id), Ok(()), "{name}");
        assert_eq!(view(&fx, name).mark, Some(HostMark::Draining), "{name}");
        assert_eq!(fx.registry.release_host(&id), Ok(()), "{name}");
        assert_eq!(view(&fx, name).mark, Some(HostMark::Releasing), "{name}");
        assert_eq!(fx.registry.remove(&id), Ok(()), "{name}");
    }
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts that silence does not make a host that lists a run quiet: a lost host that still
/// lists a run is refused `drain` with `Busy::Runs` and keeps no mark. It catches a rule that
/// lets silence stand for "has nothing", which would cut work the registry still sees.
#[test]
fn a_silent_host_that_still_lists_a_run_is_not_drained() {
    let fx = fixture(2);
    fx.join("a", "t");
    let mut listing = fx.heartbeat("a", 1);
    listing.held = vec![HeldRun {
        run_id: "r1".into(),
        state: HeldState::Running,
    }];
    fx.registry.heartbeat(&listing, &cred("t")).unwrap();
    fx.pass_grace();
    assert_eq!(view(&fx, "a").state, HostState::Lost);
    assert_eq!(fx.registry.drain(&host("a")), Err(Busy::Runs));
    assert_eq!(view(&fx, "a").mark, None);
}
