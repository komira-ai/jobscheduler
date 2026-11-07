//! The elastic fault scenarios, shared by the elastic fault family (`tests/elastic_faults.rs`)
//! and the golden hashes (`tests/replay.rs`): the cloud of an elastic pool loses the answer
//! to a launch, refuses it, throttles it, boots a machine that never says hello, preempts a
//! machine, holds a machine that is not the scheduler's, or lags in its list, each scripted
//! into the elastic fake at a virtual time (`js_sim_cell::actions`).
//!
//! Each scenario expects only what the server as built and the design both promise. The
//! times below follow from the server's tick (5 s), the fake's knobs (a machine boots in
//! 120 s, a lost answer stays unresolved for 30 minutes) and the scaler's defaults, which
//! the cell's configuration file keeps: a boot timeout of 15 minutes, a brake that trips
//! after three failures in a row, and a cooldown of 10 minutes that doubles after each, to
//! 2 hours at most. The first acquire is made at one of the first ticks.
//!
//! Sizes, in agent requests a seed: one or two machines, each beating every 5 s for at most
//! 6,410 s of virtual time (the brake scenario, the longest: its reset at [`RESET_AT`]
//! and [`SETTLE`] after it), 2 * 6,500 / 5 = 2,600
//! heartbeats, under the 30,000 a seed of the cell's sizing rule.

use std::time::Duration;

use js_elastic::RefusalCode;
use js_elastic_fake::{Call, Knobs, Step};
use js_sim_cell::actions::{
    add_foreign_machine, machines_never_join, preempt_machine, reset_brake, script_provider,
    script_provider_forever, set_knobs, submit_work,
};
use js_sim_cell::{
    Cell, ClientEvent, CloudEvent, Counter, ElasticPool, Expect, HowEnded, ItemId, Scenario,
    ServerSpec, Work,
};

use crate::elastic::{POOL, offers, shape};

/// How long each run lasts.
pub const RUN: Duration = Duration::from_secs(300);

/// The quiet time after the last action of a scenario whose machines all join: ample for
/// a boot, the run, the idle time and the release after it.
pub const SETTLE: Duration = Duration::from_secs(1_500);

/// When the brake scenarios reset the brake: after the third failure (at about 4,505 s for
/// the boot failures, 1,805 s for the denials) and a tick to read it.
pub const RESET_AT: Duration = Duration::from_secs(4_910);

/// When the denied launches' scenario resets the brake.
pub const DENIED_RESET_AT: Duration = Duration::from_secs(2_010);

/// The pool of every scenario of the family, of at most `max_instances` machines.
fn pool(max_instances: u64) -> ElasticPool {
    ElasticPool::ec2_like(POOL, offers(), max_instances)
}

/// `scenario` of the family: its server's one pool `pool`.
fn on(scenario: Scenario, pool: ElasticPool) -> Scenario {
    scenario
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::elastic_pool(pool, shape()))
}

/// The time `secs` seconds after the start.
fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// `Step::Lost { landed: true }` on the first acquire, one whole-host run waiting. The
/// scaler writes its record before the call, so the lost answer leaves it unresolved and
/// covering the run; the next list shows the machine under the record's acquire id and the
/// scaler takes it as the record's, making no second acquire. The cloud runs one machine
/// for the run, launched by 60 s and never another (the count of launches only grows, so
/// one at the end is one at every step); it joins and the run succeeds.
pub fn lost_but_landed() -> Scenario {
    on(
        Scenario::new("an_acquire_whose_answer_is_lost_but_landed_launches_one_machine"),
        pool(2),
    )
    .at(
        Duration::ZERO,
        script_provider(Call::Acquire, &[Step::Lost { landed: true }]),
    )
    .work(1, Work::whole_host(RUN))
    .expect_at(secs(60), Expect::machines_launched(POOL, 1))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .expect_that("every machine launched joined", every_machine_joined)
    .witness("acquire_lost", |c| c.get(Counter::AcquireLost) >= 1)
    .settle_within(SETTLE)
}

/// `Step::Lost { landed: false }` on the first acquire. The record stays unresolved for
/// the fake's and the scaler's 30 minutes, covering the run, so nothing is launched before
/// then (none at 1,500 s); then the scaler gives the acquire up and launches again, once.
/// Two acquires in all, the first unanswered and the second answered with the one machine,
/// which joins, and the run succeeds. Which acquire id or token the second uses is not
/// asserted (design section 8 row 34).
pub fn lost_and_not_landed() -> Scenario {
    on(
        Scenario::new("an_acquire_whose_answer_is_lost_and_did_not_land_is_retried_once"),
        pool(2),
    )
    .at(
        Duration::ZERO,
        script_provider(Call::Acquire, &[Step::Lost { landed: false }]),
    )
    .work(1, Work::whole_host(RUN).give_up_after(secs(5_400)))
    .expect_at(secs(1_500), Expect::machines_launched(POOL, 0))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .expect_that("the lost acquire was made again once", retried_once)
    .expect_that("every machine launched joined", every_machine_joined)
    .witness("acquire_lost", |c| c.get(Counter::AcquireLost) >= 1)
    .settle_within(SETTLE)
}

