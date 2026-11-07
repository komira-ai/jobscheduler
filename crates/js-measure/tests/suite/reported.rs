//! What a job reports about itself: the result line it prints, the log tail the harness
//! reads it from, and the numbers that reach the record.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io;

use js_analysis::{Options, analyze, parse_jsonl};
use js_measure::farm::Phase;
use js_measure::measure::{HostReading, Identity, assemble};
use js_measure::reported::{PREFIX, TAG, merge, parse, prefixed};
use js_measure::{
    Clock, Experiment, FakeClock, FakeFarm, Farm, FarmError, HostShape, JobAsk, JobStanding,
    JobTiming, LogTail, NoSetter, Observation, Plan, RunOutput, UsageRow, Workload, drive,
    run_plan,
};

use crate::stub::{Stub, standing};
use crate::support::{GIB, T0, job};

fn report(pairs: &[(&str, Option<f64>)]) -> BTreeMap<String, Option<f64>> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

fn tail(stdout: &str, stderr: &str, truncated: bool) -> LogTail {
    LogTail {
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
        truncated,
    }
}

// ---- the line -------------------------------------------------------------------

/// Asserts the tag the job prints and the prefix a key is recorded under, as documented.
#[test]
fn the_tag_and_the_prefix_are_the_documented_ones() {
    assert_eq!(TAG, "js-measure-result");
    assert_eq!(PREFIX, "reported_");
}

/// Asserts that a result line is read into its numeric keys from among other output, and
/// that a `null` key is absent. Catches a parser that reads nothing, or that turns a null
/// into zero or keeps it as a key.
#[test]
fn a_result_line_among_other_output_is_read() {
    let out = "starting\njs-measure-result {\"steady_ms\":12.5,\"first_run_ms\":45,\"warm\":null}\ndone\n";
    assert_eq!(
        parse(out),
        report(&[("steady_ms", Some(12.5)), ("first_run_ms", Some(45.0)),])
    );
}

/// Asserts that output with no result line reports nothing, whether it is empty, has
/// other lines, or has the tag only inside another line or without its space.
#[test]
fn no_result_line_reports_nothing() {
    for out in [
        "",
        "hello\nworld\n",
        "note: js-measure-result {\"a\":1}\n",
        "js-measure-result{\"a\":1}\n",
        "js-measure-resulted {\"a\":1}\n",
    ] {
        assert_eq!(parse(out), BTreeMap::new(), "{out:?}");
    }
}

/// Asserts the rule for two result lines: the last one wins whole, and its keys are not
/// mixed with the first's. Catches a first-wins parser and a parser that merges lines.
#[test]
fn the_last_of_two_result_lines_wins() {
    let out = "js-measure-result {\"a\":1,\"only_first\":2}\njs-measure-result {\"a\":3}\n";
    assert_eq!(parse(out), report(&[("a", Some(3.0))]));
}

/// Asserts that a malformed last line reports nothing and is not replaced by an earlier
/// good line: broken JSON, an array, a bare number, a string, and a line cut short.
#[test]
fn a_malformed_last_line_reports_nothing() {
    for bad in [
        "js-measure-result {\"a\":",
        "js-measure-result {\"a\":1,}",
        "js-measure-result [1,2]",
        "js-measure-result 7",
        "js-measure-result \"x\"",
        "js-measure-result ",
        "js-measure-result {\"a\":1} trailing",
    ] {
        let out = format!("js-measure-result {{\"a\":1}}\n{bad}\n");
        assert_eq!(parse(&out), BTreeMap::new(), "{bad}");
    }
}

/// Asserts that a value that is not a number leaves its key out and does not spoil its
/// neighbours: a string (even a numeric one), a boolean, an array, an object and null.
#[test]
fn a_value_that_is_not_a_number_is_absent() {
    let out = "js-measure-result {\"n\":1.5,\"s\":\"7\",\"b\":true,\"a\":[1],\"o\":{\"x\":1},\"z\":null,\"neg\":-2}";
    assert_eq!(
        parse(out),
        report(&[("n", Some(1.5)), ("neg", Some(-2.0)),])
    );
}

/// Asserts that a tail cut inside the line (the front of the line, tag included, is
/// gone) reports nothing, since no complete line is left to read.
#[test]
fn a_tail_that_cuts_the_line_reports_nothing() {
    assert_eq!(parse("sult {\"a\":1}\n"), BTreeMap::new());
    assert_eq!(parse("measure-result {\"a\":1}"), BTreeMap::new());
}

