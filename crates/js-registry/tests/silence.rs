//! What the registry says of a host's silence: since when it has missed its grace
//! (`Registry::silent_since`), and that a host whose placer holds work on it is not forgotten
//! for being silent (`Placer::holds_work_on`). Nothing here ends anything by a clock.

mod support;

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_registry::DEFAULT_LOST_RETENTION_MS;
use js_types::{Clock, Timestamp};
use support::*;

/// Catches a silence read from the wrong instant (the last beat, the read itself), one that
/// starts at the grace rather than past it, one that moves while the host stays silent, and
/// one that does not clear when the host is heard again.
#[test]
fn silence_is_the_instant_the_grace_ran_out_and_clears_when_heard() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let a = host("a");
    let heard = fx.clock.now().millis();
    assert_eq!(fx.registry.silent_since(&a), None, "a host just heard");
    fx.clock.advance(Duration::from_millis(DEFAULT_GRACE_MS));
    assert_eq!(
        fx.registry.silent_since(&a),
        None,
        "at the grace it is heard"
    );
    fx.clock.advance(Duration::from_millis(1));
    let missed = Some(Timestamp::from_millis(heard + DEFAULT_GRACE_MS));
    assert_eq!(fx.registry.silent_since(&a), missed);
    fx.clock.advance(Duration::from_secs(3600));
    assert_eq!(
        fx.registry.silent_since(&a),
        missed,
        "silence does not move"
    );
    fx.beat("a", epoch);
    assert_eq!(fx.registry.silent_since(&a), None, "a host heard again");
    assert_eq!(fx.registry.silent_since(&host("never")), None);
}

/// Catches a host forgotten after the lost retention while its placer holds work on it (a
/// job's allocation that only the host or the provider frees), and one the registry keeps
/// once the placer holds nothing there.
#[test]
fn a_host_its_placer_holds_work_on_is_not_forgotten_for_silence() {
    let placer = Arc::new(Holding::default());
    let fx = with_placer(config(2), placer.clone());
    let epoch = fx.join("a", "t");
    let a = host("a");
    placer.hold(&a);
    let silent = DEFAULT_GRACE_MS + DEFAULT_LOST_RETENTION_MS + 1;
    fx.clock.advance(Duration::from_millis(silent));
    let listed = |fx: &Fixture| fx.registry.hosts().into_iter().any(|view| view.id == a);
    assert!(listed(&fx), "a host its placer holds work on was forgotten");
    assert!(fx.registry.silent_since(&a).is_some());
    let beat = fx.heartbeat("a", epoch);
    assert!(
        fx.registry.check_heartbeat(&beat, &cred("t")).is_ok(),
        "the check refuses a host the registry keeps"
    );
    placer.release(&a);
    assert!(
        fx.registry.check_heartbeat(&beat, &cred("t")).is_err(),
        "the check admits a host the housekeeping would forget"
    );
    assert!(
        !listed(&fx),
        "a host nothing holds stays past the retention"
    );
}