/// The machines of the pool never say hello (`machines_never_join`), one whole-host run
/// waiting. Each machine is a boot failure at the boot timeout (15 minutes after its
/// launch): the scaler releases it and starts the pool's cooldown, 10 minutes after the
/// first failure (one machine at 1,400 s), 20 after the second (two at 3,400 s, where a
/// cooldown that did not double would have launched the third), and the third trips the
/// brake: three machines and the pool tripped at 4,900 s. From the trip until the reset
/// `no-acquire-while-plan-only` holds over every acquire. At [`RESET_AT`] the machines join
/// again and the operator resets the brake through the server's route; the pool launches a
/// fourth machine, which joins, the run succeeds, and the brake reads untripped at the end.
pub fn boot_failures_trip_the_brake() -> Scenario {
    on(
        Scenario::new("three_boot_failures_trip_the_brake_and_reset_clears_it"),
        pool(2),
    )
    .at(Duration::ZERO, machines_never_join(POOL, true))
    .work(1, Work::whole_host(RUN).give_up_after(secs(6_600)))
    .expect_at(secs(1_400), Expect::machines_launched(POOL, 1))
    .expect_at(secs(1_400), Expect::pool_tripped(POOL, false))
    .expect_at(secs(3_400), Expect::machines_launched(POOL, 2))
    .expect_at(secs(3_400), Expect::pool_tripped(POOL, false))
    .expect_at(secs(4_900), Expect::machines_launched(POOL, 3))
    .expect_at(secs(4_900), Expect::pool_tripped(POOL, true))
    .at(RESET_AT, machines_never_join(POOL, false))
    .at(RESET_AT, reset_brake(POOL))
    .expect(Expect::machines_launched(POOL, 4))
    .expect(Expect::pool_tripped(POOL, false))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .settle_within(SETTLE)
}

/// `Step::Refuse(RefusalCode::Denied)` on the first three acquires. Each denial starts the
/// pool's cooldown (10 minutes, then 20) and counts toward the brake as a boot failure
/// does: untripped at 1,700 s (three denials with no cooldown between them would have
/// tripped it in the first ticks), tripped at 2,000 s after the third. The operator resets
/// it at [`DENIED_RESET_AT`] and the next acquire launches the one machine, which joins.
pub fn denied_launches() -> Scenario {
    let denied = Step::Refuse(RefusalCode::Denied);
    on(
        Scenario::new("denied_launches_start_a_cooldown_and_count_toward_the_trip"),
        pool(2),
    )
    .at(
        Duration::ZERO,
        script_provider(Call::Acquire, &[denied, denied, denied]),
    )
    .work(1, Work::whole_host(RUN).give_up_after(secs(6_000)))
    .expect_at(secs(1_700), Expect::machines_launched(POOL, 0))
    .expect_at(secs(1_700), Expect::pool_tripped(POOL, false))
    .expect_at(secs(2_000), Expect::pool_tripped(POOL, true))
    .at(DENIED_RESET_AT, reset_brake(POOL))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::pool_tripped(POOL, false))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .witness("acquire_refused", |c| c.get(Counter::AcquireRefused) >= 3)
    .settle_within(SETTLE)
}

/// `Step::Refuse(RefusalCode::Capacity)` on the first four acquires. The scaler declines
/// the offer for that attempt and tries again later, and a capacity refusal is not a
/// failure for the brake: it never reads tripped, at 200 s, at the end, or at any reading
/// in between, and the fifth acquire launches the one machine, which joins. This pins the
/// distinction the denied launches' scenario relies on.
pub fn capacity_refusals() -> Scenario {
    let full = Step::Refuse(RefusalCode::Capacity);
    on(
        Scenario::new("a_capacity_refusal_declines_the_offer_and_trips_nothing"),
        pool(2),
    )
    .at(
        Duration::ZERO,
        script_provider(Call::Acquire, &[full, full, full, full]),
    )
    .work(1, Work::whole_host(RUN))
    .expect_at(secs(200), Expect::pool_tripped(POOL, false))
    .expect(Expect::pool_tripped(POOL, false))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .expect_that("the brake never read tripped", never_tripped)
    .witness("acquire_refused", |c| c.get(Counter::AcquireRefused) >= 4)
    .settle_within(SETTLE)
}