/// Asserts that a line without a final newline and one with a carriage return are read.
#[test]
fn line_endings_do_not_matter() {
    assert_eq!(
        parse("js-measure-result {\"a\":1}"),
        report(&[("a", Some(1.0))])
    );
    assert_eq!(
        parse("x\r\njs-measure-result {\"a\":2}\r\n"),
        report(&[("a", Some(2.0))])
    );
}

// ---- the names ------------------------------------------------------------------

/// Asserts that every reported key is recorded under the prefix, values kept as they are.
#[test]
fn keys_are_prefixed() {
    assert_eq!(
        prefixed(&report(&[("steady_ms", Some(1.5)), ("warm_ms", Some(2.5))])),
        report(&[
            ("reported_steady_ms", Some(1.5)),
            ("reported_warm_ms", Some(2.5))
        ])
    );
}

/// Asserts the merge of several jobs: one job's numbers are copied exactly, two jobs'
/// are averaged, and a key some job lacks is absent (not that job's number alone, not
/// null). No key, no metric.
#[test]
fn jobs_merge_by_mean_and_a_gap_is_absent() {
    let one = [report(&[("a", Some(0.1))])];
    assert_eq!(merge(&one), report(&[("reported_a", Some(0.1))]));
    let two = [
        report(&[("a", Some(2.0)), ("b", Some(1.0)), ("c", Some(5.0))]),
        report(&[("a", Some(4.0)), ("b", Some(3.0))]),
    ];
    assert_eq!(
        merge(&two),
        report(&[("reported_a", Some(3.0)), ("reported_b", Some(2.0))])
    );
    assert_eq!(merge(&[]), BTreeMap::new());
    assert_eq!(merge(&[BTreeMap::new(), BTreeMap::new()]), BTreeMap::new());
}

/// Asserts that a mean that overflows is none, not infinity.
#[test]
fn a_mean_that_overflows_is_none() {
    let jobs = [
        report(&[("a", Some(f64::MAX))]),
        report(&[("a", Some(f64::MAX))]),
    ];
    assert_eq!(merge(&jobs), report(&[("reported_a", None)]));
}

// ---- the farm and the drive ------------------------------------------------------

fn exited(log_tail: Option<LogTail>) -> JobStanding {
    JobStanding {
        outcome: Some("succeeded".to_owned()),
        log_tail,
        ..standing("JOB_STATE_SUCCEEDED", Some("h1"))
    }
}

/// Asserts that the drive keeps the tail the poll that saw the job over returned, and none
/// when the server had none.
#[test]
fn the_drive_keeps_the_tail_of_a_job_that_is_over() {
    let clock = FakeClock::at(T0);
    let held = tail("js-measure-result {\"a\":1}\n", "", false);
    let farm = Stub {
        job: Ok(exited(Some(held.clone()))),
        ..Stub::running()
    };
    let obs = drive(&farm, &clock, &[job("a", 1)], 10, 1_000);
    assert_eq!(obs.jobs[0].log_tail, Some(held));
    let bare = Stub {
        job: Ok(exited(None)),
        ..Stub::running()
    };
    let obs = drive(&bare, &clock, &[job("a", 1)], 10, 1_000);
    assert_eq!(obs.jobs[0].log_tail, None);
}

/// Asserts that a job still running has no tail in the observation, whatever the server
/// sent with it.
#[test]
fn a_job_still_running_keeps_no_tail() {
    let clock = FakeClock::at(T0);
    let farm = Stub {
        job: Ok(JobStanding {
            log_tail: Some(tail("js-measure-result {\"a\":1}", "", false)),
            ..standing("JOB_STATE_RUNNING", Some("h1"))
        }),
        ..Stub::running()
    };
    let obs = drive(&farm, &clock, &[job("a", 1)], 10, 100);
    assert_eq!(obs.jobs[0].log_tail, None);
    assert_eq!(obs.jobs[0].failure.as_deref(), Some("deadline"));
}

fn fake_with(clock: &FakeClock, tail: Option<LogTail>) -> FakeFarm<'_> {
    let shape = HostShape {
        cpu_millis: 32_000,
        memory_bytes: 128 * GIB,
    };
    let farm = FakeFarm::new(clock, vec![("h1".to_owned(), shape)]);
    match tail {
        Some(tail) => farm.with_log_tail(tail),
        None => farm,
    }
}

