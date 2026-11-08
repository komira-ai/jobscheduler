//! The report's plans (`plans/job-launch`, `plans/queue-wait-under-load` and
//! `plans/whole-host-vs-backfill`): each validates, its workload is the job mix the results
//! page describes, the results page names every column the harness writes, and a run of
//! each on the simulated farm analyses to a golden. The goldens are simulated runs, not
//! measurements; nothing here reads a clock.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use js_analysis::{Options, analyze, parse_jsonl, to_markdown};
use js_measure::farm::{RunStanding, Standing};
use js_measure::{
    Acquired, Curve, Experiment, FakeClock, FakeFarm, Farm, FarmError, HostShape, LeaseAsk,
    NoSetter, Plan, RunAsk, RunOutput, UsageRow, Workload, run_plan,
};

use crate::support::{GIB, T0};

/// One of the report's plans and how the tests run it on the simulated farm.
struct Shipped {
    /// The plan's name: its files are `plans/<name>.plan.json` and `.workload.json`.
    name: &'static str,
    /// The base arm of the analysis, as the results page gives it.
    baseline: Option<&'static str>,
    /// The metrics the results page says the plan is read by.
    metrics: &'static [&'static str],
    /// Hosts of the simulated farm.
    hosts: usize,
    /// Milliseconds between polls.
    poll_ms: u64,
}

const SHIPPED: [Shipped; 3] = [
    Shipped {
        name: "job-launch",
        baseline: Some("shared"),
        metrics: &[
            "request_to_token_ms_mean",
            "request_to_token_ms_max",
            "queue_wait_ms_mean",
            "queue_wait_ms_max",
        ],
        hosts: 1,
        poll_ms: 1_000,
    },
    Shipped {
        name: "queue-wait-under-load",
        baseline: None,
        metrics: &[
            "jobs",
            "jobs_failed",
            "makespan_ms",
            "queue_wait_ms_mean",
            "queue_wait_ms_max",
        ],
        hosts: 1_000,
        poll_ms: 10_000,
    },
    Shipped {
        name: "whole-host-vs-backfill",
        baseline: Some("backfill"),
        metrics: &[
            "queue_wait_ms_max",
            "queue_wait_ms_mean",
            "makespan_ms",
            "host_cpu_share_mean",
            "jobs_failed",
        ],
        hosts: 128,
        poll_ms: 10_000,
    },
];

/// The simulated farm's host: the shape `js-measure run` without `--live` uses.
const SHAPE: HostShape = HostShape {
    cpu_millis: 64_000,
    memory_bytes: 256 * GIB,
};

fn manifest(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn read(path: PathBuf) -> String {
    fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()))
}

fn plan(name: &str) -> Plan {
    let text = read(manifest(&format!("plans/{name}.plan.json")));
    serde_json::from_str(&text).unwrap_or_else(|why| panic!("{name}: {why}"))
}

fn workload(name: &str) -> Workload {
    let text = read(manifest(&format!("plans/{name}.workload.json")));
    serde_json::from_str(&text).unwrap_or_else(|why| panic!("{name}: {why}"))
}

fn results_page() -> String {
    read(manifest("../../docs/report/results/README.md"))
}

/// The plan cut to 3 measured runs per arm after 1 warm-up, its seed, settings and arms
/// kept, so that its simulated run stays short.
fn cut(mut plan: Plan) -> Plan {
    plan.runs_per_arm = 3;
    plan.warmups_per_arm = 1;
    plan
}

/// The simulated farm of `shipped`'s hosts on `clock`.
fn farm<'a>(shipped: &Shipped, clock: &'a FakeClock) -> FakeFarm<'a> {
    let hosts = (1..=shipped.hosts).map(|n| (format!("fake-{n}"), SHAPE));
    FakeFarm::new(clock, hosts.collect())
}