/// Every acquire answered 429 (`Step::Throttled`, a deferral of the fake's 30 s
/// `THROTTLE_WAIT`) for the first 300 s, then every acquire passes. The scaler makes no
/// acquire inside the wait of a 429 (`throttle-honoured`), the storm counts nothing toward
/// the brake, and once it ends the waiting run is covered by one machine, which joins.
pub fn a_429_storm() -> Scenario {
    on(
        Scenario::new("a_429_storm_defers_launches_by_the_throttle_wait"),
        pool(2),
    )
    .at(
        Duration::ZERO,
        script_provider_forever(Call::Acquire, Step::Throttled),
    )
    .at(
        secs(300),
        script_provider_forever(Call::Acquire, Step::Pass),
    )
    .work(1, Work::whole_host(RUN))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::pool_tripped(POOL, false))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .witness("acquire_throttled", |c| {
        c.get(Counter::AcquireThrottled) >= 1
    })
    .settle_within(SETTLE)
}

/// Two machines in the cloud's account that the scheduler did not launch, one under
/// another scheduler's name and one unmarked with none, and a quota of three machines;
/// two whole-host runs waiting in a pool of at most two. The foreign machines count toward
/// the quota: the scaler's first acquire launches the third machine of the account and its
/// second is refused for quota, so the second run waits for the first machine and runs on
/// it. Neither foreign machine is ever released (`only-own-machines-released`), both still
/// run at the end, and the scheduler launched one machine.
pub fn foreign_machines() -> Scenario {
    let knobs = Knobs {
        quota: Some(3),
        ..Knobs::default()
    };
    on(
        Scenario::new("a_foreign_machine_in_the_account_is_never_released"),
        pool(2),
    )
    .at(Duration::ZERO, set_knobs(knobs))
    .at(
        Duration::ZERO,
        add_foreign_machine(Some("another-scheduler")),
    )
    .at(Duration::ZERO, add_foreign_machine(None))
    .work(2, Work::whole_host(RUN))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(2, HowEnded::Succeeded))
    .expect_that("an acquire was refused for quota", refused_for_quota)
    .expect_that("both foreign machines still run", foreign_still_run)
    .witness("acquire_refused", |c| c.get(Counter::AcquireRefused) >= 1)
    .settle_within(SETTLE)
}

/// The cloud preempts the first machine at 300 s, while it runs its whole-host run of
/// 600 s; a second run is asked for at 400 s. The machine's agent is killed with it; the
/// first run is never run again elsewhere (`at-most-one-launch`), no host is booked twice
/// (`no-double-booking`), and the client hears one terminal answer for each run
/// (`one-terminal-answer`), the first giving up at the latest. The second run is covered by
/// a new machine and succeeds: two machines in all. How the first run ends is not asserted.
pub fn a_preempted_machine() -> Scenario {
    on(
        Scenario::new("a_preempted_machine_ends_its_runs_and_double_books_nothing"),
        pool(2),
    )
    .work(1, Work::whole_host(secs(600)).give_up_after(secs(1_200)))
    .at(secs(300), preempt_machine(0))
    .at(secs(400), submit_work(1, Work::whole_host(RUN)))
    .expect(Expect::machines_launched(POOL, 2))
    .expect(Expect::all_items_terminal())
    .expect_that(
        "the run asked for after the preemption succeeded",
        second_succeeded,
    )
    .witness("machine_preempted", |c| {
        c.get(Counter::MachinePreempted) >= 1
    })
    .settle_within(SETTLE)
}

/// The cloud's list lags the truth by 60 s (`Knobs::list_lag`), so for its first minute a
/// launched machine is in no list. The scaler takes the machine from the acquire's answer,
/// not from a list: one machine for the one run, launched by 100 s and never another, and
/// it joins and the run succeeds.
pub fn a_lagging_list() -> Scenario {
    let knobs = Knobs {
        list_lag: secs(60),
        ..Knobs::default()
    };
    on(
        Scenario::new("a_slow_provider_list_does_not_launch_a_second_machine_for_the_same_demand"),
        pool(4),
    )
    .at(Duration::ZERO, set_knobs(knobs))
    .work(1, Work::whole_host(RUN))
    .expect_at(secs(100), Expect::machines_launched(POOL, 1))
    .expect(Expect::machines_launched(POOL, 1))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .expect_that("every machine launched joined", every_machine_joined)
    .settle_within(SETTLE)
}

