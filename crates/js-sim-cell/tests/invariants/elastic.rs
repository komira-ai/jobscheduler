//! The elastic pool's invariants' negative cases (in [`NEGATIVE_CASES`], which the
//! parent's table check reads with its own) and their correct siblings.

use js_elastic::RefusalCode;
use js_elastic_fake::THROTTLE_WAIT;
use js_jobs::{AllocationId, JobId};
use js_sim_cell::{AcquireAnswer, CloudEvent, CloudFacts, ReleaseAnswer};

use super::*;

/// Every negative case of an elastic pool's invariant and the one invariant it must trip,
/// as the parent's `NEGATIVE_CASES`.
pub(crate) const NEGATIVE_CASES: &[NegativeCase] = &[
    (Invariant::InstancesWithinLimits, past_the_limit),
    (Invariant::OnlyOwnMachinesReleased, foreign_released),
    (Invariant::NoAcquireWhilePlanOnly, acquired_after_the_trip),
    (Invariant::ThrottleHonoured, acquired_inside_the_wait),
    (Invariant::LaunchedMachinesSettle, left_running_idle),
];

/// The cloud of pool `burst`, of at most two machines, before any call.
fn cloud() -> CloudFacts {
    CloudFacts::new("burst", 2)
}

/// An acquire of the pool's offer, answered `answer`.
fn acquire(answer: AcquireAnswer) -> CloudEvent {
    CloudEvent::Acquire {
        offer: "sim_1x_a100".to_owned(),
        zone: Some("zone-a".to_owned()),
        acquire: "burst-1".to_owned(),
        answer,
    }
}

/// An acquire answered with machine `machine`.
fn launched(machine: &str) -> CloudEvent {
    acquire(AcquireAnswer::Accepted {
        machine: machine.to_owned(),
    })
}

/// A release of `machine` the cloud began.
fn release(machine: &str) -> CloudEvent {
    CloudEvent::Release {
        machine: machine.to_owned(),
        answer: ReleaseAnswer::Terminating,
    }
}

/// The observations at 300 of `cloud`.
fn at_300(cloud: CloudFacts) -> Observations {
    Observations::quiet().at(ms(300)).with_cloud(cloud)
}

/// The cloud runs three of the scheduler's machines in a pool of at most two.
fn past_the_limit() -> Observations {
    let running = cloud()
        .with_running("fake-0001", host(1))
        .with_running("fake-0002", host(2))
        .with_running("fake-0003", host(3));
    at_300(running)
}

/// The scaler released machine `fake-0009`, which it never launched and no list showed.
fn foreign_released() -> Observations {
    let calls = cloud()
        .with_call(ms(100), launched("fake-0001"))
        .with_call(ms(200), release("fake-0009"));
    at_300(calls)
}

/// The brake read tripped at 100 and an acquire was made at 200, with no reset between.
fn acquired_after_the_trip() -> Observations {
    let calls = cloud()
        .with_call(ms(100), CloudEvent::Brake { tripped: true })
        .with_call(ms(200), launched("fake-0001"));
    at_300(calls)
}

/// The cloud answered an acquire at 1,000 with a 429, and the next acquire came 29,999 ms
/// later, a millisecond inside the wait.
fn acquired_inside_the_wait() -> Observations {
    let wait = THROTTLE_WAIT.as_millis() as u64;
    let calls = cloud()
        .with_call(ms(1_000), acquire(AcquireAnswer::Deferred(THROTTLE_WAIT)))
        .with_call(ms(1_000 + wait - 1), launched("fake-0001"));
    Observations::quiet().at(ms(40_000)).with_cloud(calls)
}

/// At the end, the cloud still runs the scheduler's machine `fake-0001` (host 1), whose
/// agent runs nothing and whose host the server lists neither drained nor released.
fn left_running_idle() -> Observations {
    let running = cloud().with_running("fake-0001", host(1));
    Observations::quiet()
        .at(ms(END))
        .with_cloud(running)
        .with_agent(host(1), AgentTruth::new(capacity()))
}

