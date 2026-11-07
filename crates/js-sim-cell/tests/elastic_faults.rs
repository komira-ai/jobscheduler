//! The elastic fault family: the cloud of an elastic pool loses the answer to a launch,
//! refuses it, throttles it, boots machines that never join, preempts a machine, holds a
//! machine that is not the scheduler's, or lags in its list, each scenario over [`SEEDS`]
//! seeds under every invariant of `Invariant::ALL`; and two negative cases, a cloud that
//! refuses every release and a hand-built record of a pool that ignores its brake.
//!
//! The scenarios are in `cell/elastic_faults.rs`, shared with the replay tests that pin a
//! trace hash of one of them. Each states only what the server as built and the design
//! both promise; the times each reads follow from the server's defaults, which that file's
//! module doc names. Sizes, in agent requests a seed: one or two machines beating every
//! 5 s for at most 6,410 s of virtual time, about 2,600 heartbeats a seed.

#[path = "cell/elastic.rs"]
#[allow(
    dead_code,
    reason = "the elastic family's scenarios are not run here: this family takes only its pool's name, offers and shape"
)]
mod elastic;
#[path = "cell/elastic_faults.rs"]
mod elastic_faults;

use js_sim::text::time;
use js_sim_cell::{
    AcquireAnswer, Checker, CloudEvent, CloudFacts, Invariant, Observations, ScenarioFailure,
    Touched,
};
use js_types::sched::FarmTime;

/// How many seeds each scenario runs.
const SEEDS: u64 = 16;

/// Proves design section 7, "Scaler double launch" ("records written whole before the
/// provider call; acquire ids idempotent"), when the cloud loses the first answer: the
/// call landed, exactly one machine exists for the demand at every step, and it joins.
/// Witness `acquire_lost`. Red for a scaler that launches again on a lost answer, and for
/// one that drops the machine the list shows under its record's acquire id.
#[test]
fn an_acquire_whose_answer_is_lost_but_landed_launches_one_machine() {
    elastic_faults::lost_but_landed().run_seeds(SEEDS);
}

/// Proves the other half of section 7's "Scaler double launch": a lost answer that did not
/// land is given up once the unknown hold passes and the launch is made once more, one
/// machine after it and never two. The token of the second launch is design section 8
/// row 34 (Suspected) and is not asserted. Witness `acquire_lost`. Red for a scaler that
/// launches again before the hold (two machines' worth of acquires) or never.
#[test]
fn an_acquire_whose_answer_is_lost_and_did_not_land_is_retried_once() {
    elastic_faults::lost_and_not_landed().run_seeds(SEEDS);
}

/// Proves design section 6.5's brake: three boot failures in a row (machines that never
/// say hello, given up at the boot timeout) trip the pool to plan-only, after cooldowns of
/// 10 and then 20 minutes; `no-acquire-while-plan-only` holds until the operator's reset
/// through `POST /v1/pools/{pool}/brake/reset`, and after it the pool launches again. Red
/// for a brake that does not trip, a cooldown that does not double, a pool that launches
/// while tripped, and a reset that the route does not reach.
#[test]
fn three_boot_failures_trip_the_brake_and_reset_clears_it() {
    elastic_faults::boot_failures_trip_the_brake().run_seeds(SEEDS);
}

/// Proves that a `denied` refusal is a failure for the brake by the other route: each
/// starts the pool's cooldown and the third trips it, until the operator's reset, after
/// which the pool launches. Red for a denial that cools nothing (three acquires in the
/// first ticks) or never counts toward the trip.
#[test]
fn denied_launches_start_a_cooldown_and_count_toward_the_trip() {
    elastic_faults::denied_launches().run_seeds(SEEDS);
}

/// Proves that a `capacity` refusal declines the offer for the attempt and is not a
/// failure for the brake: four of them never trip it, and a later acquire launches. Red
/// for a brake that counts capacity refusals (the distinction the boot and denied
/// scenarios rely on).
#[test]
fn a_capacity_refusal_declines_the_offer_and_trips_nothing() {
    elastic_faults::capacity_refusals().run_seeds(SEEDS);
}

/// Proves that a 429 storm defers launches by the throttle wait: no acquire inside 30 s of
/// a 429 (`throttle-honoured`), and the demand is covered once the storm ends. Witness
/// `acquire_throttled`. Red for a scaler that retries inside the wait or drops the waiting
/// launch.
#[test]
fn a_429_storm_defers_launches_by_the_throttle_wait() {
    elastic_faults::a_429_storm().run_seeds(SEEDS);
}

