//! The silence bound over the native runtime: a silence of the server stops every native
//! run through the same `Runtime::stop` as a server's stop, which the helper answers with
//! `kill-uid` (no grace: the helper only kills). The runtime is the real one, over the fake
//! helper, so this runs on every platform. What the helper cannot do (bound a run while no
//! agent runs) is documented in the runtime, not tested.

use std::time::Duration;

use js_agent::lifecycle::{Contact, detail};
use js_agent_wire::Outcome;

use super::lifecycle::{Native, job};

/// Asserts that with two native runs and no contact, a step one millisecond short of the
/// bound kills nothing, and the bound ends both: one `kill-uid` for the stop and one for
/// the removal of each run's user (and its `user-delete`), each result `killed` with the
/// detail `silence-stop`; further steps a bound later kill nothing more. It catches a
/// native run the silence does not reach, and one stopped more than once.
#[test]
fn a_silence_kills_every_native_run_once() {
    let mut h = Native::with("silence", None);
    h.life
        .stop_on_silence(Duration::from_secs(300), Contact::default());
    for run in ["r1", "r2"] {
        let mut spec = job(run, "/bin/sleep");
        spec.limits.wall_ms = 86_400_000;
        h.start(spec);
    }
    let kills = |h: &Native| {
        let verbs = h.fake.verbs();
        verbs.iter().filter(|v| v.starts_with("kill-uid ")).count()
    };
    h.clock.advance(299_999);
    h.life.step();
    assert_eq!(kills(&h), 0, "{:?}", h.fake.verbs());

    h.clock.advance(1);
    h.until(|life| life.results().len() == 2 && life.held().len() == 2);
    for result in h.life.results() {
        assert_eq!(result.outcome, Outcome::Killed, "{}", result.run_id);
        assert_eq!(result.detail.as_deref(), Some(detail::SILENCE_STOP));
    }
    h.until(|life| life.busy_slots().is_empty());
    let verbs = h.fake.verbs();
    assert_eq!(kills(&h), 4, "a stop and a removal for each run: {verbs:?}");
    let deletes = verbs
        .iter()
        .filter(|v| v.starts_with("user-delete "))
        .count();
    assert_eq!(deletes, 2, "{verbs:?}");

    h.clock.advance(300_000);
    h.life.step();
    assert_eq!(kills(&h), 4, "stopped once for the silence");
}
