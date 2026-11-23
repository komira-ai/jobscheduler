//! The node's CPU per second (`--host-second-samples`) over fake cgroup files and a clock the
//! test sets: what the worker's step reads, what a reading carries to the heartbeat, and what
//! is lost with a beat that fails.

use std::sync::Arc;
use std::time::Duration;

use js_agent::usage::{Collector, Outbox};
use js_agent_wire::NodeLoad;

use super::collect::{ACTIONS, SetClock, Shared};

const SECOND: u64 = 1_000_000;

/// A collector with a 5 second window over files and a clock the test sets.
struct Rig {
    files: Shared,
    clock: SetClock,
    outbox: Arc<Outbox>,
    collector: Collector,
}

impl Rig {
    fn new(seconds: bool) -> Self {
        let (files, clock) = (Shared::default(), SetClock::default());
        let outbox = Arc::new(Outbox::new().with_host_seconds(seconds));
        outbox.set_window(Duration::from_secs(5));
        let collector = Collector::new(
            Box::new(files.clone()),
            ACTIONS,
            Box::new(clock.clone()),
            outbox.clone(),
        );
        Self {
            files,
            clock,
            outbox,
            collector,
        }
    }

    fn put(&self, name: &str, text: &str) {
        let mut files = self.files.0.borrow_mut();
        *files = files.clone().with(&format!("{ACTIONS}/{name}"), text);
    }

    /// The node's files with `usage` microseconds of CPU and, when given, `pressure`
    /// microseconds of `some` pressure.
    fn node(&self, usage: u64, pressure: Option<u64>) {
        self.put("cpu.stat", &format!("usage_usec {usage}\n"));
        self.put("memory.current", "100\n");
        self.put("memory.stat", "inactive_file 0\nactive_file 5\n");
        self.put("cpu.max", "max 100000\n");
        if let Some(total) = pressure {
            let text = format!(
                "some avg10=0.00 avg60=0.00 avg300=0.00 total={total}\n\
                 full avg10=0.00 avg60=0.00 avg300=0.00 total=9\n"
            );
            self.put("cpu.pressure", &text);
        }
    }

    /// Moves the clock to `micros`, and takes a step as the worker does.
    fn step_at(&mut self, micros: u64) -> bool {
        self.clock.0.set(micros);
        self.collector.step()
    }

    /// Runs the collector at the clock and takes the load it published.
    fn run(&mut self) -> NodeLoad {
        assert!(self.collector.run(&[]).is_empty());
        self.outbox.take().expect("a load")
    }
}

/// Asserts the whole path: with the flag on, a step each second reads the node's `cpu.stat`
/// and `cpu.pressure`, and the load that the next window's run publishes carries one busy
/// value and one pressure value for each second between the readings, in order. It catches
/// a series that is not carried to the wire, rates against the wrong baseline, and a
/// pressure total read from the `full` line.
#[test]
fn a_load_carries_the_rates_of_each_second() {
    let mut rig = Rig::new(true);
    rig.node(0, Some(0));
    rig.step_at(0);
    for second in 1..=5_u64 {
        rig.node(second * second * 500_000, Some(second * 100_000));
        rig.step_at(second * SECOND);
    }
    let load = rig.run();
    assert_eq!(load.second_cpu_millis, vec![500, 1500, 2500, 3500, 4500]);
    assert_eq!(load.second_cpu_pressure_some_permille, vec![100; 5]);
    assert_eq!(load.dropped.total(), 0);
}

/// Asserts that a second sample is not taken sooner than a second after the last one: a
/// step half a second later reads nothing, and the value the next step gives is the whole
/// second since the first sample. Catches samples taken at every step, which would give
/// rates over a few milliseconds when the worker steps for an order.
#[test]
fn a_step_sooner_than_a_second_after_the_last_sample_takes_none() {
    let mut rig = Rig::new(true);
    rig.node(0, Some(0));
    rig.step_at(0);
    rig.node(10_000_000, Some(10_000_000));
    rig.step_at(SECOND / 2);
    rig.node(1_000_000, Some(100_000));
    rig.step_at(SECOND);
    let load = rig.run();
    assert_eq!(load.second_cpu_millis, vec![1000]);
    assert_eq!(load.second_cpu_pressure_some_permille, vec![100]);
}

