//! What a job reports about itself: the result line it prints, the log tail the harness
//! reads it from, and the numbers that reach the record.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io;

use js_analysis::{Options, analyze, parse_jsonl};
use js_client::LogTail;
use js_measure::farm::{Phase, RunStanding, Standing};
use js_measure::measure::{HostReading, Identity, assemble};
use js_measure::reported::{PREFIX, TAG, merge, parse, prefixed};
use js_measure::{
    Acquired, Clock, Curve, Experiment, FakeClock, FakeFarm, Farm, FarmError, HostShape, JobTiming,
    LeaseAsk, NoSetter, Observation, Plan, RunAsk, RunOutput, UsageRow, Workload, drive, run_plan,
};

use crate::stub::Stub;
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

/// Asserts that a result line is read into its keys, numbers as numbers and `null` as
/// none, from among other output. Catches a parser that reads nothing, or that turns a
/// null into zero.
#[test]
fn a_result_line_among_other_output_is_read() {
    let out = "starting\njs-measure-result {\"steady_ms\":12.5,\"first_run_ms\":45,\"warm\":null}\ndone\n";
    assert_eq!(
        parse(out),
        report(&[
            ("steady_ms", Some(12.5)),
            ("first_run_ms", Some(45.0)),
            ("warm", None)
        ])
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

/// Asserts that a value that is not a number is none for its key and does not spoil its
/// neighbours: a string (even a numeric one), a boolean, an array and an object.
#[test]
fn a_value_that_is_not_a_number_is_none() {
    let out = "js-measure-result {\"n\":1.5,\"s\":\"7\",\"b\":true,\"a\":[1],\"o\":{\"x\":1},\"z\":null,\"neg\":-2}";
    assert_eq!(
        parse(out),
        report(&[
            ("n", Some(1.5)),
            ("s", None),
            ("b", None),
            ("a", None),
            ("o", None),
            ("z", None),
            ("neg", Some(-2.0)),
        ])
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
        prefixed(&report(&[("steady_ms", Some(1.5)), ("warm", None)])),
        report(&[("reported_steady_ms", Some(1.5)), ("reported_warm", None)])
    );
}

/// Asserts the merge of several jobs: one job's numbers are copied exactly, two jobs'
/// are averaged, a key some job lacks or reports as none is none, and a key only one job
/// reports is none (not that job's number alone). No key, no metric.
#[test]
fn jobs_merge_by_mean_and_a_gap_is_none() {
    let one = [report(&[("a", Some(0.1)), ("b", None)])];
    assert_eq!(
        merge(&one),
        report(&[("reported_a", Some(0.1)), ("reported_b", None)])
    );
    let two = [
        report(&[("a", Some(2.0)), ("b", Some(1.0)), ("c", Some(5.0))]),
        report(&[("a", Some(4.0)), ("b", None)]),
    ];
    assert_eq!(
        merge(&two),
        report(&[
            ("reported_a", Some(3.0)),
            ("reported_b", None),
            ("reported_c", None)
        ])
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

fn exited(log_tail: Option<LogTail>) -> RunStanding {
    RunStanding {
        state: "exited".to_owned(),
        outcome: Some("succeeded".to_owned()),
        log_tail,
    }
}

/// Asserts that the drive keeps the tail the poll that saw the run over returned, and none
/// when the server had none.
#[test]
fn the_drive_keeps_the_tail_of_a_run_that_is_over() {
    let clock = FakeClock::at(T0);
    let held = tail("js-measure-result {\"a\":1}\n", "", false);
    let farm = Stub {
        run: Ok(exited(Some(held.clone()))),
        ..Stub::held()
    };
    let obs = drive(&farm, &clock, &[job("a", 1, None)], 10, 1_000);
    assert_eq!(obs.jobs[0].log_tail, Some(held));
    let bare = Stub {
        run: Ok(exited(None)),
        ..Stub::held()
    };
    let obs = drive(&bare, &clock, &[job("a", 1, None)], 10, 1_000);
    assert_eq!(obs.jobs[0].log_tail, None);
}

/// Asserts that a run still running has no tail in the observation, whatever the server
/// sent with it.
#[test]
fn a_run_still_running_keeps_no_tail() {
    let clock = FakeClock::at(T0);
    let farm = Stub {
        run: Ok(RunStanding {
            state: "running".to_owned(),
            outcome: None,
            log_tail: Some(tail("js-measure-result {\"a\":1}", "", false)),
        }),
        ..Stub::held()
    };
    let obs = drive(&farm, &clock, &[job("a", 1, None)], 10, 100);
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

fn ask() -> RunAsk {
    job("a", 5_000, None).run
}

/// Asserts the fake farm's log: a run that is running holds none, an exited run holds the
/// tail it was given, and a farm given none holds none.
#[test]
fn the_fake_farm_holds_a_log_only_for_an_exited_run() {
    let clock = FakeClock::at(T0);
    let given = tail("out\n", "err\n", true);
    let farm = fake_with(&clock, Some(given.clone()));
    let Acquired::Held { id, .. } = farm.acquire(&job("a", 1, None).lease).unwrap() else {
        panic!("not held");
    };
    let run = farm.start_run(&id, &ask()).unwrap();
    let live = farm.run(&id, &run).unwrap();
    assert_eq!((live.phase(), live.log_tail), (Phase::Running, None));
    clock.sleep_ms(5_000);
    let over = farm.run(&id, &run).unwrap();
    assert_eq!((over.phase(), over.log_tail), (Phase::Over, Some(given)));

    let clock = FakeClock::at(T0);
    let farm = fake_with(&clock, None);
    let Acquired::Held { id, .. } = farm.acquire(&job("a", 1, None).lease).unwrap() else {
        panic!("not held");
    };
    let run = farm.start_run(&id, &ask()).unwrap();
    clock.sleep_ms(5_000);
    assert_eq!(farm.run(&id, &run).unwrap().log_tail, None);
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

fn timing(name: &str, log_tail: Option<LogTail>) -> JobTiming {
    JobTiming {
        name: name.to_owned(),
        submit_at_ms: T0,
        requested_ms: Some(T0),
        granted_ms: Some(T0),
        started_ms: Some(T0),
        finished_ms: Some(T0 + 60_000),
        host: Some("h1".to_owned()),
        outcome: Some("succeeded".to_owned()),
        failure: None,
        log_tail,
    }
}

fn assembled(tails: Vec<Option<LogTail>>) -> RunOutput {
    let jobs: Vec<_> = tails
        .iter()
        .enumerate()
        .map(|(n, _)| job(&format!("j{n}"), 60_000, None))
        .collect();
    let obs = Observation {
        t0_ms: T0,
        end_ms: T0 + 60_000,
        jobs: tails
            .into_iter()
            .enumerate()
            .map(|(n, tail)| timing(&format!("j{n}"), tail))
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
    assemble(&who(), &obs, &jobs, &BTreeMap::new(), &hosts)
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
    let line = "js-measure-result {\"makespan_ms\":1,\"jobs\":99,\"request_to_token_ms_mean\":2}\n";
    let out = assembled(vec![Some(tail(line, "", false))]);
    assert_eq!(out.record.metrics["makespan_ms"], Some(60_000.0));
    assert_eq!(out.record.metrics["jobs"], Some(1.0));
    assert_eq!(out.record.metrics["request_to_token_ms_mean"], Some(0.0));
    assert_eq!(out.record.metrics["reported_makespan_ms"], Some(1.0));
    assert_eq!(out.record.metrics["reported_jobs"], Some(99.0));
    assert_eq!(
        out.record.metrics["reported_request_to_token_ms_mean"],
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

/// Asserts the record of two jobs: the mean of their numbers, none when one of them has
/// no number for the key.
#[test]
fn two_jobs_record_the_mean_and_a_gap_is_none() {
    let a = tail("js-measure-result {\"x\":2,\"y\":1}\n", "", false);
    let b = tail("js-measure-result {\"x\":4}\n", "", false);
    let out = assembled(vec![Some(a), Some(b)]);
    assert_eq!(
        reported_of(&out),
        report(&[("reported_x", Some(3.0)), ("reported_y", None)])
    );
}

// ---- the plan ---------------------------------------------------------------------

/// A farm that records the lease class of every request and otherwise is the fake.
struct Spy<'a> {
    inner: FakeFarm<'a>,
    classes: RefCell<Vec<String>>,
}

impl Farm for Spy<'_> {
    fn acquire(&self, ask: &LeaseAsk) -> Result<Acquired, FarmError> {
        self.classes.borrow_mut().push(ask.class.clone());
        self.inner.acquire(ask)
    }
    fn lease(&self, id: &str) -> Result<Standing, FarmError> {
        self.inner.lease(id)
    }
    fn start_run(&self, lease: &str, ask: &RunAsk) -> Result<String, FarmError> {
        self.inner.start_run(lease, ask)
    }
    fn run(&self, lease: &str, run: &str) -> Result<RunStanding, FarmError> {
        self.inner.run(lease, run)
    }
    fn release(&self, lease: &str) -> Result<(), FarmError> {
        self.inner.release(lease)
    }
    fn host(&self, id: &str) -> Result<HostShape, FarmError> {
        self.inner.host(id)
    }
    fn usage_rows(&self, host: &str, from: u64, to: u64) -> Result<Vec<UsageRow>, FarmError> {
        self.inner.usage_rows(host, from, to)
    }
    fn curve(&self, profile: &str) -> Result<Option<Curve>, FarmError> {
        self.inner.curve(profile)
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
        r#"{"parameter":"sharing","settings":["s"],"arms":[{"label":"shared","control":true},{"label":"whole","lease_class":"whole-node"}],"runs_per_arm":3,"seed":2}"#,
    )
    .unwrap()
}

fn one_job_workload(class: &str) -> Workload {
    serde_json::from_str(&format!(
        r#"{{"jobs":[{{"name":"w","count":1,"image":"img","cpu_millis":1000,"memory_bytes":1000,"wall_ms":30000,"class":"{class}"}}]}}"#
    ))
    .unwrap()
}

/// Asserts that an arm's `lease_class` replaces the class the workload names for every
/// job of that arm and only that arm, and that an arm without one keeps the workload's.
#[test]
fn an_arm_can_set_the_lease_class() {
    let plan = two_arm_plan();
    let workload = one_job_workload("pool-shared");
    let clock = FakeClock::at(T0);
    let spy = Spy {
        inner: fake_with(&clock, None),
        classes: RefCell::new(Vec::new()),
    };
    let outputs = run_everything(&plan, &workload, &spy, &clock);
    let by_run: Vec<(String, String)> = outputs
        .iter()
        .map(|o| o.record.arm.clone())
        .zip(spy.classes.borrow().iter().cloned())
        .collect();
    assert_eq!(by_run.len(), 6);
    for (arm, class) in by_run {
        let expected = if arm == "whole" {
            "whole-node"
        } else {
            "pool-shared"
        };
        assert_eq!(class, expected, "arm {arm}");
    }
}

/// Asserts that an arm's `lease_class` is optional in the plan file and that an unknown
/// key in an arm is still refused.
#[test]
fn the_lease_class_is_optional_in_an_arm() {
    let plan = two_arm_plan();
    assert_eq!(plan.arms[0].lease_class, None);
    assert_eq!(plan.arms[1].lease_class.as_deref(), Some("whole-node"));
    let bad = r#"{"label":"x","lease_clas":"y"}"#;
    assert!(serde_json::from_str::<js_measure::Arm>(bad).is_err());
}

fn read_shipped(name: &str) -> String {
    let path = format!("{}/plans/{name}", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(&path).unwrap_or_else(|why| panic!("{path}: {why}"))
}

/// Asserts the shipped whole-host noise plan: one setting (the bench cell), two arms
/// `shared` (the control) and `whole-host` (asking for `whole-node`), 20 measured runs
/// of each, and its schedule gives each arm exactly 20 runs.
#[test]
fn the_whole_host_noise_plan_has_two_arms_of_twenty() {
    let plan: Plan = serde_json::from_str(&read_shipped("whole-host-noise.plan.json")).unwrap();
    assert_eq!(plan.settings, vec!["tpch/q6-sf1".to_owned()]);
    let arms: Vec<(&str, bool, Option<&str>)> = plan
        .arms
        .iter()
        .map(|a| (a.label.as_str(), a.control, a.lease_class.as_deref()))
        .collect();
    assert_eq!(
        arms,
        vec![
            ("shared", true, None),
            ("whole-host", false, Some("whole-node"))
        ]
    );
    assert_eq!(plan.runs_per_arm, 20);
    let slots = plan.schedule().unwrap();
    for arm in 0..2 {
        let n = slots.iter().filter(|s| s.arm == arm && !s.warmup).count();
        assert_eq!(n, 20);
    }
}

/// Asserts the shipped workload: one job that runs the bench runner with its real flags
/// (query q6, scale 1, the result-line opt-in), asks for the `action` class, and that the
/// plan's whole-host arm asks for a different one, `whole-node`.
#[test]
fn the_whole_host_noise_workload_runs_the_cell() {
    let workload: Workload =
        serde_json::from_str(&read_shipped("whole-host-noise.workload.json")).unwrap();
    let plan: Plan = serde_json::from_str(&read_shipped("whole-host-noise.plan.json")).unwrap();
    let jobs = workload.expand().unwrap();
    assert_eq!(jobs.len(), 1);
    let args = &jobs[0].run.args;
    let value_of = |flag: &str| {
        let at = args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag}: {args:?}"));
        args[at + 1].as_str()
    };
    assert_eq!(value_of("--query"), "q6");
    assert_eq!(value_of("--scale"), "1");
    assert_eq!(value_of("--dataset"), "tpch");
    assert_eq!(value_of("--suite"), "tpch");
    assert_eq!(args.last().map(String::as_str), Some("--js-measure-result"));
    assert!(!args.iter().any(|a| a == "--cell" || a == "--result-line"), "{args:?}");
    assert_eq!(jobs[0].lease.class, "action");
    let whole = plan.arms.iter().find(|a| a.label == "whole-host").unwrap();
    assert_eq!(whole.lease_class.as_deref(), Some("whole-node"));
    assert!(
        jobs[0].run.image.contains("@sha256:"),
        "{}",
        jobs[0].run.image
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
    assert_eq!(m["reported_verdict"], None);
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