fn ask() -> JobAsk {
    job("a", 5_000).ask
}

/// Asserts the fake farm's log: a job that is running holds none, an ended job holds the
/// tail it was given, and a farm given none holds an empty tail.
#[test]
fn the_fake_farm_holds_a_log_only_for_an_ended_job() {
    let clock = FakeClock::at(T0);
    let given = tail("out\n", "err\n", true);
    let farm = fake_with(&clock, Some(given.clone()));
    let id = farm.submit(&ask()).unwrap();
    let live = farm.job(&id).unwrap();
    assert_eq!((live.phase(), live.log_tail), (Phase::Running, None));
    clock.sleep_ms(5_000);
    let over = farm.job(&id).unwrap();
    assert_eq!((over.phase(), over.log_tail), (Phase::Over, Some(given)));

    let clock = FakeClock::at(T0);
    let farm = fake_with(&clock, None);
    let id = farm.submit(&ask()).unwrap();
    clock.sleep_ms(5_000);
    assert_eq!(
        farm.job(&id).unwrap().log_tail,
        Some(LogTail::default()),
        "an ended job without output holds an empty tail"
    );
}

// ---- the record ------------------------------------------------------------------

fn who() -> Identity {
    Identity {
        setting: "tpch/q6-sf1".to_owned(),
        arm: "shared".to_owned(),
        run_index: 4,
        seed: 1,
        commit: "c0".to_owned(),
        label: String::new(),
    }
}

fn timing(name: &str, log_tail: Option<LogTail>, outcome: &str) -> JobTiming {
    JobTiming {
        name: name.to_owned(),
        submit_at_ms: T0,
        requested_ms: Some(T0),
        placed_ms: Some(T0),
        started_ms: Some(T0),
        finished_ms: Some(T0 + 60_000),
        host: Some("h1".to_owned()),
        outcome: Some(outcome.to_owned()),
        failure: None,
        log_tail,
    }
}

fn assembled(tails: Vec<Option<LogTail>>) -> RunOutput {
    assembled_with(tails.into_iter().map(|tail| (tail, "succeeded")).collect())
}

fn assembled_with(tails: Vec<(Option<LogTail>, &str)>) -> RunOutput {
    let jobs: Vec<_> = tails
        .iter()
        .enumerate()
        .map(|(n, _)| job(&format!("j{n}"), 60_000))
        .collect();
    let obs = Observation {
        t0_ms: T0,
        end_ms: T0 + 60_000,
        jobs: tails
            .into_iter()
            .enumerate()
            .map(|(n, (tail, outcome))| timing(&format!("j{n}"), tail, outcome))
            .collect(),
    };
    let hosts = BTreeMap::from([(
        "h1".to_owned(),
        HostReading {
            shape: Some(HostShape {
                cpu_millis: 32_000,
                memory_bytes: 128 * GIB,
            }),
            rows: Some(vec![UsageRow {
                minute: 10,
                cpu_millis: 16_000,
                ws_bytes: 4 * GIB,
                leases: 1,
            }]),
        },
    )]);
    assemble(&who(), &obs, &jobs, &hosts)
}

fn reported_of(out: &RunOutput) -> BTreeMap<String, Option<f64>> {
    out.record
        .metrics
        .iter()
        .filter(|(key, _)| key.starts_with(PREFIX))
        .map(|(key, value)| (key.clone(), *value))
        .collect()
}

/// Asserts that the names and numbers a job printed are copied into the run's record
/// under the prefix, and into the job's own row, and that the metrics the harness
/// measured are still there.
#[test]
fn the_reported_names_are_copied_into_the_record() {
    let line = "warming\njs-measure-result {\"steady_ms\":12.3,\"first_run_ms\":45.6}\n";
    let out = assembled(vec![Some(tail(line, "", false))]);
    assert_eq!(
        reported_of(&out),
        report(&[
            ("reported_steady_ms", Some(12.3)),
            ("reported_first_run_ms", Some(45.6))
        ])
    );
    assert_eq!(out.jobs[0].metrics["reported_steady_ms"], Some(12.3));
    assert_eq!(out.jobs[0].metrics["reported_first_run_ms"], Some(45.6));
    assert_eq!(out.record.metrics["makespan_ms"], Some(60_000.0));
    assert_eq!(out.record.metrics["jobs"], Some(1.0));
}

