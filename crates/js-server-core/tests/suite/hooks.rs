//! The adapter's clock and report hooks: when the core calls them, in what order, and with
//! what the adapter needs to work after a result is taken.

use crate::common::{
    Gate, Hook, Trace, at, beat, block_on, core, gated, hooks, lease, submit, up, worker,
};
use js_server_core::{Attempt, Reason, Report, Standing, Taken};
use js_types::sched::{Outcome, ResultRef};

fn done() -> Outcome {
    Outcome::Completed {
        action_result: ResultRef::new([7; 32], 1),
    }
}

/// The hooks after the first `seen`.
fn since(core: &js_server_core::Core<crate::common::Rig>, seen: usize) -> Vec<Hook> {
    hooks(core).split_off(seen)
}

/// Catches: a feed that reaches the scheduler before the adapter is told the time, for
/// each kind of input that feeds it: a tick, a heartbeat, a report. The first hook of a
/// registration, tick and heartbeat is the clock at that call's time, and a report's feed
/// tells it exactly once.
#[test]
fn the_clock_is_told_at_the_top_of_every_feed() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    assert_eq!(
        hooks(&core).first(),
        Some(&Hook::Clock(at(0))),
        "registering"
    );
    let admitted = submit(&core, 10, 1);

    let seen = hooks(&core).len();
    block_on(core.tick(at(15)));
    assert_eq!(since(&core, seen).first(), Some(&Hook::Clock(at(15))));

    let seen = hooks(&core).len();
    beat(&core, 16, ("node-a", stream), 1, &[lease(1, 0)]);
    assert_eq!(since(&core, seen).first(), Some(&Hook::Clock(at(16))));

    let seen = hooks(&core).len();
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: done(),
    };
    block_on(core.report(at(20), &worker("node-a"), report, |_| {}));
    let clocks = since(&core, seen)
        .into_iter()
        .filter(|hook| matches!(hook, Hook::Clock(_)))
        .count();
    assert_eq!(clocks, 1, "one clock call for the one feed of a report");
}

/// Catches: a hook order the adapter cannot work with: the open operation and its waiters
/// must be seen before the feed finishes the operation, the answers and `Taken` after it
/// and before they are settled, and each exactly once for the one report.
#[test]
fn a_report_is_hooked_before_the_feed_and_after_it_and_before_the_settle() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let node = worker("node-a");
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: done(),
    };
    let seen = hooks(&core).len();
    assert_eq!(
        block_on(core.report(at(20), &node, report, |_| {})),
        Taken::Accepted
    );
    // The feed's one clock call falls between the hooks: `before_report` carries the time
    // itself.
    assert_eq!(
        since(&core, seen),
        [
            Hook::Before {
                now: at(20),
                worker: node.clone(),
                report,
                open: true,
                waiters: vec![admitted.waiter],
            },
            Hook::Clock(at(20)),
            Hook::After {
                now: at(20),
                worker: node,
                report,
                answers: vec![(admitted.operation, vec![admitted.waiter])],
                taken: Taken::Accepted,
                open: false,
            },
            Hook::Settle(lease(1, 0)),
        ]
    );
}

/// Catches: a report that is fed but not taken being hooked twice or not at all, or its
/// `Taken` lost: a report of an unknown lease, a copy of an answered one, and a result
/// whose finish waits to be written are each hooked once, with the answers it led to
/// (none) and how it was taken; and a `standing` check, which feeds nothing, calls no hook.
#[test]
fn a_report_that_is_not_answered_is_hooked_once_with_how_it_was_taken() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let node = worker("node-a");
    let report = |lease| Report {
        operation: admitted.operation,
        lease,
        outcome: done(),
    };
    let report_hooks = |seen: usize| -> Vec<Hook> {
        since(&core, seen)
            .into_iter()
            .filter(|hook| matches!(hook, Hook::Before { .. } | Hook::After { .. }))
            .collect()
    };
    let taken_as = |hook: &Hook| match hook {
        Hook::After { taken, answers, .. } => Some((*taken, answers.len())),
        _ => None,
    };

    let seen = hooks(&core).len();
    assert_eq!(
        core.standing(&node, lease(1, 9)),
        Ok(Standing::Refused(Reason::UnknownLease))
    );
    assert_eq!(
        hooks(&core).len(),
        seen,
        "standing feeds nothing and hooks nothing"
    );

    // Unknown lease: fed, refused.
    block_on(core.report(at(12), &node, report(lease(1, 9)), |_| {}));
    let hooked = report_hooks(seen);
    assert_eq!(hooked.len(), 2, "one before and one after: {hooked:?}");
    assert!(matches!(hooked[0], Hook::Before { open: true, .. }));
    assert_eq!(
        taken_as(&hooked[1]),
        Some((Taken::Refused(Reason::UnknownLease), 0))
    );

    // Finish waits to be written: taken, not answered.
    gate.finishes(Attempt::Transient);
    let seen = hooks(&core).len();
    block_on(core.report(at(20), &node, report(lease(1, 0)), |_| {}));
    let hooked = report_hooks(seen);
    assert_eq!(hooked.len(), 2);
    assert_eq!(taken_as(&hooked[1]), Some((Taken::Held, 0)));

    // Written later: settled, with no report hook of its own.
    gate.finishes(Attempt::Taken);
    let seen = hooks(&core).len();
    block_on(core.tick(at(30)));
    assert!(report_hooks(seen).is_empty(), "a tick is not a report");
    assert!(since(&core, seen).contains(&Hook::Settle(lease(1, 0))));

    // The copy of an answered result.
    let seen = hooks(&core).len();
    block_on(core.report(at(40), &node, report(lease(1, 0)), |_| {}));
    let hooked = report_hooks(seen);
    assert_eq!(hooked.len(), 2);
    assert_eq!(taken_as(&hooked[1]), Some((Taken::Dropped, 0)));
}