/// Asserts that the cloud running more of the scheduler's machines than the pool's
/// `max_instances` is flagged, by exact line, and that running exactly the limit, and a
/// cell with no elastic pool, are not. Catches a limit check that is missing, that
/// compares with `>=`, or that reads the scheduler's records instead of the cloud.
#[test]
fn launching_past_the_instance_limit_is_flagged() {
    assert_eq!(
        lines(&step_all(&past_the_limit())),
        [
            "instances-within-limits at 300: the cloud of pool burst runs 3 of this \
             scheduler's machines, past its max_instances of 2"
        ]
    );
    let at_limit = cloud()
        .with_running("fake-0001", host(1))
        .with_running("fake-0002", host(2));
    assert_eq!(lines(&step_all(&at_300(at_limit))), Vec::<String>::new());
    assert_eq!(
        lines(&step_all(&Observations::quiet().at(ms(300)))),
        Vec::<String>::new()
    );
}

/// Asserts that a release of a machine the scheduler never launched is flagged, by exact
/// line, and that releases of a machine an acquire was answered with, and of one a list
/// showed, are not, nor is a list that failed. Catches a check that is missing, one that
/// takes a list's failure for an empty pool, and one that knows only the machines its
/// acquires were answered with (a machine whose answer was lost and a list later showed
/// is the scheduler's).
#[test]
fn releasing_a_foreign_machine_is_flagged() {
    assert_eq!(
        lines(&step_all(&foreign_released())),
        [
            "only-own-machines-released at 300: a release of machine fake-0009 at 200, \
             which this scheduler never launched"
        ]
    );
    let own = cloud()
        .with_call(ms(100), launched("fake-0001"))
        .with_call(ms(110), CloudEvent::List { listed: None })
        .with_call(
            ms(120),
            CloudEvent::List {
                listed: Some(vec!["fake-0002".to_owned()]),
            },
        )
        .with_call(ms(200), release("fake-0001"))
        .with_call(ms(210), release("fake-0002"));
    assert_eq!(lines(&step_all(&at_300(own))), Vec::<String>::new());
}

/// Asserts that an acquire after the brake read tripped is flagged, by exact line, and
/// that acquires after the operator's reset, after the brake reads untripped again, and
/// before any trip, are not; and that a checker that read the trip at one step flags the
/// acquire it reads at a later step. Catches a check that is missing, one that ignores
/// the operator's reset or a brake read untripped, and one that forgets the trip between
/// steps.
#[test]
fn an_acquire_after_the_brake_tripped_is_flagged() {
    assert_eq!(
        lines(&step_all(&acquired_after_the_trip())),
        [
            "no-acquire-while-plan-only at 300: an acquire of offer sim_1x_a100 in zone \
             zone-a at 200, while the brake of pool burst has read tripped since 100 with \
             no reset"
        ]
    );
    let reset = cloud()
        .with_call(ms(100), CloudEvent::Brake { tripped: true })
        .with_call(ms(150), CloudEvent::Brake { tripped: true })
        .with_call(ms(160), CloudEvent::BrakeReset)
        .with_call(ms(200), launched("fake-0001"));
    assert_eq!(lines(&step_all(&at_300(reset))), Vec::<String>::new());
    let untripped = cloud()
        .with_call(ms(100), CloudEvent::Brake { tripped: true })
        .with_call(ms(160), CloudEvent::Brake { tripped: false })
        .with_call(ms(200), launched("fake-0001"));
    assert_eq!(lines(&step_all(&at_300(untripped))), Vec::<String>::new());

    let tripped = cloud().with_call(ms(100), CloudEvent::Brake { tripped: true });
    let mut checker = Checker::new();
    let first = at_300(tripped.clone());
    assert!(checker.step(&first, &Touched::nothing()).is_empty());
    let later = Observations::quiet()
        .at(ms(400))
        .with_cloud(tripped.with_call(ms(350), launched("fake-0001")));
    let found = checker.step(&later, &Touched::nothing());
    assert_eq!(flagged(&found), [Invariant::NoAcquireWhilePlanOnly]);
}

