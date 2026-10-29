//! What a [`Config`] sets on the scheduler: the memory ladder, the bound on memory floors,
//! the count of lost leases that refuses an operation, and the whole-machine settings. A
//! core built from the defaults behaves as it did before they were settable (the other
//! files of the suite build one); each test here changes one setting and sees the scheduler
//! follow it.

use js_caps::NodeCaps;
use js_sched::MemoryLadder;
use js_server_core::{Config, Core, PercentError, Report, Reported, WholeNodeConfig};
use js_types::sched::{Failure, LeaseId, LeaseKind, OperationId, Outcome, Resources, WorkKey};

use crate::common::{
    Call, GIB, Rig, at, beat, block_on, calls, core_by, lease, reported, request, resend, submit,
    submit_request, up, up_after, worker,
};

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
    // Without the setting: the default rungs, 4 then 16 GiB.
    for (config, rungs) in [
        (Config::new(1).with_memory_ladder(ladder), [2, 8]),
        (Config::new(1), [4, 16]),
    ] {
        let core = core_by(&config);
        let stream = up(&core, 0, "node-a", "d1");
        let admitted = submit(&core, 10, 1);
        let floor = |core: &Core<Rig>| core.with(|view, _| view.sched().memory_floor(&key(1)));

        kill(&core, 20, admitted.operation, lease(1, 0));
        assert_eq!(
            floor(&core),
            Some(rungs[0] * GIB),
            "the first rung above its booking: {config:?}"
        );
        beat(&core, 30, ("node-a", stream), 1, &[]);
        kill(&core, 40, admitted.operation, lease(1, 1));
        assert_eq!(
            floor(&core),
            Some(rungs[1] * GIB),
            "the second rung: {config:?}"
        );
    }
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
/// operation (its callers are told), where the default count of three refuses it on the
/// third and not before.
#[test]
fn a_loss_count_of_two_refuses_the_operation_on_the_second_lost_lease() {
    for (config, count) in [
        (Config::new(1).with_loss_requeues(2), 2),
        (Config::new(1), 3),
    ] {
        let core = core_by(&config);
        up(&core, 0, "node-a", "d1");
        let admitted = submit(&core, 0, 1);

        // The daemon registers again and lists no lease: a lease lost to a reconnect
        // charges no farm rerun, so the loss count alone decides when the operation ends.
        for loss in 1..=count {
            let now = loss * 100;
            let stream = up(&core, now, "node-a", "d1");
            beat(&core, now + 1, ("node-a", stream), 1, &[]);
            assert_eq!(
                core.with(|view, _| view.sched().losses(admitted.operation)),
                Some(u32::try_from(loss).expect("small")),
                "losses after reconnect {loss}: {config:?}"
            );
            assert_eq!(
                calls(&core).contains(&Call::Refuse(admitted.operation)),
                loss == count,
                "after loss {loss}: {config:?}"
            );
        }
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
        // A hold under whole-node settings carries a mark; one without them has none.
        let marked = core.with(|view, _| view.sched().whole_mark(admitted.operation).is_some());
        assert_eq!(marked, held_at > 0, "{config:?}");
    }
}

/// Catches: `memory_schedulable_percent` ignored by the core, or the status showing the
/// reported memory. A host that reports 64 GiB and 8 cores is shown at the floor of the
/// share in memory and at its cores as reported, at its registration and after a resent
/// report; the default leaves the report as it was.
#[test]
fn the_scheduler_shows_the_cut_capacity_of_a_host() {
    let shown = |core: &Core<Rig>| core.with(|view, _| view.sched().capacity(&worker("node-a")));
    for (config, memory) in [
        (
            Config::new(1)
                .with_memory_schedulable_percent(90)
                .expect("in range"),
            u64::try_from(u128::from(64 * GIB) * 90 / 100).expect("fits"),
        ),
        (Config::new(1), 64 * GIB),
    ] {
        let core = core_by(&config);
        let stream = up(&core, 0, "node-a", "d1");
        let want = Some(Resources::new(8_000, memory));
        assert_eq!(shown(&core), want, "at registration: {config:?}");
        resend(&core, 1, ("node-a", stream), reported(8)).expect("heard");
        assert_eq!(shown(&core), want, "after a resent report: {config:?}");
    }
}

/// Catches: a percent outside 1 to 100 accepted by `Config`, by `validate` or by the
/// builder, and an error that does not name the field.
#[test]
fn a_percent_of_zero_or_above_a_hundred_is_refused_naming_the_field() {
    assert_eq!(Config::new(1).memory_schedulable_percent, 100);
    assert_eq!(Config::new(1).validate(), Ok(()));
    for bad in [0, 101, 1_000] {
        let mut config = Config::new(1);
        config.memory_schedulable_percent = bad;
        let err = config.validate().expect_err("refused");
        assert_eq!(err, PercentError(bad));
        assert!(
            err.to_string().contains("memory_schedulable_percent"),
            "{err}"
        );
        let built = Config::new(1).with_memory_schedulable_percent(bad);
        assert_eq!(built, Err(PercentError(bad)));
    }
    for good in [1, 90, 100] {
        let config = Config::new(1)
            .with_memory_schedulable_percent(good)
            .expect("in range");
        assert_eq!(config.memory_schedulable_percent, good);
        assert_eq!(config.validate(), Ok(()));
    }
}

/// Catches: a core built from an out-of-range value (which a server that skipped
/// `validate` could hand it) booking all of the host at 0, or more than it reports above
/// 100.
#[test]
fn a_core_built_from_an_out_of_range_percent_holds_it_to_the_range() {
    for (bad, memory) in [(0, 64 * GIB / 100), (500, 64 * GIB)] {
        let mut config = Config::new(1);
        config.memory_schedulable_percent = bad;
        let core = core_by(&config);
        up(&core, 0, "node-a", "d1");
        let shown = core.with(|view, _| view.sched().capacity(&worker("node-a")));
        assert_eq!(shown.map(|c| c.memory_bytes), Some(memory), "{bad}");
    }
}