/// Asserts that a key named like a metric the harness measures cannot overwrite it: the
/// measured number stays, and the job's number is recorded under the prefix.
#[test]
fn a_reported_key_cannot_replace_a_measured_one() {
    let line = "js-measure-result {\"makespan_ms\":1,\"jobs\":99,\"submit_to_placed_ms_mean\":2}\n";
    let out = assembled(vec![Some(tail(line, "", false))]);
    assert_eq!(out.record.metrics["makespan_ms"], Some(60_000.0));
    assert_eq!(out.record.metrics["jobs"], Some(1.0));
    assert_eq!(out.record.metrics["submit_to_placed_ms_mean"], Some(0.0));
    assert_eq!(out.record.metrics["reported_makespan_ms"], Some(1.0));
    assert_eq!(out.record.metrics["reported_jobs"], Some(99.0));
    assert_eq!(
        out.record.metrics["reported_submit_to_placed_ms_mean"],
        Some(2.0)
    );
}

/// Asserts that a job with no tail, no line, a malformed line or a tail cut inside the
/// line adds no key to the record, so the record has exactly the keys of a run that
/// reports nothing, and no zero is written.
#[test]
fn a_job_that_reports_nothing_adds_no_key() {
    let quiet = assembled(vec![None]);
    assert_eq!(reported_of(&quiet), BTreeMap::new());
    let plain: Vec<&String> = quiet.record.metrics.keys().collect();
    for tail in [
        tail("no result here\n", "", false),
        tail("js-measure-result {\"a\":\n", "", false),
        tail("sult {\"a\":1}\n", "", true),
        tail("", "js-measure-result {\"a\":1}\n", false),
    ] {
        let out = assembled(vec![Some(tail)]);
        assert_eq!(out.record.metrics.keys().collect::<Vec<_>>(), plain);
        assert_eq!(
            out.jobs[0].metrics.keys().count(),
            quiet.jobs[0].metrics.len()
        );
    }
}

/// Asserts that a truncated tail whose last line is whole is read: `truncated` says the
/// front is lost, not that the line is.
#[test]
fn a_truncated_tail_with_a_whole_last_line_is_read() {
    let line = "ce of output\njs-measure-result {\"a\":7}\n";
    let out = assembled(vec![Some(tail(line, "", true))]);
    assert_eq!(reported_of(&out), report(&[("reported_a", Some(7.0))]));
}

/// Asserts the record of two jobs: the mean of their numbers, and a key only one of them
/// has is absent.
#[test]
fn two_jobs_record_the_mean_and_a_gap_is_absent() {
    let a = tail("js-measure-result {\"x\":2,\"y\":1}\n", "", false);
    let b = tail("js-measure-result {\"x\":4}\n", "", false);
    let out = assembled(vec![Some(a), Some(b)]);
    assert_eq!(reported_of(&out), report(&[("reported_x", Some(3.0))]));
}

/// Asserts that a string value (a verdict) leaves no key in the record or the job's row,
/// while its numeric neighbour is recorded.
#[test]
fn a_string_value_is_not_recorded() {
    let line = "js-measure-result {\"steady_ms\":1.5,\"verdict\":\"PASS\"}\n";
    let out = assembled(vec![Some(tail(line, "", false))]);
    assert_eq!(
        reported_of(&out),
        report(&[("reported_steady_ms", Some(1.5))])
    );
    assert!(!out.jobs[0].metrics.contains_key("reported_verdict"));
}

/// Asserts that a job that did not succeed adds nothing to the run's reported mean, and
/// that the dropped report is counted in the record as `reports_dropped` (outside the
/// job's `reported_` prefix); the job's own
/// row still shows what it printed. Catches a merge that averages in a wrong job's numbers.
#[test]
fn a_failed_jobs_report_is_excluded_and_counted() {
    let good = tail("js-measure-result {\"x\":2}\n", "", false);
    let bad = tail("js-measure-result {\"x\":100}\n", "", false);
    let out = assembled_with(vec![(Some(good), "succeeded"), (Some(bad), "failed")]);
    assert_eq!(reported_of(&out), report(&[("reported_x", Some(2.0))]));
    assert_eq!(out.record.metrics["reports_dropped"], Some(1.0));
    assert_eq!(out.jobs[1].metrics["reported_x"], Some(100.0));
}

