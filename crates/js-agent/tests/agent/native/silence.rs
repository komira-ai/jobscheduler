//! The silence bound over the native runtime: a silence of the server stops every native
//! run through the same `Runtime::stop` as a server's stop, which ends the run's process
//! group, and the run's removal retires its account. The runtime is the real one, over the
//! real pool on a fake host and the fake launcher, so this runs on every platform. What the
//! runtime cannot do (bound a run while no agent runs) is documented in the runtime, not
//! tested.

use std::time::Duration;

use js_agent::lifecycle::{Contact, detail};
use js_agent_wire::Outcome;

use super::lifecycle::{Native, sleeper};

/// Asserts that with two native runs and no contact, a step one millisecond short of the
/// bound ends nothing, and the bound ends both: each result is `killed` with the detail
/// `silence-stop` and both accounts are retired with their homes; further steps a bound later
/// end nothing more (no run is launched or stopped again). It catches a native run the
/// silence does not reach, and one stopped more than once.
#[test]
fn a_silence_kills_every_native_run_once() {
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

    h.clock.advance(299_999);
    h.life.step();
    assert!(
        h.life.results().is_empty(),
        "nothing is stopped short of the bound"
    );
    assert_eq!(h.life.busy_slots().len(), 2);

    h.clock.advance(1);
    h.until(|life| life.results().len() == 2 && life.held().len() == 2);
    for result in h.life.results() {
        assert_eq!(result.outcome, Outcome::Killed, "{}", result.run_id);
        assert_eq!(result.detail.as_deref(), Some(detail::SILENCE_STOP));
    }
    h.until(|life| life.busy_slots().is_empty());
    assert!(
        homes.iter().all(|home| !home.exists()),
        "both accounts retired"
    );
    assert_eq!(h.launcher.launched().len(), 2, "no run is started again");

    h.clock.advance(300_000);
    h.life.step();
    assert_eq!(h.life.results().len(), 2, "stopped once for the silence");
    for result in h.life.results() {
        assert_eq!(result.detail.as_deref(), Some(detail::SILENCE_STOP));
    }
}