/// The negative case: machines that never say hello, one whole-host run that gives up at
/// 1,200 s, and, when `refused`, every release refused for good
/// (`script_provider_forever(Release, Refuse(Denied))`). The scaler gives the first
/// machine up at its boot timeout and asks the cloud to release it; refused, the machine
/// still runs at the end with no work and no host to drain, which
/// `launched-machines-settle` must name. Not `refused` (its twin, named with `-released`),
/// it is released and the seed passes.
pub fn a_refused_release(refused: bool) -> Scenario {
    let name = "a_cloud_whose_release_is_refused_forever_fails_launched_machines_settle";
    let (name, release) = if refused {
        (name.to_owned(), Step::Refuse(RefusalCode::Denied))
    } else {
        (format!("{name}-released"), Step::Pass)
    };
    on(Scenario::new(&name), pool(2))
        .at(Duration::ZERO, machines_never_join(POOL, true))
        .at(
            Duration::ZERO,
            script_provider_forever(Call::Release, release),
        )
        .work(1, Work::whole_host(RUN).give_up_after(secs(1_200)))
        .settle_within(secs(1_800))
}

/// Whether every machine the cloud launched for the scheduler joined (the server's listing
/// showed its host): the machines that never did, by number, when any did not.
pub fn every_machine_joined(cell: &mut Cell) -> Option<String> {
    let Some(cloud) = cell.observations().cloud.as_ref() else {
        return Some("the cell has no elastic pool".to_owned());
    };
    let silent = cloud.machines.values().filter(|machine| !machine.joined);
    let silent: Vec<String> = silent.map(|m| format!("m{:03}", m.number)).collect();
    (!silent.is_empty()).then(|| format!("{} never joined", silent.join(", ")))
}

/// The acquires the cloud's recording wrapper holds, by their answers.
fn acquire_answers(cell: &Cell) -> Vec<js_sim_cell::AcquireAnswer> {
    let calls = cell
        .cloud()
        .map(js_sim_cell::Cloud::calls)
        .unwrap_or_default();
    let answers = calls.into_iter().filter_map(|call| match call.event {
        CloudEvent::Acquire { answer, .. } => Some(answer),
        _ => None,
    });
    answers.collect()
}

/// Whether the cloud got exactly two acquires, the first unanswered and the second
/// answered with a machine: what it got, when it did not.
pub fn retried_once(cell: &mut Cell) -> Option<String> {
    let answers = acquire_answers(cell);
    let lost_then_launched = matches!(
        answers.as_slice(),
        [
            js_sim_cell::AcquireAnswer::Unknown,
            js_sim_cell::AcquireAnswer::Accepted { .. }
        ]
    );
    (!lost_then_launched).then(|| format!("{} acquires: {answers:?}", answers.len()))
}

/// Whether the pool's brake never read tripped, at any reading the cell recorded.
pub fn never_tripped(cell: &mut Cell) -> Option<String> {
    let calls = cell
        .cloud()
        .map(js_sim_cell::Cloud::calls)
        .unwrap_or_default();
    let tripped = CloudEvent::Brake { tripped: true };
    let at = calls.iter().find(|call| call.event == tripped);
    at.map(|call| format!("it read tripped at {} ms", call.at.as_millis()))
}

/// Whether some acquire was refused for quota.
pub fn refused_for_quota(cell: &mut Cell) -> Option<String> {
    let quota = js_sim_cell::AcquireAnswer::Refused(RefusalCode::Quota);
    let answers = acquire_answers(cell);
    (!answers.contains(&quota)).then(|| format!("the acquires were answered {answers:?}"))
}

/// Whether the cloud still runs both foreign machines: what it runs that the scheduler did
/// not launch, when that is not two.
pub fn foreign_still_run(cell: &mut Cell) -> Option<String> {
    let Some(cloud) = cell.cloud() else {
        return Some("the cell has no elastic pool".to_owned());
    };
    let running = cloud.fake().running().len();
    let ours = cell
        .observations()
        .cloud
        .as_ref()
        .map_or(0, |c| c.running.len());
    let foreign = running.saturating_sub(ours);
    (foreign != 2).then(|| format!("it runs {foreign} foreign machines"))
}

/// Whether the second run asked for (item 1) succeeded: how it ended, when it did not.
pub fn second_succeeded(cell: &mut Cell) -> Option<String> {
    let record = cell.observations().items.get(&ItemId(1));
    let heard = record.into_iter().flat_map(|record| record.heard.iter());
    let ended = heard
        .filter_map(|heard| match &heard.event {
            ClientEvent::Terminal { how } => Some(*how),
            _ => None,
        })
        .next();
    (ended != Some(HowEnded::Succeeded)).then(|| format!("it ended {ended:?}"))
}