/// Runs `plan` with the workload of `shipped` on a simulated farm of its hosts.
fn simulate(shipped: &Shipped, plan: &Plan) -> Vec<RunOutput> {
    let clock = FakeClock::at(T0);
    simulate_on(shipped, plan, &farm(shipped, &clock), &clock)
}

/// Runs `plan` with the workload of `shipped` on `farm`.
fn simulate_on(
    shipped: &Shipped,
    plan: &Plan,
    farm: &dyn Farm,
    clock: &FakeClock,
) -> Vec<RunOutput> {
    let jobs = workload(shipped.name).expand().unwrap();
    let experiment = Experiment {
        plan,
        jobs: &jobs,
        farm,
        clock,
        setter: &NoSetter,
        commit: "0000000",
        poll_ms: shipped.poll_ms,
        deadline_ms: 3_600_000,
    };
    let mut outputs = Vec::new();
    run_plan(&experiment, &mut |run| -> io::Result<()> {
        outputs.push(run.clone());
        Ok(())
    })
    .unwrap();
    outputs
}

/// The markdown report `js-analysis <records> [--baseline B]` prints for these runs.
fn analysis(shipped: &Shipped, outputs: &[RunOutput]) -> String {
    let text: String = outputs
        .iter()
        .map(|o| serde_json::to_string(&o.record).unwrap() + "\n")
        .collect();
    let options = Options {
        baseline: shipped.baseline.map(str::to_owned),
        effects: vec![0.05, 0.1],
        ..Options::default()
    };
    to_markdown(&analyze(parse_jsonl(&text).unwrap(), &options).unwrap())
}

/// The names of `keys` that `page` does not show as a code span.
fn unnamed<'a>(page: &str, keys: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    keys.into_iter()
        .filter(|key| !page.contains(&format!("`{key}`")))
        .cloned()
        .collect()
}

/// The column names of a JSON object row, and of its `metrics` object.
fn columns(row: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let object = row.as_object().unwrap();
    let metrics = object["metrics"].as_object().unwrap();
    (
        object.keys().cloned().collect(),
        metrics.keys().cloned().collect(),
    )
}

/// The row of the results page's plan table that starts with the plan's name.
fn table_row<'a>(page: &'a str, name: &str) -> &'a str {
    let start = format!("| `{name}` |");
    page.lines()
        .find(|line| line.starts_with(&start))
        .unwrap_or_else(|| panic!("the results page has no row for {name}"))
}

/// Asserts that every shipped plan validates and that its schedule runs every arm at
/// every setting exactly `runs_per_arm` times after exactly `warmups_per_arm` warm-ups,
/// the warm-ups first. Catches a plan file with a repeated arm or setting, no runs, or a
/// key the plan format does not have (the plan refuses unknown keys).
#[test]
fn every_report_plan_validates_and_schedules_each_cell_fully() {
    for shipped in &SHIPPED {
        let plan = plan(shipped.name);
        plan.validate()
            .unwrap_or_else(|why| panic!("{}: {why}", shipped.name));
        let slots = plan.schedule().unwrap();
        let warmups = plan.warmups_per_arm as usize;
        let cells = plan.settings.len() * plan.arms.len();
        assert_eq!(slots.len(), cells * (warmups + plan.runs_per_arm as usize));
        assert!(
            slots.iter().take(cells * warmups).all(|s| s.warmup),
            "{}: the warm-ups come first",
            shipped.name
        );
        for setting in 0..plan.settings.len() {
            for arm in 0..plan.arms.len() {
                let of = |warmup: bool| {
                    slots
                        .iter()
                        .filter(|s| s.setting == setting && s.arm == arm && s.warmup == warmup)
                        .count()
                };
                assert_eq!(of(true), warmups, "{} arm {arm}", shipped.name);
                assert_eq!(
                    of(false),
                    plan.runs_per_arm as usize,
                    "{} arm {arm}",
                    shipped.name
                );
            }
        }
    }
}

