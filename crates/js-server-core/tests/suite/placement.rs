//! What the callers of work that waits are shown, what an operator's cordon, a drain, a
//! closed intake and a fence do to the feed, and the refusal of work no worker can run.

use std::time::Duration;

use crate::common::{
    Call, GIB, Rig, at, beat, block_on, calls, core, core_by, lease, request, started, submit,
    submit_request, up, worker,
};
use js_sched::{Cordon, LoadConfig, PlacementLoad};
use js_server_core::{
    Admitted, Config, Core, NodeAction, NotQueued, PlaceError, Stage, UnknownNode,
};
use js_types::sched::{OperationId, Resources};

fn shown(core: &Core<Rig>) -> Vec<Stage> {
    calls(core)
        .into_iter()
        .filter_map(|call| match call {
            Call::Show(_, stage) => Some(stage),
            _ => None,
        })
        .collect()
}

fn waiting(stage: &Stage) -> bool {
    matches!(stage, Stage::Waiting(_))
}

/// Catches: callers of work no worker can run left to think it is queued, and callers
/// told it still waits once a worker appears.
#[test]
fn work_no_worker_can_run_waits_with_a_reason_until_one_appears() {
    let core = core();
    submit(&core, 0, 1);
    assert!(matches!(shown(&core)[..], [Stage::Waiting(_)]));
    up(&core, 1, "node-a", "d1");
    assert_eq!(shown(&core)[1..], [Stage::Queued]);
}

/// Catches: an operation that started before, whose lease was given up, shown as one that
/// never started: a drain would end the streams of callers whose run may be going on.
#[test]
fn work_that_started_and_lost_its_worker_waits_kept() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    let silent = u64::try_from(js_sched::fence::LEASE_GRACE.as_millis()).expect("small") + 2;
    block_on(core.tick(at(silent)));
    assert!(matches!(shown(&core)[..], [Stage::WaitingKept(_)]));
}

/// Catches: work held by the drain's cordon on one node and an operator's on another shown
/// as queued for the other frontends: the operator's cordon keeps it waiting, whichever
/// node it is on.
#[test]
fn work_held_by_a_drain_and_an_operators_cordon_together_is_waiting() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    up(&core, 0, "node-b", "d2");
    core.place(at(1), &worker("node-a"), NodeAction::Cordon)
        .expect("placed");
    core.close_intake();
    submit(&core, 2, 1);
    assert!(waiting(&shown(&core)[0]), "{:?}", shown(&core));
}

/// Catches: work held only by the cordon an operator placed shown as queued (it waits for
/// the operator), and work held only by the drain's own cordon shown as waiting (the other
/// frontends take it).
#[test]
fn work_held_by_a_cordon_is_queued_for_a_drain_and_waiting_for_an_operator() {
    let drained = core();
    up(&drained, 0, "node-a", "d1");
    drained.close_intake();
    submit(&drained, 1, 1);
    assert_eq!(shown(&drained), [Stage::Queued]);

    let cordoned = core();
    up(&cordoned, 0, "node-a", "d1");
    cordoned
        .place(at(1), &worker("node-a"), NodeAction::Cordon)
        .expect("placed");
    submit(&cordoned, 2, 1);
    assert!(waiting(&shown(&cordoned)[0]));
}

/// Catches: an uncordon of a core whose intake is closed placing work on a node the drain
/// told it not to use, and an uncordon of an open core not placing the work at once; a node
/// that registers after the intake closed being offered work before it is cordoned.
#[test]
fn a_closed_intake_places_nothing_and_an_open_one_places_at_an_uncordon() {
    let closed = core();
    up(&closed, 0, "node-a", "d1");
    closed.close_intake();
    submit(&closed, 1, 1);
    closed
        .place(at(2), &worker("node-a"), NodeAction::Uncordon)
        .expect("placed");
    up(&closed, 3, "node-b", "d2");
    assert_eq!(started(&closed), []);

    let open = core();
    up(&open, 0, "node-a", "d1");
    let node_a = worker("node-a");
    open.place(at(1), &node_a, NodeAction::Cordon)
        .expect("placed");
    submit(&open, 2, 1);
    assert_eq!(started(&open), []);
    open.place(at(3), &node_a, NodeAction::Uncordon)
        .expect("placed");
    assert_eq!(started(&open), [lease(1, 0)]);
}

