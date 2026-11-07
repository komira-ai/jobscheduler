//! What the registry answers a placement engine that fails the prepare of a whole-host job:
//! the exclusion the table of reasons gives the host, which is the one a failed whole-node lease
//! gets, and the taint a `host-unsafe` refusal puts on the host.

mod support;

use js_agent_wire::{Heartbeat, RefusalReason, Taint};
use js_lease::Timestamp;
use js_registry::{Exclusion, prepare_exclusion};
use support::*;

/// The taint the registry holds for host `name`.
fn taint_of(fx: &Fixture, name: &str) -> Option<Taint> {
    fx.registry.own_host(&host(name)).unwrap().host.taint
}

/// Asserts the table row by row: a reason that leaves the host out for a while gives a
/// back-off ending at the time asked for, `unsupported` leaves it out for good, `host-unsafe`
/// gives no exclusion (it taints the host), and a deadline that passed with no report acts as
/// `setup-failed`. It catches a row copied wrongly, a timeout that is not a back-off, and the
/// back-off end dropped.
#[test]
fn every_reason_gives_the_exclusion_of_its_row() {
    let end = Timestamp::from_millis(5_000);
    let rows = [
        (Some(RefusalReason::NotQuiet), Some(Exclusion::Until(end))),
        (
            Some(RefusalReason::NoiseOverBound),
            Some(Exclusion::Until(end)),
        ),
        (
            Some(RefusalReason::SetupFailed),
            Some(Exclusion::Until(end)),
        ),
        (Some(RefusalReason::HostUnsafe), None),
        (Some(RefusalReason::Unsupported), Some(Exclusion::ForLife)),
        (None, Some(Exclusion::Until(end))),
    ];
    for (refused, want) in rows {
        assert_eq!(prepare_exclusion(refused, end), want, "{refused:?}");
    }
    let later = Timestamp::from_millis(9_000);
    assert_eq!(
        prepare_exclusion(None, later),
        Some(Exclusion::Until(later)),
        "the back-off ends when it is asked to"
    );
}

/// Asserts that a known host with no taint is tainted `host-unsafe` as a failed whole-node
/// lease taints it, that this is reported once, that the first taint is kept, and that the
/// binding a caller persists carries it. It catches a taint that is not recorded, one reported
/// on every call, and a second call that replaces the first.
#[test]
fn a_known_clean_host_is_tainted_host_unsafe_once() {
    let fx = fixture(2);
    fx.join("a", "t");
    let a = host("a");
    assert!(taint_of(&fx, "a").is_none());

    assert!(fx.registry.taint_after_prepare(&a));
    let taint = taint_of(&fx, "a").unwrap();
    assert_eq!(taint.reason, "host-unsafe");
    assert_eq!((taint.run_id, taint.check), (None, None));
    let bound = fx.registry.bindings();
    assert_eq!(
        bound[&a].taint().map(|taint| taint.reason.as_str()),
        Some("host-unsafe")
    );

    assert!(!fx.registry.taint_after_prepare(&a));
    assert_eq!(taint_of(&fx, "a").unwrap().reason, "host-unsafe");
}

/// Asserts that a host that reported its own taint keeps it and the call reports no change.
/// It catches a taint written over the one the host reported.
#[test]
fn a_host_with_a_taint_of_its_own_keeps_it() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let report = Heartbeat {
        taint: Some(Taint {
            reason: "residue".to_owned(),
            run_id: None,
            check: None,
        }),
        ..fx.heartbeat("a", epoch)
    };
    fx.registry.heartbeat(&report, &cred("t")).unwrap();

    assert!(!fx.registry.taint_after_prepare(&host("a")));
    assert_eq!(taint_of(&fx, "a").unwrap().reason, "residue");
}

/// Asserts that a host the registry does not know is not made known, and that no known host is
/// tainted by it. It catches a taint recorded under an id the registry never saw.
#[test]
fn an_unknown_host_is_left_alone() {
    let fx = fixture(2);
    fx.join("a", "t");
    let hosts = fx.registry.hosts();

    assert!(!fx.registry.taint_after_prepare(&host("nobody")));
    assert_eq!(fx.registry.hosts(), hosts);
    assert!(fx.registry.own_host(&host("nobody")).is_none());
    assert!(taint_of(&fx, "a").is_none());
}