/// Asserts the arms and run counts of each plan as the results page gives them, and that
/// the baseline the page names is an arm of the plan (the analysis refuses an unknown one).
#[test]
fn the_report_plans_have_the_arms_the_results_page_gives() {
    let arms = |name: &str| -> Vec<(String, bool, Option<String>)> {
        plan(name)
            .arms
            .into_iter()
            .map(|a| (a.label, a.control, a.lease_class))
            .collect()
    };
    let arm = |label: &str, control: bool, class: Option<&str>| {
        (label.to_owned(), control, class.map(str::to_owned))
    };
    assert_eq!(
        arms("job-launch"),
        vec![
            arm("shared", true, None),
            arm("whole-host", false, Some("whole-node"))
        ]
    );
    assert_eq!(
        arms("queue-wait-under-load"),
        vec![arm("load-placement", true, None)]
    );
    assert_eq!(
        arms("whole-host-vs-backfill"),
        vec![
            arm("backfill", true, Some("action")),
            arm("whole-host", false, None)
        ]
    );
    let counts: Vec<(u32, u32)> = SHIPPED
        .iter()
        .map(|s| plan(s.name))
        .map(|p| (p.runs_per_arm, p.warmups_per_arm))
        .collect();
    assert_eq!(counts, vec![(30, 2), (10, 1), (20, 1)]);
    for shipped in &SHIPPED {
        let labels: Vec<String> = plan(shipped.name)
            .arms
            .into_iter()
            .map(|a| a.label)
            .collect();
        if let Some(base) = shipped.baseline {
            assert!(labels.iter().any(|l| l == base), "{}: {base}", shipped.name);
        }
    }
}

/// Asserts the job mix of each workload as the results page describes it: the job counts,
/// sizes and submit times, and that every image is a placeholder pinned by digest, so no
/// registry address is in the repository and no plan runs as shipped.
#[test]
fn the_report_workloads_are_the_job_mixes_the_results_page_describes() {
    for shipped in &SHIPPED {
        for job in workload(shipped.name).expand().unwrap() {
            let image = &job.run.image;
            assert!(
                image.starts_with("REPLACE-WITH-") && image.contains("@sha256:REPLACE-WITH-"),
                "{} {}: {image}",
                shipped.name,
                job.name
            );
            assert_eq!(job.run.args.first().map(String::as_str), Some("sleep"));
            assert_eq!(
                (job.run.cpu_millis, job.run.memory_bytes),
                (job.lease.cpu_millis, job.lease.memory_bytes)
            );
        }
    }

    let launch = workload("job-launch").expand().unwrap();
    assert_eq!(launch.len(), 1);
    assert_eq!(
        (launch[0].lease.class.as_str(), launch[0].submit_at_ms),
        ("action", 0)
    );

    let load = workload("queue-wait-under-load").expand().unwrap();
    assert_eq!(load.len(), 1000);
    assert!(load.iter().all(|j| j.lease.class == "action"
        && (j.lease.cpu_millis, j.lease.memory_bytes) == (2_000, 4 * GIB)));
    let submits: Vec<u64> = load.iter().map(|j| j.submit_at_ms).collect();
    assert_eq!(submits, (0..1000).map(|i| i * 100).collect::<Vec<u64>>());

    let mix = workload("whole-host-vs-backfill").expand().unwrap();
    let (large, small): (Vec<_>, Vec<_>) = mix.iter().partition(|j| j.name.starts_with("large"));
    assert_eq!((large.len(), small.len()), (1, 300));
    let large = large[0];
    assert_eq!(large.name, "large-0");
    assert_eq!(large.lease.class, "whole-node");
    assert_eq!(
        (large.lease.cpu_millis, large.lease.memory_bytes),
        (SHAPE.cpu_millis, SHAPE.memory_bytes)
    );
    assert!(small.iter().all(|j| j.lease.class == "action"
        && (j.lease.cpu_millis, j.lease.memory_bytes) == (4_000, 16 * GIB)));
    let last_small = small.iter().map(|j| j.submit_at_ms).max().unwrap();
    assert_eq!(last_small, 299 * 2_000);
    // The large job comes after the stream has begun and well before it ends.
    assert_eq!(large.submit_at_ms, 60_000);
    assert!(large.submit_at_ms < last_small);
}

