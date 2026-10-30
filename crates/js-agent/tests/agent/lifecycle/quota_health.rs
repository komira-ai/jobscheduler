//! The health of the scratch quota in the lifecycle, and the bound on waiting for a finished
//! run's usage: the check that follows failed calls, what an unhealthy quota refuses, the
//! slot that is parked when a usage never reads zero, and the reports that repeat. The
//! order of the calls and the rest of the quota's rules are in `quota`.

use std::fs;

use js_agent::lifecycle::RunQuota;
use js_agent::quota::Usage;

use super::quota::{DEFAULT_BYTES, INODES, project, with_quota};
use super::{job, names};

const GIB: u64 = 1 << 30;

/// Asserts the quota's health: calls that fail in a row (`apply` of the runs here) do not
/// stop the agent while the check passes (the probe says project and a trial bound works:
/// the counter starts again); once the probe says none, the next run of failures makes the
/// quota unhealthy at the third failure, reported once, and every start is then refused
/// `quota-unavailable` (not `no-capacity`: the server counts it against no run) with
/// nothing created; the report
/// comes again after a minute, not before; and when the probe says project again a step
/// reports the recovery and runs start again. It catches a host that keeps taking runs it
/// cannot bound, one that never takes runs again, and a probe on every failure.
#[test]
fn a_quota_that_fails_in_a_row_and_fails_its_check_stops_taking_runs_until_it_recovers() {
    let (mut h, spy) = with_quota("quota-health");
    spy.set_fail_runs_apply(true);
    for run in ["w1", "w2", "w3", "w4"] {
        assert_eq!(
            names(&h.start(job(run))),
            [
                format!("finding {run} scratch-failed"),
                format!("result {run}")
            ],
            "a working probe: only the run fails"
        );
    }
    h.life.step();
    spy.set_probe_none(Some("remounted without prjquota"));
    h.start(job("a"));
    h.start(job("b"));
    assert_eq!(
        names(&h.start(job("c"))),
        [
            "finding - quota-unhealthy",
            "finding c scratch-failed",
            "result c"
        ]
    );
    assert_eq!(names(&h.start(job("d"))), ["refused d quota-unavailable"]);
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("d").exists());
    assert_eq!(
        h.life.results().last().unwrap().detail.as_deref(),
        Some("quota-unavailable")
    );
    assert!(!h.life.is_tainted());

    h.life.step();
    h.clock.advance(59_999);
    assert!(!names(&h.life.step()).contains(&"finding - quota-unhealthy".to_owned()));
    h.clock.advance(1);
    assert_eq!(names(&h.life.step()), ["finding - quota-unhealthy"]);

    spy.set_probe_none(None);
    spy.set_fail_runs_apply(false);
    h.clock.advance(60_000);
    assert_eq!(names(&h.life.step()), ["finding - quota-recovered"]);
    assert_eq!(names(&h.start(job("e"))), ["started e"]);
}

/// Asserts that failed `clear` calls count toward the health as `apply` does: three steps
/// that fail to clear, on a probe that says none, make the quota unhealthy at the third.
#[test]
fn clear_failures_in_a_row_make_the_quota_unhealthy() {
    let (mut h, spy) = with_quota("quota-health-clear");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    spy.inner.set_fail_clear(true);
    spy.set_probe_none(Some("remounted without prjquota"));
    h.life.step();
    assert!(!names(&h.life.step()).contains(&"finding - quota-unhealthy".to_owned()));
    assert!(names(&h.life.step()).contains(&"finding - quota-unhealthy".to_owned()));
    // Another failure while unhealthy counts but does not probe or report again.
    assert!(
        names(&h.life.step())
            .iter()
            .all(|line| !line.contains("quota-unhealthy"))
    );
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 quota-unavailable"]);
}

/// Asserts that slots that stay parked are reported again after an hour and not before. It
/// catches a slot that is lost to the operator after one line in a log.
#[test]
fn a_slot_that_stays_parked_is_reported_again_every_hour() {
    let (mut h, spy) = with_quota("quota-remind");
    spy.inner.charge(
        project(0),
        Usage {
            bytes: 4096,
            inodes: 2,
        },
    );
    // A wall limit above the hours the test lets pass, so the run is not timed out.
    let mut long = job("r1");
    long.limits.wall_ms = 10_000_000;
    h.start(long);
    h.life.step();
    h.clock.advance(3_599_999);
    assert!(names(&h.life.step()).is_empty());
    h.clock.advance(1);
    assert_eq!(names(&h.life.step()), ["finding - quota-slot-busy"]);
    assert!(names(&h.life.step()).is_empty(), "once an hour");
}

/// Asserts that the check exercises what failed and not only the probe: with a probe that
/// says project but a bound that cannot be set (the trial bound fails as the runs' do), the
/// third failure in a row makes the quota unhealthy, and the next start is refused
/// `quota-unavailable`; the trial directory is not left behind. It catches a host whose
/// probe passes while every `apply` fails, which would never leave the failing state.
#[test]
fn a_probe_that_passes_while_the_bound_cannot_be_set_is_still_unhealthy() {
    let (mut h, spy) = with_quota("quota-health-trial");
    spy.inner.set_fail_apply(true);
    h.start(job("a"));
    h.start(job("b"));
    let third = names(&h.start(job("c")));
    assert_eq!(third[0], "finding - quota-unhealthy", "{third:?}");
    assert_eq!(names(&h.start(job("d"))), ["refused d quota-unavailable"]);
    assert!(!h.scratch_of(".js-health").exists());
}

