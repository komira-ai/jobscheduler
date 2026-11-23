//! The CPU booking is a share, never a ceiling: the kernel's own counters and settings of a
//! lease made as the agent makes it ([`LeasePolicy::TrackedMemory`]), read after its job ran.
//! Nothing here measures a time: a lease with a `cpu.max` counts its periods and the ones it
//! was throttled in (`cpu.stat` `nr_periods`, `nr_throttled`), and one without counts none.
//!
//! [`LeasePolicy::TrackedMemory`]: js_contain::cgroup::LeasePolicy::TrackedMemory

use std::fs;

use crate::cell::{Cell, Shape};

/// Eight processes that each want a core until each has counted to 300,000, then the CPUs
/// the job counts (`nproc`, `sched_getaffinity`) and its own cgroup's `cpu.max`.
const BUSY: &str = "i=0; while [ $i -lt 8 ]; do \
                    (n=0; while [ $n -lt 300000 ]; do n=$((n+1)); done) & i=$((i+1)); done; \
                    wait; nproc; cat /sys/fs/cgroup/cpu.max";

/// The value of `key` in the `cpu.stat` text `stat`.
fn stat_of(stat: &str, key: &str) -> u64 {
    stat.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("no {key} in cpu.stat:\n{stat}"))
}

/// Catches a job held to its CPU booking while the node has idle cores. A lease that booked
/// half a core runs eight busy processes: with a `cpu.max` of half a core the kernel would
/// throttle it in nearly every period of its run (`nr_throttled` close to `nr_periods`); as
/// a share it has no quota, so the kernel counts no period and throttles none, and the job
/// may use every core no other job wants. Its `cpu.weight` is half a core's, 50. The job
/// reads `max` in its own `cpu.max` and counts the CPUs this process may run on. Nor is its
/// memory booking a limit: the lease's `memory.max` and `memory.swap.max` read `max`.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_job_booked_half_a_core_is_never_throttled_and_reads_no_cpu_ceiling() {
    let cell = Cell::new("half-core");
    let shape = Shape {
        cpu_millis: 500,
        ..Shape::default()
    };
    let lease = cell.lease("half-core", &["/bin/sh", "-c", BUSY], shape);
    let running = cell.start(&lease).await;
    let run = cell.finish(&lease, running).await;
    let cpus = rustix::thread::sched_getaffinity(None)
        .expect("this process's CPUs")
        .count();
    let lines: Vec<&str> = run.out().lines().collect();
    assert_eq!(
        lines,
        [cpus.to_string().as_str(), "max 100000"],
        "{}",
        run.said
    );
    let read = |file: &str| {
        fs::read_to_string(lease.cgroup.dir().join(file))
            .unwrap_or_else(|e| panic!("read {file}: {e}"))
    };
    let stat = read("cpu.stat");
    assert!(stat_of(&stat, "usage_usec") > 0, "the job ran:\n{stat}");
    assert_eq!(
        (stat_of(&stat, "nr_periods"), stat_of(&stat, "nr_throttled")),
        (0, 0),
        "no quota, no throttled period:\n{stat}"
    );
    assert_eq!(read("cpu.max").trim(), "max 100000");
    assert_eq!(read("cpu.weight").trim(), "50");
    assert_eq!(read("memory.max").trim(), "max");
    assert_eq!(read("memory.swap.max").trim(), "max");
}

/// Catches leases that want every core not sharing them as they booked them: the kernel
/// takes the leases that booked 1 and 3 cores at weights 100 and 300, so when both want
/// every core they get a quarter and three quarters of them, and neither has a `cpu.max`
/// that would hold it to its booking while the other leaves cores idle.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn leases_that_booked_one_and_three_cores_share_the_cores_one_to_three() {
    let cell = Cell::new("one-to-three");
    let booked = |name: &str, cpu_millis| {
        let shape = Shape {
            cpu_millis,
            ..Shape::default()
        };
        let lease = cell.lease(name, &["/bin/true"], shape);
        let read = |file: &str| {
            fs::read_to_string(lease.cgroup.dir().join(file))
                .unwrap_or_else(|e| panic!("read {file}: {e}"))
        };
        let weight: u64 = read("cpu.weight").trim().parse().expect("a weight");
        (weight, read("cpu.max").trim().to_owned())
    };
    let (one, one_max) = booked("one", 1000);
    let (three, three_max) = booked("three", 3000);
    assert_eq!((one, three), (100, 300));
    assert_eq!(three, 3 * one, "one to three");
    assert_eq!(
        (one_max.as_str(), three_max.as_str()),
        ("max 100000", "max 100000")
    );
}
