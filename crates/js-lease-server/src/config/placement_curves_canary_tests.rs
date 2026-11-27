//! What a server started on the curve flags places: the command line `--placement-curves on`
//! with and without `--curve-hosts`, read by the code a start reads it with ([`super::of_flags`]),
//! drives the server's placer over a class it learned from ended runs that recorded a CPU series.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    Booked, CpuThrottle, EndReason, JobEnd, JobId, JobSpec, JobState, Needs, NewJob, Output,
    ResourceUsage,
};
use js_types::{CpuSeries, HostId, Labels, Resources, SIZE_CLASS_LABEL};

use crate::agent_api::job_rig::{FakeAgent, Rig, curve_rig, placed, spec};
use crate::config::Cli;

/// The seconds each point of a run's CPU series covers.
const GRANULARITY: u32 = 2;

/// The points of every run of the class `burst`, each a sixteenth of a core: 1.5 cores for the
/// first 20 s and a quarter core for the 120 s after. Its curve books 1500 m at a job's start;
/// its flat number, the mean, is under 500 m.
fn series_points() -> Vec<u16> {
    let mut points = vec![4; 70];
    points[..10].fill(24);
    points
}

/// The wall time of every run of `burst`: the series' 140 s.
const RUN: Duration = Duration::from_secs(140);

/// A job of the class `burst` that leaves its CPU and memory at 0, so it books what its class
/// learned.
fn of_burst() -> JobSpec {
    let mut spec = spec();
    spec.needs = Needs::Shape(Resources::new(0, 0, 0, None).unwrap());
    let labels = BTreeMap::from([(SIZE_CLASS_LABEL.to_owned(), "burst".to_owned())]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    spec
}

/// How a run of `burst` ends: exited 0, its CPU time the series', its peak memory 1 GiB, never
/// throttled, with the series and no five-second peak.
fn burst_end() -> JobEnd {
    let points = series_points();
    let sixteenths: u64 = points.iter().map(|point| u64::from(*point)).sum();
    let cpu_time = Duration::from_micros(sixteenths * u64::from(GRANULARITY) * 62_500);
    let usage = ResourceUsage {
        cpu_time,
        peak_memory_bytes: 1 << 30,
        cpu_throttle: Some(CpuThrottle {
            periods: 0,
            throttled_periods: 0,
        }),
        cpu_pressure_some: Some(Duration::ZERO),
        cpu_peak: None,
        peak_swap_bytes: None,
    };
    let run_usec = u64::try_from(RUN.as_micros()).unwrap();
    let output = Output {
        usage: Some(usage),
        cpu_series: CpuSeries::new(GRANULARITY, points, run_usec, false),
        ..Output::default()
    };
    JobEnd::Exited {
        exit_code: 0,
        output,
    }
}

/// Records the job `id` of `burst` queued.
fn queue(rig: &Rig, id: &str) -> JobId {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: of_burst(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    id
}

/// Runs 24 jobs of `burst` on `host-a` one after the other, each for [`RUN`], and teaches the
/// server's placer the ended jobs as a start does: enough runs, all alike, for the class to pass
/// its four checks and book its curve. It moves the clock past any host's grace, so the hosts
/// register after it.
fn teach_burst(rig: &Rig) {
    let host = HostId::new("host-a").unwrap();
    for at in 0..24 {
        let id = queue(rig, &format!("burst-{at:02}"));
        let booked = Booked::Shape {
            cpu_millis: 2000,
            memory_bytes: 4 << 30,
            disk_bytes: 0,
            accelerators: 0,
        };
        rig.jobs.place(&id, host.clone(), booked).unwrap();
        let jobs = &rig.jobs;
        jobs.transition(&id, JobState::Placed, JobState::Starting)
            .unwrap();
        jobs.transition(&id, JobState::Starting, JobState::Running)
            .unwrap();
        rig.advance(RUN);
        jobs.end_and_free(&id, burst_end(), EndReason::Exited)
            .unwrap();
    }
    let placer = rig.agents.placer.clone().unwrap();
    assert_eq!(crate::job_sizes::begin(&placer, &rig.jobs).teach(), 24);
}

/// A server on the command line `--placement-curves on` and `extra`, read as a start reads it,
/// whose hosts (8 cores and a CPU slack of 1.1: 8800 m) are `host-a`, holding four jobs that
/// name 1500 m each (6000 m at every slot) and then the first job of `burst`, and `host-b`,
/// empty, which registers after it and takes the second: where the two jobs were placed and the
/// fullest booked slot of each curve host after the second pass.
async fn placed_by_flags(name: &str, extra: &[&str]) -> (HostId, HostId, BTreeMap<HostId, u64>) {
    let mut line = vec![
        "js-lease-server",
        "--state-file",
        "state",
        "--boot",
        "initialise",
    ];
    line.extend(["--token-file", "token", "--placement-curves", "on"]);
    line.extend_from_slice(extra);
    let cli = Cli::try_parse_from(line).unwrap();
    let mut rig = curve_rig(name, super::of_flags(&cli).unwrap());
    teach_burst(&rig);
    rig.register("host-a", true);
    let named = Booked::Shape {
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    };
    for at in 0..4 {
        placed(&rig.jobs, &format!("fill-{at}"), "host-a", spec(), named);
    }
    let first = queue(&rig, "first");
    rig.beat(&FakeAgent::new("host-a")).await.unwrap();
    rig.register("host-b", true);
    let second = queue(&rig, "second");
    rig.beat(&FakeAgent::new("host-b")).await.unwrap();
    let view = rig.agents.placer.as_ref().unwrap().curve_view().unwrap();
    let on = |id: &JobId| rig.allocation(id).host;
    (on(&first), on(&second), view.host_peaks)
}

/// Asserts the canary through the flags. With `--curve-hosts host-a`, the job on the listed
/// `host-a` is booked from its curve (its fullest slot is the 6000 m of the jobs there and the
/// 1500 m its curve books at its start, where its flat number would make it under 6500 m), and
/// the second job, which `host-a`'s curves have no room for (7500 m and 1500 m more is over
/// 8800 m) but whose flat number fits on `host-b`, is placed flat there: `host-b` books no curve
/// and is no curve host. Without `--curve-hosts`, `on` books by curve on every host: the second
/// job goes to `host-b` by its curve, which `host-b` then books (1500 m). Catches a flag that
/// does not reach the placer, a list that leaves the listed host flat, and a host off the list
/// that books curves.
#[tokio::test]
async fn the_flags_book_curves_on_the_listed_hosts_alone_and_on_every_host_without_a_list() {
    let (a, b) = (
        HostId::new("host-a").unwrap(),
        HostId::new("host-b").unwrap(),
    );
    let listed = placed_by_flags("curve_flags_canary", &["--curve-hosts", "host-a"]).await;
    assert_eq!(
        listed,
        (a.clone(), b.clone(), BTreeMap::from([(a.clone(), 7500)]))
    );
    let every = placed_by_flags("curve_flags_every_host", &[]).await;
    let peaks = BTreeMap::from([(a.clone(), 7500), (b.clone(), 1500)]);
    assert_eq!(every, (a, b, peaks));
}
