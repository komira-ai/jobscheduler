//! What a [`Config`] sets on the scheduler: the memory ladder, the bound on memory floors,
//! the count of lost leases that refuses an operation, and the whole-machine settings. A
//! core built from the defaults behaves as it did before they were settable (the other
//! files of the suite build one); each test here changes one setting and sees the scheduler
//! follow it.

use js_caps::NodeCaps;
use js_sched::MemoryLadder;
use js_sched::fence::LEASE_GRACE;
use js_server_core::{Config, Core, Report, Reported, WholeNodeConfig};
use js_types::sched::{Failure, LeaseId, LeaseKind, OperationId, Outcome, Resources, WorkKey};

use crate::common::{
    Call, GIB, Rig, at, beat, block_on, calls, core_by, lease, request, submit, submit_request, up,
    up_after, worker,
};

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("small")
}

fn key(n: u64) -> WorkKey {
    WorkKey::new("main", format!("action-{n}"))
}

/// Reports that `lease` of `operation` on node-a was killed for passing its memory limit.
fn kill(core: &Core<Rig>, now: u64, operation: OperationId, lease: LeaseId) {
    let report = Report {
        operation,
        lease,
        outcome: Outcome::Failed(Failure::OutOfMemory),
    };
    block_on(core.report(at(now), &worker("node-a"), report, |_| {}));
}

/// Catches: `memory_ladder` ignored: with rungs of 2 and 8 GiB an action booked at 1 GiB
/// that is killed twice for memory has its floor raised to 2 GiB, then to 8, where the
/// default ladder would raise it to 4 and then 16.
#[test]
fn a_configured_ladder_sets_the_memory_floor_each_kill_raises() {
    let ladder = MemoryLadder::from_gib(&[2, 8]).expect("a ladder");
    let core = core_by(&Config::new(1).with_memory_ladder(ladder));
    let stream = up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let floor = |core: &Core<Rig>| core.with(|view, _| view.sched().memory_floor(&key(1)));

    kill(&core, 20, admitted.operation, lease(1, 0));
    assert_eq!(
        floor(&core),
        Some(2 * GIB),
        "the first rung above its booking"
    );
    beat(&core, 30, ("node-a", stream), 1, &[]);
    kill(&core, 40, admitted.operation, lease(1, 1));
    assert_eq!(floor(&core), Some(8 * GIB), "the second rung");
}

/// Catches: `memory_floors` ignored: with a bound of one, raising the floor of a second
/// action forgets the floor raised longest ago; with the default bound both stay.
#[test]
fn a_bound_of_one_forgets_the_older_memory_floor() {
    for (config, kept_first) in [
        (Config::new(1).with_memory_floors(1), false),
        (Config::new(1), true),
    ] {
        let core = core_by(&config);
        up(&core, 0, "node-a", "d1");
        let first = submit(&core, 10, 1);
        let second = submit(&core, 11, 2);
        kill(&core, 20, first.operation, lease(1, 0));
        kill(&core, 21, second.operation, lease(1, 1));
        let floors = core.with(|view, _| {
            (
                view.sched().memory_floor(&key(1)),
                view.sched().memory_floor(&key(2)),
            )
        });
        assert_eq!(
            floors.0.is_some(),
            kept_first,
            "the older floor: {config:?}"
        );
        assert!(floors.1.is_some(), "the newer floor stays: {config:?}");
    }
}

/// Catches: `loss_requeues` ignored: with a count of two the second lost lease refuses the
/// operation (its callers are told), where the default count of three runs it again.
#[test]
fn a_loss_count_of_two_refuses_the_operation_on_the_second_lost_lease() {
    for (config, refused) in [
        (Config::new(1).with_loss_requeues(2), true),
        (Config::new(1), false),
    ] {
        let core = core_by(&config);
        let stream = up(&core, 0, "node-a", "d1");
        let admitted = submit(&core, 0, 1);
        let grace = millis(LEASE_GRACE);

        // The lease goes silent past its grace and is granted again.
        block_on(core.tick(at(grace + 1)));
        assert!(
            !calls(&core).contains(&Call::Refuse(admitted.operation)),
            "one loss runs it again: {config:?}"
        );
        beat(&core, grace + 2, ("node-a", stream), 1, &[]);

        // And again.
        block_on(core.tick(at(2 * grace + 3)));
        assert_eq!(
            calls(&core).contains(&Call::Refuse(admitted.operation)),
            refused,
            "{config:?}"
        );
    }
}

/// Catches: `whole_node` ignored: with a drain wait of five seconds a whole-machine request
/// waiting for a busy node holds it only once the wait has passed; without the setting it
/// holds the node at once.
#[test]
fn a_whole_machine_request_holds_a_busy_node_as_the_whole_node_setting_says() {
    let wait = WholeNodeConfig {
        drain_after_ms: 5_000,
        ..WholeNodeConfig::dedicated(4)
    };
    for (config, held_at) in [
        (Config::new(1).with_whole_node(wait), 5_000),
        (Config::new(1), 0),
    ] {
        let core = core_by(&config);
        // A node that serves whole-machine leases.
        let caps = NodeCaps::from_report([("arch", "arm64")])
            .expect("a node report")
            .with_drivers(["container", "native-whole-machine"]);
        let node = Reported::serving(Resources::new(8_000, 64 * GIB), caps);
        up_after(&core, 0, "node-a", ("d1", ""), node);
        submit(&core, 0, 1);
        let mut whole = request(2);
        whole.kind = LeaseKind::WholeMachine;
        let admitted = submit_request(&core, 0, whole).expect("queued");
        let holder = |core: &Core<Rig>| {
            core.with(|view, _| view.sched().reservation(admitted.operation).cloned())
        };

        if held_at > 0 {
            block_on(core.tick(at(held_at - 1)));
            assert_eq!(holder(&core), None, "not before the wait: {config:?}");
        }
        block_on(core.tick(at(held_at)));
        assert_eq!(holder(&core), Some(worker("node-a")), "{config:?}");
    }
}