/// Asserts that when every job failed no reported key is recorded, and the count of
/// dropped reports is the number of failed jobs that printed a result line; a failed job
/// that printed none, whatever its outcome, is not counted.
#[test]
fn only_failed_jobs_that_reported_are_counted() {
    let line = "js-measure-result {\"x\":9}\n";
    let out = assembled_with(vec![
        (Some(tail(line, "", false)), "failed"),
        (Some(tail(line, "", false)), "out_of_memory"),
        (Some(tail("crashed\n", "", false)), "failed"),
        (None, "failed"),
    ]);
    assert_eq!(reported_of(&out), BTreeMap::new());
    assert_eq!(out.record.metrics["reports_dropped"], Some(2.0));
    assert_eq!(out.record.metrics["reports_missing"], Some(0.0));
}

/// Asserts that a run in which every job succeeded and reported records both harness
/// counts as 0: a measured zero, not missing data.
#[test]
fn a_clean_run_counts_zero_dropped_and_zero_missing() {
    let line = "js-measure-result {\"x\":9}\n";
    let out = assembled(vec![Some(tail(line, "", false))]);
    assert_eq!(out.record.metrics["reports_dropped"], Some(0.0));
    assert_eq!(out.record.metrics["reports_missing"], Some(0.0));
}

/// Asserts that a job that prints a key named `dropped` gets `reported_dropped` from its
/// own number, while the harness count of failed jobs' reports is `reports_dropped`,
/// separate and correct. Catches a harness count written inside the job's namespace.
#[test]
fn a_job_reporting_dropped_does_not_collide_with_the_harness_count() {
    let ok = tail("js-measure-result {\"dropped\":3}\n", "", false);
    let bad = tail("js-measure-result {\"dropped\":50}\n", "", false);
    let out = assembled_with(vec![(Some(ok), "succeeded"), (Some(bad), "failed")]);
    assert_eq!(
        reported_of(&out),
        report(&[("reported_dropped", Some(3.0))])
    );
    assert_eq!(out.record.metrics["reports_dropped"], Some(1.0));
}

/// Asserts that a job that succeeded but printed no result line is not silently left out
/// of the mean: with three succeeded jobs, one silent, no `reported_` key is recorded
/// (the silent job lacks every key) and `reports_missing` is 1. Catches a mean taken over
/// the two jobs that did report.
#[test]
fn a_silent_succeeded_job_empties_the_reported_keys_and_is_counted() {
    let line = |x: u32| tail(&format!("js-measure-result {{\"x\":{x}}}\n"), "", false);
    let out = assembled(vec![
        Some(line(2)),
        Some(tail("crashed after exit 0\n", "", false)),
        Some(line(4)),
    ]);
    assert_eq!(reported_of(&out), BTreeMap::new());
    assert_eq!(out.record.metrics["reports_missing"], Some(1.0));
    assert_eq!(out.record.metrics["reports_dropped"], Some(0.0));
}

/// Asserts that every way a succeeded job can be silent counts as missing (no tail, a cut
/// tail, output on stderr only), and that a failed job without a line is neither missing
/// nor dropped: its failure is already in `jobs_failed`.
#[test]
fn missing_counts_succeeded_jobs_without_a_line_and_not_failed_ones() {
    let line = tail("js-measure-result {\"x\":1}\n", "", false);
    let out = assembled_with(vec![
        (Some(line), "succeeded"),
        (None, "succeeded"),
        (Some(tail("sult {\"x\":1}\n", "", true)), "succeeded"),
        (
            Some(tail("", "js-measure-result {\"x\":1}\n", false)),
            "succeeded",
        ),
        (None, "failed"),
    ]);
    assert_eq!(reported_of(&out), BTreeMap::new());
    assert_eq!(out.record.metrics["reports_missing"], Some(3.0));
    assert_eq!(out.record.metrics["reports_dropped"], Some(0.0));
    assert_eq!(out.record.metrics["jobs_failed"], Some(1.0));
}

// ---- the plan ---------------------------------------------------------------------