/// Asserts that a host without a readable `cpu.pressure` sends the busy values and no
/// pressure list, and that pressure appears as soon as it can be read. Catches a pressure of
/// 0 sent for "not measured" and a missing pressure file that costs the busy values.
#[test]
fn a_group_without_pressure_sends_busy_values_alone() {
    let mut rig = Rig::new(true);
    rig.node(0, None);
    rig.step_at(0);
    rig.node(1_000_000, None);
    rig.step_at(SECOND);
    rig.node(2_000_000, None);
    rig.step_at(2 * SECOND);
    let load = rig.run();
    assert_eq!(load.second_cpu_millis, vec![1000, 1000]);
    assert!(load.second_cpu_pressure_some_permille.is_empty());

    rig.node(3_000_000, Some(5));
    rig.step_at(3 * SECOND);
    rig.node(4_000_000, Some(500_005));
    rig.step_at(4 * SECOND);
    let load = rig.run();
    assert_eq!(load.second_cpu_millis, vec![1000, 1000]);
    assert_eq!(load.second_cpu_pressure_some_permille, vec![500]);
}

/// Asserts that a node whose `cpu.stat` cannot be read at a step gives no value for it and is
/// tried again at the next step, and that the first readable step is the baseline. Catches a
/// zero invented for an unreadable counter.
#[test]
fn an_unreadable_cpu_stat_gives_no_value() {
    let mut rig = Rig::new(true);
    rig.step_at(0);
    rig.node(5_000_000, Some(0));
    rig.step_at(SECOND);
    rig.node(6_000_000, Some(0));
    rig.step_at(2 * SECOND);
    let load = rig.run();
    assert_eq!(load.second_cpu_millis, vec![1000]);
}

/// Asserts that a run publishes the newest 16 values when more seconds passed than a load
/// holds, and that a second run with no step between sends none: the seconds are sent once.
/// Catches the oldest values kept and values sent again.
#[test]
fn the_newest_sixteen_seconds_are_sent_once() {
    let mut rig = Rig::new(true);
    rig.node(0, Some(0));
    rig.step_at(0);
    for second in 1..=20_u64 {
        rig.node(second * second * 1_000_000, Some(0));
        rig.step_at(second * SECOND);
    }
    let load = rig.run();
    let want: Vec<u32> = (5..=20_u32).map(|s| (2 * s - 1) * 1000).collect();
    assert_eq!(load.second_cpu_millis, want);
    assert_eq!(load.second_cpu_pressure_some_permille, vec![0; 16]);
    let again = rig.run();
    assert!(again.second_cpu_millis.is_empty());
    assert!(again.second_cpu_pressure_some_permille.is_empty());
}

/// Asserts that a beat that failed loses the seconds it carried: after `failed` the next load
/// holds only what was sampled since. Catches seconds that ride on a later beat, which would
/// send an old second after newer ones.
#[test]
fn a_failed_beat_loses_its_seconds() {
    let mut rig = Rig::new(true);
    rig.node(0, Some(0));
    rig.step_at(0);
    rig.node(1_000_000, Some(0));
    rig.step_at(SECOND);
    let first = rig.run();
    assert_eq!(first.second_cpu_millis, vec![1000]);
    rig.outbox.failed();
    rig.node(3_000_000, Some(0));
    rig.step_at(2 * SECOND);
    let second = rig.run();
    assert_eq!(second.second_cpu_millis, vec![2000]);
}

/// Asserts that without the flag a step takes no sample: the load carries no lists, and the
/// step still says whether a window has passed as `due` does. Catches a series that is on by
/// default.
#[test]
fn without_the_flag_no_seconds_are_taken() {
    let mut rig = Rig::new(false);
    rig.node(0, Some(0));
    assert!(rig.step_at(0));
    rig.node(1_000_000, Some(100_000));
    assert!(rig.step_at(SECOND));
    let load = rig.run();
    assert!(load.second_cpu_millis.is_empty());
    assert!(load.second_cpu_pressure_some_permille.is_empty());
    assert!(
        !rig.step_at(2 * SECOND),
        "a window has not passed since the run"
    );
    assert!(!Outbox::new().host_seconds());
}
