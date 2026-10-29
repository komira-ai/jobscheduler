//! `Registry::mark_ending`: a host whose release was under way before a restart is
//! admitted already marked when it says hello again, and the shape a host announced is
//! listed beside the one the pool books. The registry is driven directly on a manual
//! clock; nothing sleeps and no test asserts how long anything takes.

mod support;

use js_agent_wire::Taint;
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Binding, Busy, HostMark, HostState, HostView};
use support::*;

fn view(fx: &Fixture, name: &str) -> HostView {
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|h| h.id == host(name)).unwrap()
}

/// Asserts the restart case. A registry that has just started holds no host; the scaler
/// marks the host `a` ending (its records say the release was under way), and only then
/// does `a` say hello again. It is admitted, listed `Releasing`, and a lease request finds
/// no capacity for it, though it is live and idle. Without the mark the same hello is
/// admitted unmarked and the lease lands, which is the window the mark closes. A host the
/// scaler did not mark, in the same registry, is admitted unmarked. It catches a mark that
/// is held only on records the registry already has.
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
    let mut provider = fx.registry.provider();
    assert_eq!(started_on(&provider.start(spec(1))), Some("b"));
    provider.stop(&lease(1));
    fx.registry.drain(&host("b")).unwrap();
    assert_eq!(
        provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
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
/// check: a host that holds a lease is listed `Releasing` and keeps the lease, `remove`
/// refuses it as `Busy::Lease` while the lease stands, and succeeds once the lease is
/// stopped. It catches a mark that cuts a lease, and a remove that drops a host with work
/// booked on it.
#[test]
fn marking_a_known_host_does_not_cut_its_lease() {
    let fx = fixture(1);
    fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    fx.registry.mark_ending(&host("a"));
    assert_eq!(view(&fx, "a").mark, Some(HostMark::Releasing));
    assert_eq!(view(&fx, "a").lease, Some(lease(1)));
    assert_eq!(fx.registry.remove(&host("a")), Err(Busy::Lease));
    provider.stop(&lease(1));
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

/// Asserts that silence does not make a booked host quiet: a lost host and an awaiting
/// one that still hold a lease are refused `drain` with `Busy::Lease` and keep no mark. It
/// catches a rule that lets silence stand for "books nothing", which would cut a lease the
/// registry still holds.
#[test]
fn a_silent_host_that_still_holds_a_lease_is_not_drained() {
    let fx = fixture(2);
    fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    fx.pass_grace();
    assert_eq!(view(&fx, "a").state, HostState::Lost);
    assert_eq!(fx.registry.drain(&host("a")), Err(Busy::Lease));
    fx.provider.seat(&host("b"), lease(2), None).unwrap();
    assert_eq!(view(&fx, "b").state, HostState::Awaiting);
    assert_eq!(fx.registry.drain(&host("b")), Err(Busy::Lease));
    assert_eq!(view(&fx, "a").mark, None);
    assert_eq!(view(&fx, "b").mark, None);
}