/// A farm that records whether every job it is asked for needs the whole host, and
/// otherwise is the fake.
struct Spy<'a> {
    inner: FakeFarm<'a>,
    whole_hosts: RefCell<Vec<bool>>,
}

impl Farm for Spy<'_> {
    fn submit(&self, ask: &JobAsk) -> Result<String, FarmError> {
        self.whole_hosts.borrow_mut().push(ask.whole_host);
        self.inner.submit(ask)
    }
    fn job(&self, id: &str) -> Result<JobStanding, FarmError> {
        self.inner.job(id)
    }
    fn cancel(&self, id: &str) -> Result<(), FarmError> {
        self.inner.cancel(id)
    }
    fn host(&self, id: &str) -> Result<HostShape, FarmError> {
        self.inner.host(id)
    }
    fn usage_rows(&self, host: &str, from: u64, to: u64) -> Result<Vec<UsageRow>, FarmError> {
        self.inner.usage_rows(host, from, to)
    }
}

fn run_everything(
    plan: &Plan,
    workload: &Workload,
    farm: &dyn Farm,
    clock: &FakeClock,
) -> Vec<RunOutput> {
    let jobs = workload.expand().unwrap();
    let experiment = Experiment {
        plan,
        jobs: &jobs,
        farm,
        clock,
        setter: &NoSetter,
        commit: "abc1234",
        poll_ms: 10_000,
        deadline_ms: 36_000_000,
    };
    let mut outputs = Vec::new();
    run_plan(&experiment, &mut |run| -> io::Result<()> {
        outputs.push(run.clone());
        Ok(())
    })
    .unwrap();
    outputs
}

fn two_arm_plan() -> Plan {
    serde_json::from_str(
        r#"{"parameter":"sharing","settings":["s"],"arms":[{"label":"shared","control":true},{"label":"whole","whole_host":true}],"runs_per_arm":3,"seed":2}"#,
    )
    .unwrap()
}

fn one_job_workload(whole_host: bool) -> Workload {
    serde_json::from_str(&format!(
        r#"{{"jobs":[{{"name":"w","count":1,"image":"img","cpu_millis":1000,"memory_bytes":1000,"wall_ms":30000,"whole_host":{whole_host}}}]}}"#
    ))
    .unwrap()
}

/// Asserts that an arm's `whole_host` replaces what the workload says for every job of
/// that arm and only that arm, and that an arm without one keeps the workload's.
#[test]
fn an_arm_can_set_whole_host() {
    let plan = two_arm_plan();
    for workload_says in [false, true] {
        let workload = one_job_workload(workload_says);
        let clock = FakeClock::at(T0);
        let spy = Spy {
            inner: fake_with(&clock, None),
            whole_hosts: RefCell::new(Vec::new()),
        };
        let outputs = run_everything(&plan, &workload, &spy, &clock);
        let by_run: Vec<(String, bool)> = outputs
            .iter()
            .map(|o| o.record.arm.clone())
            .zip(spy.whole_hosts.borrow().iter().copied())
            .collect();
        assert_eq!(by_run.len(), 6);
        for (arm, whole_host) in by_run {
            let expected = arm == "whole" || workload_says;
            assert_eq!(whole_host, expected, "arm {arm}, workload {workload_says}");
        }
    }
}

/// Asserts that an arm's `whole_host` is optional in the plan file and that an unknown
/// key in an arm, such as the old `lease_class`, is still refused.
#[test]
fn whole_host_is_optional_in_an_arm() {
    let plan = two_arm_plan();
    assert_eq!(plan.arms[0].whole_host, None);
    assert_eq!(plan.arms[1].whole_host, Some(true));
    let bad = r#"{"label":"x","lease_class":"whole-node"}"#;
    assert!(serde_json::from_str::<js_measure::Arm>(bad).is_err());
}