/// A farm that records the class and CPU of every lease asked of it, then asks the
/// simulated farm.
struct Asks<'a> {
    inner: FakeFarm<'a>,
    seen: RefCell<Vec<(String, u64)>>,
}

impl Farm for Asks<'_> {
    fn acquire(&self, ask: &LeaseAsk) -> Result<Acquired, FarmError> {
        self.seen
            .borrow_mut()
            .push((ask.class.clone(), ask.cpu_millis));
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

/// Asserts the leases the two arms of `whole-host-vs-backfill` ask the farm for, as the
/// harness sends them in a run of the shipped plan (cut to 3 runs per arm after 1
/// warm-up): under `backfill` every job, the large one (64 cores) included, asks for
/// `action`; under `whole-host` the large job asks for `whole-node` and the 300 small ones
/// for `action`. Catches an arm, or a change of how the harness applies an arm's class,
/// that turns the small stream whole-host too, which would leave nothing to fill in
/// around the large job, or that leaves the large job a share in both arms.
#[test]
fn only_the_large_job_differs_between_whole_host_and_backfill() {
    let shipped = &SHIPPED[2];
    let plan = cut(plan(shipped.name));
    let clock = FakeClock::at(T0);
    let asks = Asks {
        inner: farm(shipped, &clock),
        seen: RefCell::new(Vec::new()),
    };
    let outputs = simulate_on(shipped, &plan, &asks, &clock);
    let seen = asks.seen.borrow();
    assert_eq!(seen.len(), outputs.len() * 301);
    for (output, run) in outputs.iter().zip(seen.chunks(301)) {
        let arm = output.record.arm.as_str();
        let large = if arm == "whole-host" {
            "whole-node"
        } else {
            "action"
        };
        let classes: BTreeSet<(bool, &str)> = run
            .iter()
            .map(|(class, cpu)| (*cpu == SHAPE.cpu_millis, class.as_str()))
            .collect();
        assert_eq!(
            classes,
            [(false, "action"), (true, large)].into(),
            "arm {arm}"
        );
        assert_eq!(
            run.iter()
                .filter(|(_, cpu)| *cpu == SHAPE.cpu_millis)
                .count(),
            1
        );
    }
    let arms: BTreeSet<&str> = outputs.iter().map(|o| o.record.arm.as_str()).collect();
    assert_eq!(arms, ["backfill", "whole-host"].into());
}

/// Asserts that every metric the results page names for a plan is in the plan's row of
/// the page and is a metric its simulated run records. Catches a metric name the harness
/// does not have, and a row that drifted from the test's list.
#[test]
fn the_metrics_named_for_each_plan_are_recorded() {
    let page = results_page();
    for shipped in &SHIPPED {
        let row = table_row(&page, shipped.name);
        let names: Vec<String> = shipped.metrics.iter().map(|m| (*m).to_owned()).collect();
        assert_eq!(
            unnamed(row, &names),
            Vec::<String>::new(),
            "{}",
            shipped.name
        );
        let outputs = simulate(shipped, &cut(plan(shipped.name)));
        let recorded = &outputs[0].record.metrics;
        for name in &names {
            assert!(
                recorded.get(name).copied().flatten().is_some(),
                "{}: {name} is not recorded",
                shipped.name
            );
        }
    }
}

/// Asserts that the results page names, as a code span, every column of `records.jsonl`,
/// `jobs.jsonl` and `minutes.jsonl` and every metric of a record and of a job row, as a
/// simulated run writes them. Catches a column the harness adds or renames without the
/// page following.
#[test]
fn the_results_page_names_every_column_the_harness_writes() {
    let page = results_page();
    let outputs = simulate(&SHIPPED[0], &cut(plan(SHIPPED[0].name)));
    let first = &outputs[0];
    let (record, record_metrics) = columns(&serde_json::to_value(&first.record).unwrap());
    let (job, job_metrics) = columns(&serde_json::to_value(&first.jobs[0]).unwrap());
    let minute = serde_json::to_value(&first.minutes[0]).unwrap();
    let minute: Vec<String> = minute.as_object().unwrap().keys().cloned().collect();
    assert_eq!(record_metrics.len(), 18);
    assert_eq!(job_metrics.len(), 11);
    let all = record
        .iter()
        .chain(&record_metrics)
        .chain(&job)
        .chain(&job_metrics)
        .chain(&minute);
    assert_eq!(unnamed(&page, all), Vec::<String>::new());
}

/// Asserts the row check the metric test rests on: a plan's row that lacks one of the
/// plan's metrics reports it, a metric named in another row does not count for this one,
/// and a plan the page has no row for is a failure.
#[test]
fn a_metric_missing_from_a_plans_row_is_reported() {
    let page = "| `a` | x | `jobs` |\n| `b` | y | `makespan_ms` |\n";
    let names = ["jobs", "makespan_ms"].map(str::to_owned);
    assert_eq!(unnamed(table_row(page, "a"), &names), vec!["makespan_ms"]);
    assert_eq!(unnamed(table_row(page, "b"), &names), vec!["jobs"]);
    let missing = std::panic::catch_unwind(|| table_row(page, "c").to_owned());
    assert!(missing.is_err());
}

/// Asserts the check the column test rests on: a name the page does not show as a code
/// span is reported, a name it shows is not, and a name that appears only as plain text
/// (not between backticks) is reported.
#[test]
fn a_column_the_page_does_not_name_is_reported() {
    let page = "| `setting` | the setting |\nrun_index is plain text\n";
    let keys = ["setting", "run_index", "arm"].map(str::to_owned);
    assert_eq!(unnamed(page, &keys), vec!["run_index", "arm"]);
    assert_eq!(unnamed(page, &keys[..1]), Vec::<String>::new());
}

/// Asserts that the analysis of each plan's simulated run (the plan cut to 3 runs per arm
/// after 1 warm-up, on the farm sizes of [`SHIPPED`]) is the golden in `tests/golden`:
/// every run recorded with its label, every metric summarised, and the comparisons with
/// the baseline the results page names. The simulated farm gives every run of a cell the
/// same numbers, so every spread is zero, and it places every lease alike whatever its
/// class, so the two arms of a plan give the same numbers and every comparison is 0%: the
/// goldens hold the path from plan to report and its layout, not a difference of arms. Catches a change of the plans, the harness's
/// numbers or the analysis that the goldens do not show. A failure writes what the run
/// gave beside the target directory's other test files, to compare with the golden.
#[test]
fn the_analysis_of_each_simulated_plan_run_is_its_golden() {
    for shipped in &SHIPPED {
        let plan = cut(plan(shipped.name));
        let outputs = simulate(shipped, &plan);
        assert_eq!(outputs.len(), plan.settings.len() * plan.arms.len() * 4);
        assert!(
            outputs
                .iter()
                .all(|o| o.jobs.iter().all(|j| j.failure.is_none()))
        );
        let got = analysis(shipped, &outputs);
        let golden = manifest(&format!("tests/golden/{}.fake-run.md", shipped.name));
        let want = fs::read_to_string(&golden).unwrap_or_default();
        if got != want {
            let actual = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("{}.fake-run.md", shipped.name));
            fs::write(&actual, &got).unwrap();
            panic!(
                "{}: the analysis is not {}; what the run gave is in {}",
                shipped.name,
                golden.display(),
                actual.display()
            );
        }
    }
}