/// Catches: an operator action on a node that never registered applied, and a drain that
/// pauses at its deadline without leaving the leases it holds running.
#[test]
fn an_action_names_a_registered_node_and_a_drain_pauses_at_its_deadline() {
    let core = core();
    assert_eq!(
        core.place(at(0), &worker("node-x"), NodeAction::Cordon),
        Err(PlaceError::Unknown(UnknownNode("node-x".to_owned())))
    );
    let stream = up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    core.place(
        at(2),
        &worker("node-a"),
        NodeAction::Drain(Duration::from_secs(10)),
    )
    .expect("placed");
    // The drain's deadline passes with the lease still running: it pauses, and says so once.
    beat(&core, 11_000, ("node-a", stream), 1, &[lease(1, 0)]);
    block_on(core.tick(at(12_100)));
    block_on(core.tick(at(12_200)));
    let cordon = core.with(|view, _| view.sched().cordon(&worker("node-a")).cloned());
    assert!(matches!(cordon, Some(Cordon::Paused { .. })), "{cordon:?}");
    assert_eq!(started(&core), [lease(1, 0)], "the lease runs on");
}

/// Catches: a fenced core queueing a request or feeding the scheduler, a heartbeat on it
/// failing, and a resumed core staying out of service.
#[test]
fn a_fenced_core_takes_nothing_until_it_is_resumed() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    core.fence();
    let refused: Result<Admitted, NotQueued> = submit_request(&core, 2, request(2));
    assert_eq!(refused, Err(NotQueued::Fenced));
    assert_eq!(
        refused.unwrap_err().to_string(),
        "this frontend is handing its work to its successor"
    );
    assert!(beat(&core, 3, ("node-a", stream), 1, &[lease(1, 0)]));
    // A silent lease is not given up by a core that feeds the scheduler nothing.
    let silent = u64::try_from(js_sched::fence::LEASE_GRACE.as_millis()).expect("small") + 10;
    block_on(core.tick(at(silent)));
    assert!(
        calls(&core)
            .iter()
            .all(|call| !matches!(call, Call::Requeued(_)))
    );

    core.resume();
    assert_eq!(submit(&core, 6, 2).operation, OperationId(1));
}

/// Catches: work no worker is big enough for kept for ever, and a refusal that does not
/// reach the callers.
#[test]
fn work_no_worker_is_big_enough_for_is_refused_after_the_wait() {
    let config = Config::new(1).with_unservable_wait(Duration::from_secs(1));
    let core = core_by(&config);
    up(&core, 0, "node-a", "d1");
    let mut huge = request(1);
    huge.resources = Resources::new(1_000_000, GIB);
    let admitted = submit_request(&core, 1, huge).expect("queued");
    block_on(core.tick(at(5_000)));
    assert!(calls(&core).contains(&Call::Refuse(admitted.operation)));
    assert_eq!(core.queued_operations(), 0);
}

/// Catches: a finished operation kept past the finished retention, with its callers; the
/// adapter is told to forget them.
#[test]
fn the_callers_of_a_finished_operation_are_forgotten_after_the_retention() {
    let config = Config::new(1)
        .with_unservable_wait(Duration::from_secs(1))
        .with_finished_retention(Duration::from_secs(2));
    let core = core_by(&config);
    up(&core, 0, "node-a", "d1");
    let mut huge = request(1);
    huge.resources = Resources::new(1_000_000, GIB);
    let admitted = submit_request(&core, 1, huge).expect("queued");
    block_on(core.tick(at(5_000)));
    block_on(core.tick(at(9_000)));
    assert!(calls(&core).contains(&Call::Forgot(admitted.operation)));
}

/// The workers the first two one-core actions start on, submitted a round apart, with two
/// idle nodes up.
fn first_two_starts(config: &Config) -> Vec<String> {
    let core = core_by(config);
    up(&core, 0, "node-a", "d1");
    up(&core, 0, "node-b", "d2");
    submit(&core, 1, 1);
    block_on(core.tick(at(1_000)));
    submit(&core, 2, 2);
    block_on(core.tick(at(2_000)));
    calls(&core)
        .into_iter()
        .filter_map(|call| match call {
            Call::Start { worker, .. } => Some(worker.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

/// Catches: a core that does not place by load by default, and a `Config` whose load
/// setting never reaches the scheduler: by load the second action goes to the fuller node
/// (fill before open), by the booked rule (the off switch) it spreads to the empty one.
#[test]
fn a_core_places_by_load_unless_the_off_switch_is_set() {
    assert_eq!(
        first_two_starts(&Config::new(1)),
        ["node-a", "node-a"],
        "by load"
    );
    let off = LoadConfig {
        mode: PlacementLoad::Off,
        ..LoadConfig::default()
    };
    assert_eq!(
        first_two_starts(&Config::new(1).with_load_config(off)),
        ["node-a", "node-b"],
        "by the booked rule"
    );
}
