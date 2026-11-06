//! A request set aside for the machine launched for it: the step names the request each new
//! launch serves, and a request reserved for a machine on its way, or for the host it
//! became, holds that machine or host against every other request and against the idle
//! drain.

mod common;

use std::collections::BTreeMap;

use common::*;
use js_scaler::{Action, LaunchMode, Settings};
use js_types::RequestId;

fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

/// Asserts that a step that launches for two requests names each machine with the request it
/// was launched for, in queue order; that the next step, which launches nothing, names none;
/// and that a pool in `Plan` mode, which launches nothing, names none either. It catches a
/// launch credited to the wrong request, which would set the wrong job aside for a machine,
/// and a name kept from one step to the next.
#[test]
fn each_launch_is_named_with_the_request_it_serves() {
    let mut limits = settings("gpu");
    limits.max_step = 2;
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1"), request("r2")];
    let plans = acquires(&rig.tick(at(0)));
    assert_eq!(plans.len(), 2);
    let named: BTreeMap<String, RequestId> = [
        (plans[0].name.clone(), id("r1")),
        (plans[1].name.clone(), id("r2")),
    ]
    .into_iter()
    .collect();
    assert_eq!(rig.state.launched, named);

    assert!(acquires(&rig.tick(at(5 * SECOND))).is_empty());
    assert!(rig.state.launched.is_empty());

    let mut plan = Rig::new(Settings {
        launch: LaunchMode::Plan,
        ..settings("gpu")
    });
    plan.input.waiting = vec![request("r1")];
    plan.tick(at(0));
    assert_eq!(plan.state.plan.len(), 1);
    assert!(plan.state.launched.is_empty());
}

/// Asserts that a request reserved for the machine on its way that was launched for it
/// leaves that machine to itself: a second request that the machine would fit gets a
/// machine of its own in the next step, and the reserved request gets none. Without the
/// reservation the same second request is covered by the machine on its way and gets
/// nothing. It catches a reserved machine still counted for another request, which leaves
/// that request without a machine until the first one boots.
#[test]
fn a_machine_on_its_way_reserved_for_a_request_covers_no_other() {
    let (mut rig, plan, _) = booting(settings("gpu"), "i-1");
    rig.input.waiting = vec![reserved("r1", &plan.name), request("r2")];
    let launched = acquires(&rig.tick(at(2 * SECOND)));
    assert_eq!(launched.len(), 1);
    assert_eq!(rig.state.launched[&launched[0].name], id("r2"));

    let (mut free, _, _) = booting(settings("gpu"), "i-1");
    free.input.waiting = vec![request("r2")];
    assert!(acquires(&free.tick(at(2 * SECOND))).is_empty());
}

/// Asserts that a registered, quiet host reserved for a request is not drained however long
/// it stays quiet, and covers no other request, which gets a machine of its own; and that
/// the same host, once the reservation is gone, is drained when it has been quiet for `idle`.
/// It catches a host set aside for a queued job drained before the job is placed on it, and
/// a reserved host handed to another request.
#[test]
fn a_host_reserved_for_a_request_is_not_drained_and_covers_no_other() {
    let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.waiting = vec![reserved("r1", &plan.name)];
    let actions = rig.tick(at(HOUR));
    assert_eq!(count(&actions, "Drain"), 0, "{actions:?}");

    rig.input.waiting = vec![reserved("r1", &plan.name), request("r2")];
    let launched = acquires(&rig.tick(at(HOUR + SECOND)));
    assert_eq!(launched.len(), 1);
    assert_eq!(rig.state.launched[&launched[0].name], id("r2"));

    rig.input.waiting = Vec::new();
    rig.tick(at(HOUR + 2 * SECOND));
    let actions = rig.tick(at(HOUR + 11 * MINUTE));
    let drained = Action::Drain(plan.name.clone());
    assert!(actions.contains(&drained), "{actions:?}");
}
