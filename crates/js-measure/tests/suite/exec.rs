//! A plan end to end on a simulated farm: the order, the identity of each record, the
//! setter, the predictions, and what stops a plan.

use std::cell::RefCell;
use std::io;

use js_measure::metric::name;
use js_measure::{
    Arm, CommandSetter, Curve, Experiment, ExperimentError, FakeClock, FakeFarm, HostShape,
    NoSetter, PlanError, RunOutput, SetError, Setter, run_plan,
};

use crate::support::{GIB, T0, job, plan, plan_over};

fn farm(clock: &FakeClock) -> FakeFarm<'_> {
    let shape = HostShape {
        cpu_millis: 32_000,
        memory_bytes: 128 * GIB,
    };
    FakeFarm::new(clock, vec![("h1".to_owned(), shape)])
}

/// Records the arms it was asked to put in place.
struct Recording(RefCell<Vec<(String, String, String, bool)>>);

impl Setter for Recording {
    fn apply(&self, parameter: &str, setting: &str, arm: &Arm) -> Result<(), SetError> {
        self.0.borrow_mut().push((
            parameter.to_owned(),
            setting.to_owned(),
            arm.label.clone(),
            arm.control,
        ));
        Ok(())
    }
}

fn run_all(experiment: &Experiment<'_>) -> (Result<usize, ExperimentError>, Vec<RunOutput>) {
    let mut outputs = Vec::new();
    let result = run_plan(experiment, &mut |run| {
        outputs.push(run.clone());
        Ok(())
    });
    (result, outputs)
}

/// Asserts a whole plan on a simulated farm: every slot of the schedule is run in order,
/// the setter is told each arm (and whether it is the control) just before it, and each
/// record carries the setting, the arm, the executed order as `run_index`, the seed, the
/// commit, and the warm-up label on the warm-ups only. It catches a setter called out of
/// order and a record that names the wrong arm.
#[test]
fn a_plan_runs_in_its_order_and_labels_each_run() {
    let mut p = plan_over(&["0.8", "1.0"], &["whole", "booked"], 1, 1, 5);
    p.arms[1].control = true;
    let slots = p.schedule().unwrap();
    let clock = FakeClock::at(T0);
    let farm = farm(&clock);
    let jobs = [job("a", 60_000, None)];
    let setter = Recording(RefCell::new(Vec::new()));
    let experiment = Experiment {
        plan: &p,
        jobs: &jobs,
        farm: &farm,
        clock: &clock,
        setter: &setter,
        commit: "deadbeef",
        poll_ms: 10_000,
        deadline_ms: 3_600_000,
    };
    let (result, outputs) = run_all(&experiment);
    assert_eq!(result.unwrap(), 8);
    assert_eq!(outputs.len(), 8);
    for (slot, output) in slots.iter().zip(&outputs) {
        let record = &output.record;
        let arm = &p.arms[slot.arm];
        assert_eq!(record.arm, arm.label);
        assert_eq!(record.setting, p.settings[slot.setting]);
        assert_eq!(record.run_index, slot.order);
        assert_eq!((record.seed, record.commit.as_str()), (5, "deadbeef"));
        assert_eq!(record.label == "warmup", slot.warmup);
        assert_eq!(output.jobs.len(), 1);
    }
    let applied = setter.0.borrow();
    let expected: Vec<(String, String, String, bool)> = slots
        .iter()
        .map(|slot| {
            (
                "fill".to_owned(),
                p.settings[slot.setting].clone(),
                p.arms[slot.arm].label.clone(),
                slot.arm == 1,
            )
        })
        .collect();
    assert_eq!(*applied, expected);
    let measured = outputs.iter().filter(|o| o.record.label.is_empty()).count();
    assert_eq!(measured, 4);
    let warmups = outputs
        .iter()
        .filter(|o| o.record.label == "warmup")
        .count();
    assert_eq!(warmups, 4);
}

