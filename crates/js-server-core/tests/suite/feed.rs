//! The feed loop: a record is committed, its rows are written, and only then are its
//! effects carried out (moved from the farm core's tests of the same rule, with the
//! store's refusal played by a writer).

use std::time::Duration;

use crate::common::{
    Call, GIB, Gate, Rig, Trace, at, beat, block_on, calls, close, core, gated, gated_by, lease,
    lines, reported, request, started, submit, submit_request, up, up_after,
};
use js_sched::FARM_RERUNS;
use js_sched::fence::{LEASE_GRACE, START_GRACE};
use js_server_core::{Attempt, Config, Core};
use js_types::sched::{OperationId, Resources};

/// Catches: a `Start` sent before the writer has taken the grant it follows, or a lease
/// that is not offered to its worker before the grant commits.
#[test]
fn a_start_is_released_only_after_its_grant_is_written() {
    let trace = Trace::default();
    let (writer, _gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    submit(&core, 10, 1);
    assert_eq!(
        lines(&trace),
        ["offer 1.0", "write start 1.0 Taken", "start 1.0"]
    );
}

/// Catches: a `Start` released while the writer refuses, or lost for good once it takes
/// the grant again: the held effect is released by the next input that can deliver it.
#[test]
fn a_grant_the_writer_refuses_is_held_and_released_by_the_next_tick() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    gate.grants(Attempt::Transient);
    submit(&core, 10, 1);
    assert_eq!(started(&core), []);
    assert_eq!(lines(&trace), ["offer 1.0", "write start 1.0 Transient"]);

    gate.grants(Attempt::Taken);
    block_on(core.tick(at(1_000)));
    assert_eq!(started(&core), [lease(1, 0)]);
    assert_eq!(lines(&trace)[2..], ["write start 1.0 Taken", "start 1.0"]);
}

/// Catches: a later record written ahead of an earlier one the writer refused (a result
/// before its grant), and a writer asked again by every input while it is down.
#[test]
fn held_grants_are_written_in_order_and_not_asked_for_again_by_a_submit() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    gate.grants(Attempt::Transient);
    submit(&core, 10, 1);
    submit(&core, 11, 2);
    assert_eq!(
        gate.asked(),
        1,
        "the second submit adds a record behind the first"
    );

    gate.grants(Attempt::Taken);
    block_on(core.tick(at(1_000)));
    assert_eq!(started(&core), [lease(1, 0), lease(1, 1)]);
}

/// Catches: a core that holds committed records without bound for a writer that stays
/// down, or one that releases a record it dropped over the cap.
#[test]
fn a_record_over_the_cap_is_dropped_and_never_released() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer).with_held_cap(1);
    up(&core, 0, "node-a", "d1");
    gate.grants(Attempt::Transient);
    submit(&core, 10, 1);
    submit(&core, 11, 2);
    assert_eq!(core.dropped_writes(), 1);

    gate.grants(Attempt::Taken);
    block_on(core.tick(at(1_000)));
    assert_eq!(started(&core), [lease(1, 0)]);
}

/// Catches: a grant the writer refuses for good being retried for ever, or its `Start`
/// being sent anyway.
#[test]
fn a_grant_the_writer_refuses_for_good_is_dropped() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    gate.grants(Attempt::Dropped);
    submit(&core, 10, 1);
    assert_eq!(core.dropped_writes(), 1);
    assert_eq!(started(&core), []);
    gate.grants(Attempt::Taken);
    block_on(core.tick(at(1_000)));
    assert_eq!(started(&core), [], "a dropped record is not written later");
}

/// Catches: a held `Start` released for a lease the scheduler gave up meanwhile (it would
/// put a lease back that no one holds); the operation's next lease starts instead.
#[test]
fn a_held_start_of_a_lease_given_up_is_dropped() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    let stream = up(&core, 0, "node-a", "d1");
    gate.grants(Attempt::Transient);
    submit(&core, 10, 1);
    // The worker never lists the lease: past the start grace the scheduler gives it up.
    let grace = js_sched::fence::START_GRACE.as_millis() as u64;
    beat(&core, 10 + grace + 1, ("node-a", stream), 1, &[]);
    assert!(calls(&core).contains(&Call::Requeued(lease(1, 0))));

    gate.grants(Attempt::Taken);
    block_on(core.tick(at(10 + grace + 2)));
    assert_eq!(started(&core), [lease(1, 1)]);
}

/// Catches: a lease given up taking with it the held finish of another operation: only the
/// held `Start` of the lease goes, and the refusal is written and carried out later.
#[test]
fn a_held_refusal_stays_when_a_lease_is_given_up() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let config = Config::new(1).with_unservable_wait(Duration::from_secs(1));
    let core = gated_by(&config, &trace, writer);
    let stream = up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    // Work no worker is big enough for, which the scheduler refuses once it has waited.
    let mut huge = request(2);
    huge.resources = Resources::new(1_000_000, GIB);
    let refused = submit_request(&core, 2, huge).expect("queued");
    gate.finishes(Attempt::Transient);
    block_on(core.tick(at(5_000)));
    assert!(!calls(&core).contains(&Call::Refuse(refused.operation)));

    // The worker never lists the first lease: past the start grace it is given up.
    let grace = millis(START_GRACE);
    beat(&core, 5_000 + grace, ("node-a", stream), 1, &[]);
    assert!(calls(&core).contains(&Call::Requeued(lease(1, 0))));
    assert!(!calls(&core).contains(&Call::Refuse(refused.operation)));

    gate.finishes(Attempt::Taken);
    block_on(core.tick(at(6_000 + grace)));
    assert!(calls(&core).contains(&Call::Refuse(refused.operation)));
}

/// Catches: a silent lease lost past its operation's farm reruns not finishing it: the
/// answer comes back from the tick, once, after the reruns the budget allows.
#[test]
fn a_silent_lease_lost_past_the_farm_reruns_is_answered_by_the_tick() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 0, 1);
    let mut now = 0;
    for round in 0..=u64::from(FARM_RERUNS) {
        now += millis(LEASE_GRACE) + 1;
        block_on(core.tick(at(now)));
        if round < u64::from(FARM_RERUNS) {
            beat(&core, now, ("node-a", stream), round + 1, &[]);
        }
    }
    assert_eq!(settled(&core), [admitted.operation]);
    assert_eq!(
        started(&core).len(),
        usize::try_from(FARM_RERUNS).expect("small") + 1
    );
}

/// Catches: a lease a restarted daemon proves lost not finishing its operation past the
/// farm reruns: the answer comes back from the heartbeat of the new session.
#[test]
fn a_run_lost_in_a_proven_restart_past_the_farm_reruns_is_answered_by_the_heartbeat() {
    let core = core();
    let mut stream = up(&core, 0, "node-a", "d0");
    let admitted = submit(&core, 1, 1);
    for round in 0..=u64::from(FARM_RERUNS) {
        close(&core, "node-a", stream);
        let (new, old) = (format!("d{}", round + 1), format!("d{round}"));
        stream = up_after(&core, 10 + round, "node-a", (&new, &old), reported(8));
        beat(&core, 10 + round, ("node-a", stream), 1, &[]);
    }
    assert_eq!(settled(&core), [admitted.operation]);
}

/// The operations the rig was asked to settle, in order.
fn settled(core: &Core<Rig>) -> Vec<OperationId> {
    calls(core)
        .iter()
        .filter_map(|call| match call {
            Call::Settle { operation, .. } => Some(*operation),
            _ => None,
        })
        .collect()
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("small")
}