/// Asserts that an acquire a millisecond inside the throttle wait of a 429 is flagged, by
/// exact line, and that one at the end of the wait, one after a deferral that is not a
/// 429, and one after a refusal, are not. Catches a check that is missing, one that
/// compares with `<=`, and one that takes every deferral or refusal for a 429.
#[test]
fn an_acquire_inside_the_throttle_wait_is_flagged() {
    assert_eq!(
        lines(&step_all(&acquired_inside_the_wait())),
        [
            "throttle-honoured at 40000: an acquire of offer sim_1x_a100 in zone zone-a at \
             30999, inside the 30s throttle wait of the 429 answered at 1000"
        ]
    );
    let wait = THROTTLE_WAIT.as_millis() as u64;
    let waited = cloud()
        .with_call(ms(1_000), acquire(AcquireAnswer::Deferred(THROTTLE_WAIT)))
        .with_call(ms(1_000 + wait), launched("fake-0001"));
    let held = Duration::from_secs(5);
    let other = cloud()
        .with_call(ms(1_000), acquire(AcquireAnswer::Deferred(held)))
        .with_call(
            ms(1_001),
            acquire(AcquireAnswer::Refused(RefusalCode::Capacity)),
        )
        .with_call(ms(1_002), acquire(AcquireAnswer::Unknown))
        .with_call(ms(1_003), launched("fake-0001"));
    for case in [waited, other] {
        let case = Observations::quiet().at(ms(40_000)).with_cloud(case);
        assert_eq!(lines(&step_all(&case)), Vec::<String>::new());
    }
}

/// Asserts that, at the end, a machine of the scheduler's the cloud still runs while its
/// agent runs nothing and its host is not leaving the pool is flagged, by exact line,
/// whether it joined or not and whether or not the cell knows its host; and that a
/// running machine whose agent runs a run, or a job, or whose host the server lists as
/// leaving, is not, nor is a cell with no elastic pool. Catches a check that is missing,
/// one that excuses an idle machine because it joined, and one that flags a machine that
/// is busy or already being drained.
#[test]
fn a_machine_left_running_idle_is_flagged() {
    assert_eq!(
        lines(&end(&left_running_idle())),
        [
            "launched-machines-settle at 5000: machine m000 (fake-0001, host host-1) still \
             runs with no work and no drain once the cell is quiet (it never joined)"
        ]
    );
    let mut joined = cloud().with_running("fake-0001", host(1));
    joined
        .machines
        .values_mut()
        .for_each(|machine| machine.joined = true);
    joined.running.insert("fake-0042".to_owned());
    let joined = Observations::quiet().at(ms(END)).with_cloud(joined);
    assert_eq!(
        lines(&end(&joined)),
        [
            "launched-machines-settle at 5000: machine m000 (fake-0001, host host-1) still \
             runs with no work and no drain once the cell is quiet (it joined)",
            "launched-machines-settle at 5000: machine fake-0042 still runs with no work and \
             no drain once the cell is quiet (it never joined)",
        ]
    );

    let running = AgentTruth::new(capacity()).with_run(run(0), lease(0), res(1_000, GIB));
    let job = JobId::parse("sim-job-0").expect("a job id");
    let job = AgentTruth::new(capacity()).with_job(
        job,
        AllocationId::from_bits([1; 16]),
        res(1_000, GIB),
    );
    let machine = || cloud().with_running("fake-0001", host(1));
    let quiet = || Observations::quiet().at(ms(END));
    let busy = quiet().with_cloud(machine()).with_agent(host(1), running);
    let jobbed = quiet().with_cloud(machine()).with_agent(host(1), job);
    let leaving = left_running_idle().with_leaving(host(1));
    for case in [busy, jobbed, leaving, quiet()] {
        let found = end(&case);
        let ours = found
            .iter()
            .filter(|v| v.invariant == Invariant::LaunchedMachinesSettle);
        assert_eq!(ours.count(), 0, "{:?}", lines(&found));
    }
}
