//! The control groups of the jobs that run: one the runtime could not name at the start is
//! asked about again and told once.

use js_agent::lifecycle::label;

use super::{Harness, job};

const SLICE: &str = "/sys/fs/cgroup/jobs.slice";

fn at(run: &str) -> String {
    format!("{SLICE}/docker-{run}.scope")
}

/// A container of `run` that an earlier agent started `running_ms` ago.
fn leak(h: &Harness, run: &str, running_ms: u64) {
    let lease = format!("lease-{run}");
    let labels = [
        (label::RUN, run),
        (label::LEASE, lease.as_str()),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::DISK_BYTES, "0"),
        (label::GPUS, ""),
        (label::WALL_MS, "900000"),
        (label::ISOLATION, "contained"),
        (label::KEYING, "job"),
    ];
    h.fake.leak(&labels, running_ms);
}

/// Asserts that a job started here whose control group the runtime could not give is asked
/// about again: the first call says so once, naming the job; once the runtime can say, no
/// line follows. Jobs that are not running (one that exited) and adopted jobs (never
/// sampled) are left alone and are never named. It catches a job that stays unsampled for
/// its whole life after one failed lookup, a line at every window, and lines about jobs
/// that no one samples.
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
}
