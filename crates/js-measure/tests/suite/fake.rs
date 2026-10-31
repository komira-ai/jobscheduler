//! The simulated farm: placement, runs and the usage store it shows.

use js_measure::farm::{Phase, RunStanding, Standing};
use js_measure::{
    Acquired, Clock, Curve, FakeClock, FakeFarm, Farm, FarmError, HostShape, LeaseAsk, RunAsk,
};

use crate::support::{GIB, T0, job};

fn farm(clock: &FakeClock, hosts: usize) -> FakeFarm<'_> {
    let shape = HostShape {
        cpu_millis: 32_000,
        memory_bytes: 128 * GIB,
    };
    let hosts = (1..=hosts).map(|n| (format!("h{n}"), shape)).collect();
    FakeFarm::new(clock, hosts)
}

fn ask() -> LeaseAsk {
    job("x", 1, None).lease
}

fn run_ask(wall_ms: u64) -> RunAsk {
    job("x", wall_ms, None).run
}

fn refusal(result: Result<impl std::fmt::Debug, FarmError>) -> (u16, String) {
    match result {
        Err(FarmError::Refused { status, code }) => (status, code),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Asserts placement: whole hosts to leases in request order, a lease with no host free
/// pending until a release frees one, a released or unknown lease gone.
#[test]
fn leases_take_whole_hosts_in_order() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 2);
    let first = farm.acquire(&ask()).unwrap();
    let second = farm.acquire(&ask()).unwrap();
    let third = farm.acquire(&ask()).unwrap();
    assert_eq!(
        first,
        Acquired::Held {
            id: "lease-1".to_owned(),
            host: "h1".to_owned()
        }
    );
    assert_eq!(
        second,
        Acquired::Held {
            id: "lease-2".to_owned(),
            host: "h2".to_owned()
        }
    );
    assert_eq!(
        third,
        Acquired::Pending {
            id: "lease-3".to_owned()
        }
    );
    assert_eq!(farm.lease("lease-3").unwrap(), Standing::Pending);
    farm.release("lease-2").unwrap();
    assert_eq!(farm.lease("lease-2").unwrap(), Standing::Gone);
    assert_eq!(
        farm.lease("lease-3").unwrap(),
        Standing::Held {
            host: "h2".to_owned()
        }
    );
    assert_eq!(farm.lease("nope").unwrap(), Standing::Gone);
}

/// Asserts a run lasts its wall limit and then reads as exited with the farm's outcome
/// (`succeeded` unless set), and that the refusals have the codes the live server gives:
/// a run on a lease that is not held is 409, an unknown run, lease or host is 404.
#[test]
fn a_run_lasts_its_wall_limit_and_refusals_have_codes() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 1).with_outcome("out_of_memory");
    let Acquired::Held { id, .. } = farm.acquire(&ask()).unwrap() else {
        panic!("a free host");
    };
    let run = farm.start_run(&id, &run_ask(5_000)).unwrap();
    let live = farm.run(&id, &run).unwrap();
    assert_eq!((live.phase(), live.outcome), (Phase::Running, None));
    clock.sleep_ms(5_000);
    assert_eq!(
        farm.run(&id, &run).unwrap(),
        RunStanding {
            state: "exited".to_owned(),
            outcome: Some("out_of_memory".to_owned())
        }
    );
    assert_eq!(
        refusal(farm.start_run("lease-9", &run_ask(1))),
        (409, "not-held".to_owned())
    );
    assert_eq!(
        refusal(farm.run(&id, "run-9")),
        (404, "unknown-run".to_owned())
    );
    assert_eq!(
        refusal(farm.release("lease-9")),
        (404, "unknown-lease".to_owned())
    );
    assert_eq!(refusal(farm.host("h9")), (404, "unknown-host".to_owned()));
    assert_eq!(farm.host("h1").unwrap().cpu_millis, 32_000);
}

/// Asserts the usage store a run leaves: per minute, the CPU and memory the run used in
/// proportion to the part of the minute it ran, as a percent of its limits (50 by
/// default), and the number of leases that ran in it; a minute with no run has no row.
#[test]
fn the_usage_store_shows_what_ran_in_each_minute() {
    let clock = FakeClock::at(T0 + 30_000);
    let farm = farm(&clock, 1);
    let Acquired::Held { id, .. } = farm.acquire(&ask()).unwrap() else {
        panic!("a free host");
    };
    farm.start_run(&id, &run_ask(60_000)).unwrap();
    let rows = farm.usage_rows("h1", 9, 12).unwrap();
    let shape: Vec<(u64, u64, u64, u64)> = rows
        .iter()
        .map(|r| (r.minute, r.cpu_millis, r.ws_bytes, r.leases))
        .collect();
    // Limits 4000 millicores and 8 GiB, half used, half of each of minutes 10 and 11.
    assert_eq!(
        shape,
        vec![(10, 1_000, 4 * GIB, 1), (11, 1_000, 4 * GIB, 1)]
    );
    assert!(farm.usage_rows("other", 9, 12).unwrap().is_empty());
}

/// Asserts that two leases that ran in one minute are two leases in its row (so the
/// harness will not attribute the minute to either) and that the use percent is a knob.
#[test]
fn two_leases_in_a_minute_are_two() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 1).with_use_percent(100);
    let Acquired::Held { id, .. } = farm.acquire(&ask()).unwrap() else {
        panic!("a free host");
    };
    farm.start_run(&id, &run_ask(30_000)).unwrap();
    clock.sleep_ms(30_000);
    farm.release(&id).unwrap();
    let Acquired::Held { id, .. } = farm.acquire(&ask()).unwrap() else {
        panic!("the host was released");
    };
    farm.start_run(&id, &run_ask(30_000)).unwrap();
    let rows = farm.usage_rows("h1", 10, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].leases, rows[0].cpu_millis, rows[0].ws_bytes),
        (2, 4_000, 8 * GIB)
    );
}

/// Asserts that the farm answers a curve for the profiles it was given and none for others.
#[test]
fn curves_are_the_ones_given() {
    let clock = FakeClock::at(T0);
    let curve = Curve {
        runs: 3,
        ws_bytes: vec![GIB],
        cores_milli: vec![500],
    };
    let farm = farm(&clock, 1).with_curve("p", curve.clone());
    assert_eq!(farm.curve("p").unwrap(), Some(curve));
    assert_eq!(farm.curve("q").unwrap(), None);
}
