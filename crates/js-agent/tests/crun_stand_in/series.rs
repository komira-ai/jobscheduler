//! A run's CPU second by second (`--cpu-series`) over the stand-in `crun`: the runtime takes
//! the `usage_usec` of the lease's `cpu.stat` at each poll of the running job, on the agent's
//! monotonic clock, and gives the series once the job has ended. Each test moves its own
//! monotonic clock by hand and writes the `cpu.stat` itself; nothing waits for time. A job that
//! is stopped is started with [`crate::adopt::started`], so the stop comes once its script is up.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent::crun::{CrunRuntime, CrunSettings, Hooks};
use js_agent::lifecycle::{Exit, RunHandle, Runtime, Status};
use js_agent_wire::CpuSeries;

use crate::node::{LONG, NoImages, Node};

/// One second, in microseconds.
const SECOND: u64 = 1_000_000;

/// A runtime on `node` that records series when `on`, reading `mono` for its monotonic clock.
fn open(node: &Node, on: bool, mono: fn() -> u64) -> CrunRuntime {
    let settings = CrunSettings {
        cpu_series: on,
        ..node.settings("/root")
    };
    let hooks = Hooks {
        mono_us: mono,
        ..Node::hooks(LONG, LONG)
    };
    CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens")
}

/// Sets the clock `at` to `seconds` seconds.
fn at(clock: &AtomicU64, seconds: u64) {
    clock.store(seconds * SECOND, Ordering::SeqCst);
}

/// Writes the `cpu.stat` of the lease `name` on `node`: `usec` CPU microseconds used.
fn cpu_stat(node: &Node, name: &str, usec: u64) {
    std::fs::write(
        node.cgroup_of(name).join("cpu.stat"),
        format!("usage_usec {usec}\n"),
    )
    .expect("cpu.stat");
}

/// Stops the running `id` and waits for its end, then reads the end with a poll at 20 s.
/// A job of the stand-in exits with the code of its TERM.
fn ended_at_twenty(runtime: &mut CrunRuntime, clock: &AtomicU64, id: &RunHandle) {
    runtime.stop(id, LONG).expect("stopped");
    runtime.wait_ended(id);
    at(clock, 20);
    assert!(matches!(runtime.poll(id), Ok(Status::Exited(Exit { .. }))));
}

/// Proves a job polled while it runs gives its series once it has ended: started at 10 s,
/// it has used 1 CPU second at 11 s and 3 at 12 s; the end is seen by the poll at 20 s, and
/// its usage read at 50 s with 5 CPU seconds in all. The series is one point a second from
/// the start (1 core, then 2 cores), the last 2 CPU seconds spread over the eight seconds
/// from the last poll to the poll that saw the end (a quarter of a core each), and it is 10 s
/// long, not 40. Catches a series that ends where the usage is read, one that does not start
/// at the start of the run, a gap put in one point, and a series not carried by the runtime.
#[test]
fn a_polled_job_gives_its_cpu_by_the_second_and_ends_where_the_end_was_seen() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("series-polled");
    let mut runtime = open(&node, true, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    for (seconds, used) in [(11, SECOND), (12, 3 * SECOND)] {
        at(&MONO, seconds);
        cpu_stat(&node, &id.0, used);
        assert_eq!(runtime.poll(&id), Ok(Status::Running));
    }
    ended_at_twenty(&mut runtime, &MONO, &id);
    at(&MONO, 50);
    cpu_stat(&node, &id.0, 5 * SECOND);
    let mut points = vec![16, 32];
    points.extend([4; 8]);
    assert_eq!(
        runtime.cpu_series(&id),
        Some(CpuSeries {
            granularity_secs: 1,
            points,
            run_usec: 10 * SECOND,
            partial: false,
        })
    );
    runtime.remove(&id).expect("removed");
}

/// Proves a run of an agent without the flag records nothing: the same run as above gives no
/// series, though its usage reads as it always did. Catches a series recorded by default.
#[test]
fn a_job_of_an_agent_without_the_flag_gives_no_series() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("series-off");
    let mut runtime = open(&node, false, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    at(&MONO, 11);
    cpu_stat(&node, &id.0, SECOND);
    assert_eq!(runtime.poll(&id), Ok(Status::Running));
    ended_at_twenty(&mut runtime, &MONO, &id);
    at(&MONO, 50);
    cpu_stat(&node, &id.0, 5 * SECOND);
    assert_eq!(runtime.cpu_series(&id), None);
    runtime.remove(&id).expect("removed");
}