/// Asserts the numbers of a run on the simulated farm for one job of 60 s on one host:
/// the makespan is the job's run (to the poll), nothing waited, and the host's usage is
/// half of the CPU the job was limited to over the host's 32 cores.
#[test]
fn a_run_on_the_simulated_farm_records_its_numbers() {
    let p = plan(&["a"], 1, 0, 1);
    let clock = FakeClock::at(T0);
    let farm = farm(&clock);
    let jobs = [job("a", 60_000, None)];
    let experiment = Experiment {
        plan: &p,
        jobs: &jobs,
        farm: &farm,
        clock: &clock,
        setter: &NoSetter,
        commit: "c",
        poll_ms: 10_000,
        deadline_ms: 3_600_000,
    };
    let (result, outputs) = run_all(&experiment);
    assert_eq!(result.unwrap(), 1);
    let m = &outputs[0].record.metrics;
    assert_eq!(m[name::MAKESPAN_MS], Some(60_000.0));
    assert_eq!(m[name::QUEUE_WAIT_MS_MAX], Some(10_000.0));
    assert_eq!(m[name::JOBS_FAILED], Some(0.0));
    // 4 cores limited, half used, over 32 cores.
    assert_eq!(m[name::HOST_CPU_SHARE_MAX], Some(2_000.0 / 32_000.0));
    assert_eq!(m[name::CPU_USED_OVER_BOOKED], Some(0.5));
    assert_eq!(m[name::MEMORY_USED_OVER_BOOKED], Some(0.5));
    assert_eq!(m[name::RUNTIME_ACTUAL_OVER_PREDICTED_MEAN], None);
}

/// Asserts that a job with a profile the server has a curve for is compared with it, and
/// that a profile without a curve, or with a curve of no minute, is not.
#[test]
fn predictions_come_from_the_servers_curves() {
    let p = plan(&["a"], 1, 0, 1);
    let clock = FakeClock::at(T0);
    let curve = Curve {
        runs: 4,
        ws_bytes: vec![4 * GIB, 2 * GIB],
        cores_milli: Vec::new(),
    };
    let empty = Curve {
        runs: 1,
        ws_bytes: Vec::new(),
        cores_milli: Vec::new(),
    };
    let farm = farm(&clock)
        .with_curve("known", curve)
        .with_curve("empty", empty);
    let jobs = [
        job("a", 60_000, Some("known")),
        job("b", 60_000, Some("unknown")),
        job("c", 60_000, Some("empty")),
    ];
    let experiment = Experiment {
        plan: &p,
        jobs: &jobs,
        farm: &farm,
        clock: &clock,
        setter: &NoSetter,
        commit: "c",
        poll_ms: 10_000,
        deadline_ms: 3_600_000,
    };
    let (_, outputs) = run_all(&experiment);
    let rows = &outputs[0].jobs;
    assert_eq!(rows[0].metrics["predicted_runtime_ms"], Some(120_000.0));
    assert_eq!(
        rows[0].metrics["predicted_peak_ws_bytes"],
        Some(4.0 * GIB as f64)
    );
    assert_eq!(rows[1].metrics["predicted_runtime_ms"], None);
    assert_eq!(rows[2].metrics["predicted_runtime_ms"], None);
}

/// Asserts that an out-of-memory outcome is counted as a memory kill in the record.
#[test]
fn memory_kills_are_counted() {
    let p = plan(&["a"], 1, 0, 1);
    let clock = FakeClock::at(T0);
    let farm = farm(&clock).with_outcome("out_of_memory");
    let jobs = [job("a", 20_000, None)];
    let experiment = Experiment {
        plan: &p,
        jobs: &jobs,
        farm: &farm,
        clock: &clock,
        setter: &NoSetter,
        commit: "c",
        poll_ms: 10_000,
        deadline_ms: 3_600_000,
    };
    let (_, outputs) = run_all(&experiment);
    assert_eq!(outputs[0].record.metrics[name::MEMORY_KILLS], Some(1.0));
    assert_eq!(outputs[0].record.metrics[name::JOBS_FAILED], Some(1.0));
}

struct Failing;

impl Setter for Failing {
    fn apply(&self, _parameter: &str, _setting: &str, _arm: &Arm) -> Result<(), SetError> {
        Err(SetError::Failed("x".to_owned(), "no".to_owned()))
    }
}