fn read_shipped(name: &str) -> String {
    let path = format!("{}/plans/{name}", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(&path).unwrap_or_else(|why| panic!("{path}: {why}"))
}

/// Asserts the shipped whole-host noise plan: one setting (the bench cell), two arms
/// `shared` (the control) and `whole-host` (needing the whole host), 20 measured runs
/// of each, and its schedule gives each arm exactly 20 runs.
#[test]
fn the_whole_host_noise_plan_has_two_arms_of_twenty() {
    let plan: Plan = serde_json::from_str(&read_shipped("whole-host-noise.plan.json")).unwrap();
    assert_eq!(plan.settings, vec!["tpch/q6-sf1".to_owned()]);
    let arms: Vec<(&str, bool, Option<bool>)> = plan
        .arms
        .iter()
        .map(|a| (a.label.as_str(), a.control, a.whole_host))
        .collect();
    assert_eq!(
        arms,
        vec![("shared", true, None), ("whole-host", false, Some(true))]
    );
    assert_eq!(plan.runs_per_arm, 20);
    let slots = plan.schedule().unwrap();
    for arm in 0..2 {
        let n = slots.iter().filter(|s| s.arm == arm && !s.warmup).count();
        assert_eq!(n, 20);
    }
}

/// Asserts the shipped workload: one job that runs the bench runner with its real flags
/// (query q6, scale 1, the result-line opt-in), asks for a share of a host, and that the
/// plan's whole-host arm asks for the whole host.
#[test]
fn the_whole_host_noise_workload_runs_the_cell() {
    let workload: Workload =
        serde_json::from_str(&read_shipped("whole-host-noise.workload.json")).unwrap();
    let plan: Plan = serde_json::from_str(&read_shipped("whole-host-noise.plan.json")).unwrap();
    let jobs = workload.expand().unwrap();
    assert_eq!(jobs.len(), 1);
    let args = &jobs[0].ask.args;
    let value_of = |flag: &str| {
        let at = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag}: {args:?}"));
        args[at + 1].as_str()
    };
    assert_eq!(value_of("--query"), "q6");
    assert_eq!(value_of("--scale"), "sf1");
    assert_eq!(value_of("--dataset"), "tpch");
    assert_eq!(value_of("--suite"), "tpch");
    assert_eq!(args.last().map(String::as_str), Some("--js-measure-result"));
    assert!(
        !args.iter().any(|a| a == "--cell" || a == "--result-line"),
        "{args:?}"
    );
    assert!(!jobs[0].ask.whole_host);
    let whole = plan.arms.iter().find(|a| a.label == "whole-host").unwrap();
    assert_eq!(whole.whole_host, Some(true));
    assert!(
        jobs[0].ask.image.contains("@sha256:"),
        "{}",
        jobs[0].ask.image
    );
}

/// Asserts the whole path of the shipped plan on a simulated farm whose jobs print a
/// result line: 40 records, every one with the job's numbers, the analysis reads them,
/// and `--compare-cv shared whole-host` produces a comparison of 20 against 20 for the
/// reported metric.
#[test]
fn the_shipped_plan_feeds_the_analysis_compare_cv() {
    let plan: Plan = serde_json::from_str(&read_shipped("whole-host-noise.plan.json")).unwrap();
    let workload: Workload =
        serde_json::from_str(&read_shipped("whole-host-noise.workload.json")).unwrap();
    let clock = FakeClock::at(T0);
    let printed = tail(
        "js-measure-result {\"steady_ms\":12.3,\"first_run_ms\":45.6,\"duckdb_steady_ms\":10.1,\"verdict\":\"PASS\"}\n",
        "",
        false,
    );
    let farm = fake_with(&clock, Some(printed));
    let outputs = run_everything(&plan, &workload, &farm, &clock);
    assert_eq!(outputs.len(), 40);
    let m = &outputs[0].record.metrics;
    assert_eq!(m["reported_steady_ms"], Some(12.3));
    assert_eq!(m["reported_first_run_ms"], Some(45.6));
    assert_eq!(m["reported_duckdb_steady_ms"], Some(10.1));
    assert!(!m.contains_key("reported_verdict"));
    let text: String = outputs
        .iter()
        .map(|o| serde_json::to_string(&o.record).unwrap() + "\n")
        .collect();
    let options = Options {
        cv_pairs: vec![("shared".to_owned(), "whole-host".to_owned())],
        ..Options::default()
    };
    let report = analyze(parse_jsonl(&text).unwrap(), &options).unwrap();
    let steady: Vec<_> = report
        .cv_comparisons
        .iter()
        .filter(|c| c.metric == "reported_steady_ms")
        .collect();
    assert_eq!(steady.len(), 1);
    assert_eq!((steady[0].n_base, steady[0].n_arm), (20, 20));
    assert_eq!(
        (steady[0].base.as_str(), steady[0].arm.as_str()),
        ("shared", "whole-host")
    );
}