/// Proves a job whose group's `cpu.stat` cannot be read at its end gives no series (never a
/// series with a made-up last second), and so does a container the runtime does not hold.
/// Catches a series sent with an unknown final count, and a panic on an id it never saw.
#[test]
fn a_job_whose_cpu_stat_cannot_be_read_at_its_end_gives_no_series() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("series-blind");
    let mut runtime = open(&node, true, mono);
    at(&MONO, 10);
    let id = crate::adopt::started(&node, &mut runtime, &node.spec("r9-hold"));
    at(&MONO, 11);
    cpu_stat(&node, &id.0, SECOND);
    assert_eq!(runtime.poll(&id), Ok(Status::Running));
    ended_at_twenty(&mut runtime, &MONO, &id);
    std::fs::remove_file(node.cgroup_of(&id.0).join("cpu.stat")).expect("removed cpu.stat");
    assert_eq!(runtime.cpu_series(&id), None);
    let unknown = RunHandle("never-held".to_owned());
    assert_eq!(runtime.cpu_series(&unknown), None);
    runtime.remove(&id).expect("removed");
}

/// Proves a run the agent adopted after a restart gives a partial series that starts at its
/// first poll in the new process: the job ran in the first process (which sampled nothing
/// here), the second adopts it, takes its first sample at 100 s at a count of 400 CPU
/// seconds (what it used before is not in the series), has used 4 more at 102 s (2 cores a
/// second for the two seconds), and its end is seen at 103 s. The series is two points of
/// 2 cores and one of none, 3 s long, marked partial. Catches an adopted run's series that
/// counts its whole CPU in its first second, one that starts at the restart, and one not
/// marked partial.
#[test]
fn an_adopted_job_gives_a_partial_series_from_its_first_poll() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("series-adopted");
    let mut first = open(&node, true, mono);
    let id = crate::adopt::held(&node, &mut first, "r1-hold");
    drop(first);
    let mut second = open(&node, true, mono);
    assert_eq!(second.swept().adopted, ["r1-hold"]);
    for (seconds, used) in [(100, 400 * SECOND), (102, 404 * SECOND)] {
        at(&MONO, seconds);
        cpu_stat(&node, &id.0, used);
        assert_eq!(second.poll(&id), Ok(Status::Running));
    }
    second.stop(&id, LONG).expect("stopped");
    second.wait_ended(&id);
    at(&MONO, 103);
    assert!(matches!(second.poll(&id), Ok(Status::Exited(_))));
    at(&MONO, 200);
    assert_eq!(
        second.cpu_series(&id),
        Some(CpuSeries {
            granularity_secs: 1,
            points: vec![32, 32, 0],
            run_usec: 3 * SECOND,
            partial: true,
        })
    );
    second.remove(&id).expect("removed");
}

/// Proves a job that ended while no agent ran, adopted by an agent that records, gives no
/// series: the agent never sampled it, so its series has no time in it. Catches a series
/// made from nothing for a run that was never seen running.
#[test]
fn a_job_that_ended_while_no_agent_ran_gives_no_series() {
    static MONO: AtomicU64 = AtomicU64::new(0);
    fn mono() -> u64 {
        MONO.load(Ordering::SeqCst)
    }
    let node = Node::new("series-ended-away");
    let mut first = open(&node, true, mono);
    let id = first
        .create_and_start(&node.job("r1-sayexit", 60_000))
        .expect("started");
    first.wait_ended(&id);
    drop(first);
    let mut second = open(&node, true, mono);
    assert_eq!(second.swept().adopted, ["r1-sayexit"]);
    at(&MONO, 30);
    cpu_stat(&node, &id.0, 9 * SECOND);
    assert!(matches!(second.poll(&id), Ok(Status::Exited(_))));
    assert_eq!(second.cpu_series(&id), None);
    second.remove(&id).expect("removed");
}