/// Asserts what stops a plan: a plan that is not valid before any run, a setter that
/// fails before the first run, and a writer that fails after the first run was handed
/// over (which is the run it failed on, and the plan stops there).
#[test]
fn a_plan_stops_on_a_bad_plan_a_failed_setter_or_a_failed_write() {
    let clock = FakeClock::at(T0);
    let farm = farm(&clock);
    let jobs = [job("a", 20_000, None)];
    let mut experiment = Experiment {
        plan: &plan(&[], 1, 0, 1),
        jobs: &jobs,
        farm: &farm,
        clock: &clock,
        setter: &NoSetter,
        commit: "c",
        poll_ms: 10_000,
        deadline_ms: 3_600_000,
    };
    let (result, outputs) = run_all(&experiment);
    assert!(matches!(
        result,
        Err(ExperimentError::Plan(PlanError::NoArms))
    ));
    assert!(outputs.is_empty());

    let p = plan(&["a", "b"], 1, 0, 1);
    experiment.plan = &p;
    experiment.setter = &Failing;
    let (result, outputs) = run_all(&experiment);
    assert!(matches!(
        result,
        Err(ExperimentError::Set(SetError::Failed(..)))
    ));
    assert!(outputs.is_empty());

    experiment.setter = &NoSetter;
    let mut handed = 0;
    let result = run_plan(&experiment, &mut |_| {
        handed += 1;
        Err(io::Error::other("disk full"))
    });
    let error = result.unwrap_err();
    assert!(matches!(error, ExperimentError::Emit(_)));
    assert_eq!(error.to_string(), "cannot write the records: disk full");
    assert_eq!(handed, 1);
}

/// Asserts the command setter: a program that succeeds is applied for a treatment and
/// for a control arm, one that exits nonzero is a failure naming it, and one that cannot
/// be started is a failure of another kind.
#[test]
fn the_command_setter_runs_a_program() {
    let ok = CommandSetter {
        program: "true".to_owned(),
    };
    let arm = |control| Arm {
        label: "x".to_owned(),
        control,
    };
    assert_eq!(ok.apply("fill", "0.8", &arm(false)), Ok(()));
    assert_eq!(ok.apply("fill", "0.8", &arm(true)), Ok(()));
    let failing = CommandSetter {
        program: "false".to_owned(),
    };
    assert!(
        matches!(failing.apply("fill", "0.8", &arm(false)), Err(SetError::Failed(program, _)) if program == "false")
    );
    let missing = CommandSetter {
        program: "/nonexistent/js-measure-setter".to_owned(),
    };
    assert!(matches!(
        missing.apply("fill", "0.8", &arm(false)),
        Err(SetError::Spawn(..))
    ));
    assert_eq!(NoSetter.apply("fill", "0.8", &arm(false)), Ok(()));
}

/// Asserts that every line a plan emits is read by the analysis crate's own `RunRecord`
/// (unknown fields refused), field for field, and written back as the
/// same line: setting and arm as the cell's texts, `run_index` the executed order, the
/// label `warmup` or empty, a number or `null` for each metric. It catches a field added
/// to or dropped from the emitted line, and a value the reader would take for another.
#[test]
fn every_emitted_line_is_what_the_analysis_reads() {
    use js_analysis::RunRecord as AnalysisRunRecord;
    let p = plan_over(&["0.8", "1.0"], &["whole", "booked"], 1, 1, 3);
    let clock = FakeClock::at(T0);
    let farm = farm(&clock);
    let jobs = [job("a", 60_000, Some("unknown"))];
    let experiment = Experiment {
        plan: &p,
        jobs: &jobs,
        farm: &farm,
        clock: &clock,
        setter: &NoSetter,
        commit: "abc1234",
        poll_ms: 10_000,
        deadline_ms: 3_600_000,
    };
    let (_, outputs) = run_all(&experiment);
    assert_eq!(outputs.len(), 8);
    for (index, output) in outputs.iter().enumerate() {
        let line = serde_json::to_string(&output.record).unwrap();
        let read: AnalysisRunRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(serde_json::to_string(&read).unwrap(), line);
        assert_eq!(usize::try_from(read.run_index).unwrap(), index);
        assert!(["0.8", "1.0"].contains(&read.setting.as_str()));
        assert!(["whole", "booked"].contains(&read.arm.as_str()));
        assert_eq!((read.seed, read.commit.as_str()), (3, "abc1234"));
        assert_eq!(read.label, if index < 4 { "warmup" } else { "" });
        assert_eq!(read.metrics["makespan_ms"], Some(60_000.0));
        assert_eq!(read.metrics["runtime_actual_over_predicted_mean"], None);
        assert!(
            line.contains(r#""runtime_actual_over_predicted_mean":null"#),
            "{line}"
        );
    }
}
