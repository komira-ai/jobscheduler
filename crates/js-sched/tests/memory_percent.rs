//! `memory_schedulable_percent` in the scheduler: the share of a worker's reported memory
//! that admission books against. Each test registers a worker of 251 GiB.

#[path = "load_rig/mod.rs"]
mod load_rig;

use js_caps::NodeCaps;
use js_sched::{Event, LoadConfig, MemoryPercent, Scheduler};
use js_types::sched::{LeaseKind, Resources};

use load_rig::{GIB, Rig, w, warm};

/// The worker of these tests.
fn node() -> Resources {
    Resources {
        leases: Some(64),
        gpus: 2,
        vms: 3,
        ..Resources::new(96_000, 251 * GIB)
    }
}

/// `floor(251 GiB * 90 / 100)`, computed wide so that it does not share the code under test.
fn ninety() -> u64 {
    u64::try_from(u128::from(251 * GIB) * 90 / 100).expect("fits")
}

/// A rig whose scheduler books `percent` of a worker's memory, with worker `a` registered.
fn rig(percent: u32) -> Rig {
    let mut r = Rig::on();
    r.s = Scheduler::new(1)
        .with_load_config(LoadConfig::default())
        .with_memory_percent(MemoryPercent::new(percent).expect("in range"));
    r.up_with("a", node(), caps());
    r.baseline("a");
    r
}

/// A container worker that also runs VMs.
fn caps() -> NodeCaps {
    load_rig::caps("container").with_drivers(["container", "vm"])
}

/// How many of the actions predicted at `memories` bytes (each limited to 8 GiB, one
/// core) start in one round.
fn started(r: &mut Rig, memories: &[u64]) -> usize {
    for memory in memories {
        r.action(1_000, 8 * GIB, Some(warm(250, *memory)));
    }
    r.tick().len()
}

/// Catches: the percent not applied to the capacity admission books against, or applied
/// off by one. At 90% a worker of 251 GiB books up to `floor(251 GiB * 90 / 100)` bytes:
/// leases summing to exactly that all start, and one byte more leaves the last waiting.
/// At 100% the same one byte over starts, so the refusal is the percent's.
#[test]
fn ninety_percent_admits_exactly_its_floor_and_refuses_one_byte_more() {
    let limit = ninety();
    assert_eq!(started(&mut rig(90), &[limit - 4096, 4096]), 2);
    assert_eq!(started(&mut rig(90), &[limit - 4096, 4097]), 1);
    assert_eq!(started(&mut rig(100), &[limit - 4096, 4097]), 2);
}

/// Catches: a default that is not 100, and a percent of 100 that cuts anything: all 251
/// GiB books at 100%, and one byte more does not.
#[test]
fn a_hundred_percent_books_the_whole_capacity() {
    let all = 251 * GIB;
    assert_eq!(MemoryPercent::default().get(), 100);
    assert_eq!(started(&mut rig(100), &[all - 4096, 4096]), 2);
    assert_eq!(started(&mut rig(100), &[all - 4096, 4097]), 1);
}

/// Catches: the cut applied only to the load predicate. An action limited above the
/// effective capacity (230 GiB of 251) is not placed at 90%, a whole-machine request
/// for the reported memory does not fit, and a VM booking the reported memory does not
/// either; at 100% each is placed on its own.
#[test]
fn requests_above_the_effective_capacity_are_placed_nowhere_else() {
    let place = |percent: u32, kind: LeaseKind, memory: u64| {
        let mut r = rig(percent);
        let mut request = r.request(kind, 1_000, memory);
        request.resources = request.resources.with_vms(u64::from(kind == LeaseKind::Vm));
        r.submit(request);
        r.tick().len()
    };
    for kind in [LeaseKind::Action, LeaseKind::WholeMachine, LeaseKind::Vm] {
        let memory = if kind == LeaseKind::Action {
            230 * GIB
        } else {
            251 * GIB
        };
        assert_eq!(
            place(90, kind, memory),
            0,
            "{kind:?} over the effective capacity"
        );
        assert_eq!(place(100, kind, memory), 1, "{kind:?} at 100%");
    }
    assert_eq!(
        place(90, LeaseKind::Action, 8 * GIB),
        1,
        "a small action is placed"
    );
}

/// Catches: the status showing the reported capacity, the cut applied twice (a second
/// report cut again, or a report cut from the cut one), and cores, GPUs, VM slots or the
/// lease cap changed.
#[test]
fn the_capacity_shown_is_the_cut_one_applied_once_and_only_to_memory() {
    let mut r = rig(90);
    let cut = Resources {
        memory_bytes: ninety(),
        ..node()
    };
    assert_eq!(r.s.capacity(&w("a")), Some(cut));
    for _ in 0..2 {
        r.feed(Event::Capacity {
            worker: w("a"),
            capacity: node(),
            caps: caps(),
        });
        assert_eq!(
            r.s.capacity(&w("a")),
            Some(cut),
            "a report is cut from itself"
        );
    }
    let smaller = Resources::new(8_000, 100 * GIB);
    r.feed(Event::Capacity {
        worker: w("a"),
        capacity: smaller,
        caps: caps(),
    });
    assert_eq!(
        r.s.capacity(&w("a")),
        Some(Resources::new(8_000, 90 * GIB)),
        "a changed report"
    );
}

/// Catches: a percent that is not a floor in whole bytes, and one that overflows for the
/// largest capacity.
#[test]
fn the_cut_is_a_floor_and_cannot_overflow() {
    let p = |n: u32| MemoryPercent::new(n).expect("in range");
    assert_eq!(p(90).of_bytes(251 * GIB), ninety());
    assert_eq!(p(1).of_bytes(199), 1);
    assert_eq!(p(1).of_bytes(99), 0);
    assert_eq!(p(100).of_bytes(u64::MAX), u64::MAX);
    let wide = u128::from(u64::MAX) * 99 / 100;
    assert_eq!(u128::from(p(99).of_bytes(u64::MAX)), wide);
    assert_eq!(MemoryPercent::clamped(0).get(), 1);
    assert_eq!(MemoryPercent::clamped(101).get(), 100);
    assert_eq!(MemoryPercent::clamped(70).get(), 70);
}

/// Catches: 0 and values above 100 accepted, and an error that does not name the key.
#[test]
fn a_percent_outside_one_to_a_hundred_is_refused_naming_the_key() {
    for bad in [0, 101, 255, 256, 100_000] {
        let err = MemoryPercent::new(bad).expect_err("refused");
        assert!(
            err.to_string().contains("memory_schedulable_percent"),
            "{err}"
        );
        assert!(err.to_string().contains(&bad.to_string()), "{err}");
    }
    assert_eq!(MemoryPercent::new(1).map(MemoryPercent::get), Ok(1));
    assert_eq!(MemoryPercent::new(100).map(MemoryPercent::get), Ok(100));
}