/// Proves that a machine of the account the scheduler did not launch, named or unmarked,
/// is never released (`only-own-machines-released`) and counts toward the cloud's quota (a
/// launch refused for quota while the scheduler ran one machine of three). Red for a
/// scaler that adopts or releases a foreign machine.
#[test]
fn a_foreign_machine_in_the_account_is_never_released() {
    elastic_faults::foreign_machines().run_seeds(SEEDS);
}

/// Proves that a machine preempted mid-run double-books nothing: its run is not run again
/// (`at-most-one-launch`), no host is booked twice (`no-double-booking`), each run has one
/// terminal answer (`one-terminal-answer`), and a new machine covers the demand that
/// follows. Witness `machine_preempted`. Red for a server that places the preempted run a
/// second time or books the new machine twice.
#[test]
fn a_preempted_machine_ends_its_runs_and_double_books_nothing() {
    elastic_faults::a_preempted_machine().run_seeds(SEEDS);
}

/// Proves that a provider list lagging 60 s behind the truth does not launch a second
/// machine for the same demand: one machine at every step. Red for a scaler that counts
/// only listed machines as supply.
#[test]
fn a_slow_provider_list_does_not_launch_a_second_machine_for_the_same_demand() {
    elastic_faults::a_lagging_list().run_seeds(SEEDS);
}

/// The negative case of `launched-machines-settle`: a cloud that refuses every release,
/// and machines that never join, leave the first machine running with no work and no host
/// to drain once the scaler gives it up, and the end check names it, `m000`, as one that
/// never joined. Its twin, with releases that pass, settles. Catches an end check that
/// reads the scheduler's records (which hold the machine releasing) rather than the cloud.
#[test]
fn a_cloud_whose_release_is_refused_forever_fails_launched_machines_settle() {
    let failure = elastic_faults::a_refused_release(true)
        .try_run_seeds(1)
        .expect_err("a machine whose release is refused never settles");
    let ScenarioFailure::Violated(report) = &failure else {
        panic!("not a violation: {failure}");
    };
    assert_eq!(
        report.violation.invariant,
        Invariant::LaunchedMachinesSettle
    );
    let detail = &report.violation.detail;
    assert!(detail.starts_with("machine m000 ("), "{detail}");
    assert!(
        detail.ends_with(
            " still runs with no work and no drain once the cell is quiet (it never joined)"
        ),
        "{detail}"
    );
    let released = elastic_faults::a_refused_release(false).try_run_seeds(1);
    assert!(released.is_ok(), "{released:?}");
}

/// The negative case of `no-acquire-while-plan-only`, over a hand-built record of the
/// wrapper shaped as the brake scenario's (no server change): three launches, the brake
/// read tripped after the third, and a fourth acquire with no reset between, which is
/// flagged by its exact line; with the operator's reset recorded before the fourth, it is
/// not. Catches a check that forgets the trip, and one that a reset does not clear.
#[test]
fn a_pool_with_the_brake_ignored_fails_no_acquire_while_plan_only() {
    let (tripped, fourth) = (FarmTime::from_millis(4_510_000), 4_600_000);
    let launched = |machine: &str| CloudEvent::Acquire {
        offer: "sim_1x_a100".to_owned(),
        zone: Some("zone-a".to_owned()),
        acquire: format!("burst-{machine}"),
        answer: AcquireAnswer::Accepted {
            machine: machine.to_owned(),
        },
    };
    let record = |reset: bool| {
        let mut cloud = CloudFacts::new(elastic::POOL, 2)
            .with_call(FarmTime::from_millis(5_000), launched("fake-0001"))
            .with_call(FarmTime::from_millis(1_505_000), launched("fake-0002"))
            .with_call(FarmTime::from_millis(3_605_000), launched("fake-0003"))
            .with_call(tripped, CloudEvent::Brake { tripped: true });
        if reset {
            cloud = cloud.with_call(FarmTime::from_millis(4_590_000), CloudEvent::BrakeReset);
        }
        let cloud = cloud.with_call(FarmTime::from_millis(fourth), launched("fake-0004"));
        let observations = Observations::quiet()
            .at(FarmTime::from_millis(fourth))
            .with_cloud(cloud);
        let found = Checker::new().step(&observations, &Touched::everything_in(&observations));
        found
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
    };
    let at = time(FarmTime::from_millis(fourth));
    assert_eq!(
        record(false),
        [format!(
            "no-acquire-while-plan-only at {at}: an acquire of offer sim_1x_a100 in zone \
             zone-a at {at}, while the brake of pool burst has read tripped since {} with \
             no reset",
            time(tripped)
        )]
    );
    assert_eq!(record(true), Vec::<String>::new());
}