/// Asserts the bound on holding a slot for a usage that never reads zero (a file held open
/// outside the container, accounting that drifted): the finished run says so once, an
/// immediate next run is not delayed, and after the wait (10 minutes) the cleanup goes on
/// (scratch removed, checks passed, the run freed) with the slot parked instead of
/// released, a finding of its own, the booked bytes given back, and one failure counted
/// toward the quota's health. The parked slot is skipped by the next run, reported again
/// after an hour, and released when its usage finally reads zero. It catches a job that
/// holds its slot, its bytes and its place for good.
#[test]
fn a_usage_that_never_reads_zero_parks_the_slot_after_the_wait() {
    let (mut h, spy) = with_quota("quota-abandon");
    spy.set_lag(u32::MAX);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    assert!(names(&h.life.step()).contains(&"finding r1 quota-usage-held".to_owned()));
    assert_eq!(h.life.free().disk_bytes, 8 * GIB);
    h.clock.advance(599_999);
    assert!(names(&h.life.step()).is_empty(), "still within the wait");
    assert_eq!(h.life.busy_slots(), [0]);
    h.clock.advance(1);
    assert_eq!(
        names(&h.life.step()),
        [
            "finding r1 quota-usage-abandoned",
            "scratch removed r1",
            "checks passed r1",
            "freed r1"
        ]
    );
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(h.life.parked_slots(), [0]);
    assert_eq!(
        h.life.free().disk_bytes,
        10 * GIB,
        "bytes and place given back"
    );
    assert!(!h.life.is_tainted());

    let mut long = job("r2");
    long.limits.wall_ms = 10_000_000;
    assert_eq!(names(&h.start(long)), ["started r2"]);
    assert_eq!(h.fake.created()[1].slot, 1, "the parked slot is skipped");
    h.clock.advance(3_600_000);
    assert_eq!(
        names(&h.life.step()),
        ["finding - quota-slot-busy"],
        "hourly"
    );

    spy.end_lag();
    assert_eq!(names(&h.life.step()), ["finding - quota-slot-released"]);
    assert!(h.life.parked_slots().is_empty());
}

/// Asserts that giving up counts one failure toward the quota's health: with the limit of
/// failures at one and a probe that says none, the abandoned wait makes the quota
/// unhealthy. It catches a give-up that is invisible to the health rule.
#[test]
fn giving_up_on_a_usage_counts_toward_the_quota_health() {
    let (mut h, spy) = with_quota("quota-abandon-health");
    h.config.quota = Some(
        RunQuota::new(spy.clone(), DEFAULT_BYTES, INODES)
            .with_max_failures(1)
            .with_usage_wait_ms(1000),
    );
    h.restart().unwrap();
    spy.set_lag(u32::MAX);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    spy.set_probe_none(Some("remounted without prjquota"));
    h.clock.advance(1000);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"finding - quota-unhealthy".to_owned()),
        "{events:?}"
    );
}

/// Asserts the hourly report while the agent waits for a usage (a wait longer than an
/// hour): the finding comes at the first step, again after each hour and not between, and
/// the give-up comes at the end of the wait. It catches a report made once and then silence.
#[test]
fn a_usage_that_stays_nonzero_is_reported_every_hour_while_waited_for() {
    let (mut h, spy) = with_quota("quota-held-hourly");
    h.config.quota =
        Some(RunQuota::new(spy.clone(), DEFAULT_BYTES, INODES).with_usage_wait_ms(3 * 3_600_000));
    h.restart().unwrap();
    spy.set_lag(u32::MAX);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let held = "finding r1 quota-usage-held".to_owned();
    assert!(names(&h.life.step()).contains(&held));
    h.clock.advance(3_599_999);
    assert!(names(&h.life.step()).is_empty());
    h.clock.advance(1);
    assert_eq!(names(&h.life.step()), std::slice::from_ref(&held));
    h.clock.advance(3_600_000);
    assert_eq!(names(&h.life.step()), std::slice::from_ref(&held));
    h.clock.advance(3_600_000);
    assert!(names(&h.life.step()).contains(&"finding r1 quota-usage-abandoned".to_owned()));
}

/// Asserts that the check names what stops it: a regular file where its trial directory
/// belongs, and a `clear` that fails with a probe that says project, each make the quota
/// unhealthy at the third failure in a row, with the cause in the report. It catches a check
/// that passes while the trial cannot be made or the bound cannot be taken off.
#[test]
fn the_check_reports_a_trial_that_cannot_be_made_or_cleared() {
    let (mut h, spy) = with_quota("quota-health-causes");
    fs::write(h.scratch_of(".js-health"), "in the way").unwrap();
    spy.inner.set_fail_apply(true);
    h.start(job("a"));
    h.start(job("b"));
    let events = h.start(job("c"));
    let js_agent::lifecycle::Event::Finding(found) = &events[0] else {
        panic!("not a finding: {events:?}");
    };
    assert!(
        found.message.contains("cannot make the trial directory"),
        "{}",
        found.message
    );

    let (mut h, spy) = with_quota("quota-health-clear-cause");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    spy.inner.set_fail_clear(true);
    h.life.step();
    h.life.step();
    let events = h.life.step();
    let reported: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            js_agent::lifecycle::Event::Finding(found) if found.code == "quota-unhealthy" => {
                Some(found.message.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(reported.len(), 1, "{events:?}");
    assert!(
        reported[0].contains("clearing the trial bound failed"),
        "{}",
        reported[0]
    );
}
