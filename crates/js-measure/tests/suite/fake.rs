//! The simulated farm: placement, jobs and the usage store it shows.

use js_measure::farm::Phase;
use js_measure::{Clock, FakeClock, FakeFarm, Farm, FarmError, HostShape};

use crate::support::{GIB, T0, job};

fn farm(clock: &FakeClock, hosts: usize) -> FakeFarm<'_> {
    let shape = HostShape {
        cpu_millis: 32_000,
        memory_bytes: 128 * GIB,
    };
    let hosts = (1..=hosts).map(|n| (format!("h{n}"), shape)).collect();
    FakeFarm::new(clock, hosts)
}

fn ask(wall_ms: u64) -> js_measure::JobAsk {
    job("x", wall_ms).ask
}

fn refusal(result: Result<impl std::fmt::Debug, FarmError>) -> (u16, String) {
    match result {
        Err(FarmError::Refused { status, code }) => (status, code),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn host_of(farm: &FakeFarm<'_>, id: &str) -> Option<String> {
    farm.job(id).unwrap().host
}

/// Asserts placement: whole hosts to jobs in submission order, a job with no host free
/// queued until one frees, a cancelled job gone from its host (or never given one), and an unknown job refused.
#[test]
fn jobs_take_whole_hosts_in_order() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 2);
    let first = farm.submit(&ask(60_000)).unwrap();
    let second = farm.submit(&ask(60_000)).unwrap();
    let third = farm.submit(&ask(60_000)).unwrap();
    assert_eq!(
        (first.as_str(), second.as_str(), third.as_str()),
        ("job-1", "job-2", "job-3")
    );
    assert_eq!(host_of(&farm, &first).as_deref(), Some("h1"));
    assert_eq!(host_of(&farm, &second).as_deref(), Some("h2"));
    let queued = farm.job(&third).unwrap();
    assert_eq!((queued.phase(), queued.host), (Phase::Waiting, None));
    farm.cancel(&second).unwrap();
    let cancelled = farm.job(&second).unwrap();
    assert_eq!(
        (cancelled.phase(), cancelled.outcome.as_deref()),
        (Phase::Over, Some("cancelled"))
    );
    assert_eq!(host_of(&farm, &third).as_deref(), Some("h2"));
    let fourth = farm.submit(&ask(60_000)).unwrap();
    farm.cancel(&fourth).unwrap();
    let never = farm.job(&fourth).unwrap();
    assert_eq!(
        (never.outcome.as_deref(), never.host),
        (Some("cancelled"), None)
    );
    assert_eq!(refusal(farm.job("nope")), (404, "unknown-job".to_owned()));
}

/// Asserts a job lasts its wall limit and then reads as over with the farm's outcome
/// (`succeeded` unless set), a job with another outcome is failed, and the refusals have
/// the codes the live server gives: an unknown job or host is 404.
#[test]
fn a_job_lasts_its_wall_limit_and_refusals_have_codes() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 1).with_outcome("out_of_memory");
    let id = farm.submit(&ask(5_000)).unwrap();
    let live = farm.job(&id).unwrap();
    assert_eq!((live.phase(), live.outcome), (Phase::Running, None));
    clock.sleep_ms(5_000);
    let over = farm.job(&id).unwrap();
    assert_eq!(
        (over.state.as_str(), over.outcome.as_deref(), over.log_tail),
        (
            "JOB_STATE_FAILED",
            Some("out_of_memory"),
            Some(Default::default())
        )
    );
    let clock = FakeClock::at(T0);
    let farm = self::farm(&clock, 1);
    let id = farm.submit(&ask(5_000)).unwrap();
    clock.sleep_ms(5_000);
    assert_eq!(farm.job(&id).unwrap().state, "JOB_STATE_SUCCEEDED");
    assert_eq!(
        refusal(farm.cancel("job-9")),
        (404, "unknown-job".to_owned())
    );
    assert_eq!(refusal(farm.host("h9")), (404, "unknown-host".to_owned()));
    assert_eq!(farm.host("h1").unwrap().cpu_millis, 32_000);
}

/// Asserts that a cancel of a job that has ended changes nothing: its outcome stays the
/// one it ended with, and the host it held is not read as held longer.
#[test]
fn a_cancel_of_an_ended_job_changes_nothing() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 1);
    let id = farm.submit(&ask(5_000)).unwrap();
    clock.sleep_ms(5_000);
    farm.cancel(&id).unwrap();
    assert_eq!(farm.job(&id).unwrap().outcome.as_deref(), Some("succeeded"));
}

/// Asserts the usage store a job leaves: per minute, the CPU and memory the job used in
/// proportion to the part of the minute it ran, as a percent of what it asked for (50 by
/// default), and the number of jobs that ran in it; a minute with no job has no row.
#[test]
fn the_usage_store_shows_what_ran_in_each_minute() {
    let clock = FakeClock::at(T0 + 30_000);
    let farm = farm(&clock, 1);
    farm.submit(&ask(60_000)).unwrap();
    let rows = farm.usage_rows("h1", 9, 12).unwrap();
    let shape: Vec<(u64, u64, u64, u64)> = rows
        .iter()
        .map(|r| (r.minute, r.cpu_millis, r.ws_bytes, r.leases))
        .collect();
    // Asked for 4000 millicores and 8 GiB, half used, half of each of minutes 10 and 11.
    assert_eq!(
        shape,
        vec![(10, 1_000, 4 * GIB, 1), (11, 1_000, 4 * GIB, 1)]
    );
    assert!(farm.usage_rows("other", 9, 12).unwrap().is_empty());
}

/// Asserts that two jobs that ran in one minute are two in its row (so the harness will
/// not attribute the minute to either) and that the use percent is a knob.
#[test]
fn two_jobs_in_a_minute_are_two() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 1).with_use_percent(100);
    farm.submit(&ask(30_000)).unwrap();
    clock.sleep_ms(30_000);
    farm.submit(&ask(30_000)).unwrap();
    let rows = farm.usage_rows("h1", 10, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].leases, rows[0].cpu_millis, rows[0].ws_bytes),
        (2, 4_000, 8 * GIB)
    );
}

/// Asserts that a job cancelled while running stops using its host at the cancel: the
/// usage store shows only the part of the minute before it.
#[test]
fn a_cancelled_job_stops_at_the_cancel() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock, 1).with_use_percent(100);
    let id = farm.submit(&ask(600_000)).unwrap();
    clock.sleep_ms(15_000);
    farm.cancel(&id).unwrap();
    clock.sleep_ms(30_000);
    let rows = farm.usage_rows("h1", 10, 11).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].minute, rows[0].cpu_millis), (10, 1_000));
}
