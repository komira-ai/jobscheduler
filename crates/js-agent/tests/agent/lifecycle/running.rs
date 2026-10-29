//! What the lifecycle tells of the runs that run: the lease, the control group the runtime
//! reported and when the container started on the agent's clock.

use js_agent::lifecycle::{RunningLease, label};

use super::{Harness, job};

const SLICE: &str = "/sys/fs/cgroup/jobs.slice";

fn at(run: &str) -> String {
    format!("{SLICE}/docker-{run}.scope")
}

/// Asserts what a started run reports: its lease, the control group the runtime named when
/// it started it (asked once, so a change later does not move it), the clock reading at the
/// start in microseconds, and that it was not adopted; a run whose runtime knows no
/// control group is listed with none; and two runs of one lease are listed once. It
/// catches a lease missing from the usage, a start stamped at the wrong moment, and a
/// lease charged twice.
#[test]
fn a_started_run_reports_its_lease_cgroup_and_start() {
    let mut h = Harness::new("running-start");
    h.clock.advance(2_500);
    h.fake.set_cgroup("r1", &at("r1"));
    h.start(job("r1"));
    h.clock.advance(10_000);
    h.start(job("r2"));
    let mut twin = job("r3");
    twin.lease_id = "lease-r1".to_owned();
    h.fake.set_cgroup("r3", &at("r3"));
    h.start(twin);
    assert_eq!(
        h.life.running(),
        [
            RunningLease {
                lease_id: "lease-r1".to_owned(),
                cgroup: Some(at("r1").into()),
                started_usec: Some(2_500_000),
                adopted: false,
            },
            RunningLease {
                lease_id: "lease-r2".to_owned(),
                cgroup: None,
                started_usec: Some(12_500_000),
                adopted: false,
            },
        ]
    );
    h.fake.set_cgroup("r1", "/elsewhere");
    assert_eq!(h.life.running()[0].cgroup, Some(at("r1").into()));
}

/// Asserts that a run that exited, or that was refused, is not listed as running, though
/// the lifecycle holds it until its result is acknowledged. It catches a finished lease
/// that goes on being sampled and charged.
#[test]
fn only_runs_that_run_are_listed() {
    let mut h = Harness::new("running-ended");
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.held(), ["r1", "r2"]);
    let leases: Vec<String> = h.life.running().into_iter().map(|r| r.lease_id).collect();
    assert_eq!(leases, ["lease-r2"]);
    let mut huge = job("r4");
    huge.limits.cpu_millis = 1 << 40;
    h.start(huge);
    assert_eq!(h.life.running().len(), 1, "a refused start does not run");
}

/// Asserts the clock of an adopted run: the container began `running_ms` before the
/// takeover, so its start is that long before now on the agent's clock; it is marked
/// adopted and carries the control group the runtime reports; and when it ran for longer
/// than the agent's clock has been counting there is no start to give. It catches an
/// adopted run stamped as if it began at the restart (its minutes would then be numbered
/// from the wrong moment) and a start before time zero wrapped to a huge number.
#[test]
fn an_adopted_run_started_before_the_takeover() {
    let mut h = Harness::new("running-adopt");
    leak(&h, "old", 40_000);
    leak(&h, "older", 400_000);
    h.fake.set_cgroup("old", &at("old"));
    h.clock.advance(100_000);
    h.restart().unwrap();
    assert_eq!(
        h.life.running(),
        [
            RunningLease {
                lease_id: "lease-older".to_owned(),
                cgroup: None,
                started_usec: None,
                adopted: true,
            },
            RunningLease {
                lease_id: "lease-old".to_owned(),
                cgroup: Some(at("old").into()),
                started_usec: Some(60_000_000),
                adopted: true,
            },
        ]
    );
}

/// A container of `run` that an earlier agent started `running_ms` ago.
fn leak(h: &Harness, run: &str, running_ms: u64) {
    let lease = format!("lease-{run}");
    let labels = [
        (label::RUN, run),
        (label::LEASE, lease.as_str()),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::GPUS, ""),
        (label::WALL_MS, "900000"),
        (label::ISOLATION, "contained"),
    ];
    h.fake.leak(&labels, running_ms);
}

/// Asserts that a run started here whose control group the runtime could not give is asked
/// about again: the first call says so once, naming the run; once the runtime can say, the
/// run lists its control group and no line follows. Runs that are not running (one that
/// exited) and adopted runs (never sampled) are left alone and are never named. It catches
/// a lease that stays unsampled for its whole life after one failed lookup, a line at every
/// window, and lines about runs that no one samples.
#[test]
fn a_missing_cgroup_is_asked_again_and_told_once() {
    let mut h = Harness::new("running-late-cgroup");
    leak(&h, "older", 400_000);
    h.restart().unwrap();
    h.start(job("r1"));
    h.start(job("r3"));
    h.fake.exit("r3", 0);
    h.life.step();

    let told = h.life.find_cgroups();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("run r1 "), "{told:?}");
    assert!(h.life.find_cgroups().is_empty(), "told once");

    h.fake.set_cgroup("r1", &at("r1"));
    assert!(h.life.find_cgroups().is_empty());
    let found: Vec<_> = h
        .life
        .running()
        .into_iter()
        .map(|r| (r.lease_id, r.cgroup))
        .collect();
    assert_eq!(
        found,
        [
            ("lease-older".to_owned(), None),
            ("lease-r1".to_owned(), Some(at("r1").into())),
        ]
    );
}
