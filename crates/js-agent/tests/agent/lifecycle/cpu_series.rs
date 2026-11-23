//! A job's CPU second by second (`cpu_series`), as the runtime recorded it: it is read with
//! the job's usage when the job has ended, listed in its result, and counted in what the
//! result weighs when the log tails are cut to fit a heartbeat.

use js_agent_wire::{CpuSeries, LogTail};

use super::Harness;
use super::per_job::{job_start, listing, start};

/// A series of 128 points of one core, as long as a series gets.
fn series() -> CpuSeries {
    CpuSeries {
        granularity_secs: 2,
        points: vec![16; 128],
        run_usec: 256_000_000,
        partial: false,
    }
}

/// Proves what the runtime recorded of a job's CPU reaches the job's listed result, and only
/// that job's: a job whose runtime recorded a series lists it as `cpu_series`, and a job whose
/// runtime recorded none lists none. Catches a result that leaves the series out, one that
/// invents an empty series for a job that has none, and the series of one job on another's
/// result.
#[test]
fn a_jobs_cpu_series_reaches_its_result_and_an_unrecorded_one_is_absent() {
    let mut h = Harness::new("job-cpu-series");
    h.apply(vec![job_start(start("j1")), job_start(start("j2"))]);
    h.fake.report_cpu_series("j1", series());
    h.fake.exit("j1", 0);
    h.fake.exit("j2", 0);
    h.life.step();
    let result = |job_id: &str| listing(&h, job_id).result.expect("a result");
    assert_eq!(result("j1").cpu_series, Some(series()));
    assert_eq!(result("j2").cpu_series, None);
}

/// Proves a series counts in the weight of the result when the log tails are cut to fit a
/// heartbeat: a budget that holds a job's result with its tail whole, series left out, cuts
/// the tail of the same job once its series is in. Catches a result whose series is sent
/// but not weighed, which would send a heartbeat larger than its budget.
#[test]
fn a_cpu_series_is_weighed_with_its_result() {
    let tail = LogTail {
        stdout: "x".repeat(2000),
        ..LogTail::default()
    };
    let cut = |name: &str, with_series: bool, budget: Option<usize>| {
        let mut h = Harness::new(name);
        h.apply(vec![job_start(start("j1"))]);
        h.fake.print("j1", tail.clone());
        if with_series {
            h.fake.report_cpu_series("j1", series());
        }
        h.fake.exit("j1", 0);
        h.life.step();
        let result = listing(&h, "j1").result.expect("a result");
        // The key that holds the result in the listing adds 10 bytes to it.
        let weight = serde_json::to_vec(&result).expect("json").len() + 10;
        h.life
            .fit_log_tails(budget.unwrap_or(weight), &mut Vec::new());
        let result = listing(&h, "j1").result.expect("a result");
        (weight, result.log_tail.expect("a tail").truncated)
    };
    let (plain, plain_cut) = cut("weigh-plain", false, None);
    assert!(!plain_cut, "a result that fits its budget keeps its tail");
    let (_, series_cut) = cut("weigh-series", true, Some(plain));
    assert!(series_cut, "the series is part of the weight");
}
