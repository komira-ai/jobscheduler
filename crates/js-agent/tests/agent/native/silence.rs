//! The silence bound over the native runtime: a silence of the server stops no native run
//! that has begun (each keeps its process group and its account), and a native start that
//! comes while the silence lasts is refused before anything is launched. The runtime is the
//! real one, over the real pool on a fake host and the fake launcher, so this runs on every
//! platform.

use std::time::Duration;

use js_agent::lifecycle::{Contact, detail};
use js_agent_wire::Outcome;

use super::lifecycle::{Native, sleeper};

/// Asserts that with two native runs and no contact, the steps at the bound and ten bounds
/// later end neither (no result, both slots busy, both accounts' homes still there), and
/// that a third native start made past the bound is refused `failed_to_start` with the
/// detail `silence-stop` without a launch. It catches a silence that kills a native run the
/// server never gave away, and one that launches new work it could not report.
#[test]
fn a_silence_keeps_native_runs_and_refuses_a_new_one() {
    let mut h = Native::with("silence", None);
    h.life
        .stop_on_silence(Duration::from_secs(300), Contact::default());
    for run in ["r1", "r2"] {
        let mut spec = sleeper(run);
        spec.limits.wall_ms = 86_400_000;
        h.start(spec);
    }
    let homes = [h.home_of(0), h.home_of(1)];
    assert!(homes.iter().all(|home| home.is_dir()));

    for step in [300_000, 3_000_000] {
        h.clock.advance(step);
        h.life.step();
        assert!(h.life.results().is_empty(), "nothing is stopped");
        assert_eq!(h.life.busy_slots().len(), 2);
    }
    assert!(homes.iter().all(|home| home.is_dir()), "both accounts kept");

    assert_eq!(h.start(sleeper("r3")), ["refused r3 silence-stop"]);
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::FailedToStart);
    assert_eq!(results[0].detail.as_deref(), Some(detail::SILENCE_STOP));
    assert_eq!(h.launcher.launched().len(), 2, "nothing more is launched");
    assert_eq!(h.life.busy_slots().len(), 2);
}
