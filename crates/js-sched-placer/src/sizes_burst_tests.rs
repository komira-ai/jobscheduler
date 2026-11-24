//! A burst of a heavy-tail class over six equal build-farm hosts: the class's runs average a
//! few cores and burst to many, and the burst is placed in one pass with no load reading.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{Booked, CpuPeak, JobEnd, JobId, NewJob, Output, ResourceUsage};
use js_registry::Placer as _;
use js_types::{ManualClock, Resources, Shape, Timestamp};

use super::{GIB, Quiet, Tables, at, booked, host, needs, of_class, ran};
use crate::engine::Settings;
use crate::engine::jobs::{JobHosts, OpenHost};
use crate::place::SchedPlacer;

/// The farm's host: 88 cores and 251 GiB.
const HOST: (u64, u64) = (88_000, 251 * GIB);

/// The six hosts.
const HOSTS: [&str; 6] = ["a", "b", "c", "d", "e", "f"];

/// The five-second peaks of the class's ten runs, in millicores: 16 to 70 cores, each run
/// averaging 2.75 cores over its ten seconds.
const PEAKS: [u64; 10] = [
    16_000, 16_000, 16_000, 17_000, 18_000, 20_000, 30_000, 40_000, 52_000, 70_000,
];

/// The jobs of the burst.
const BURST: usize = 24;

/// A placer over the six hosts with the default cold size, on a clock at zero.
fn six_hosts() -> SchedPlacer {
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let resources = Resources::new(HOST.0, HOST.1, 0, None).unwrap();
    for name in HOSTS {
        placer.engine().host_up(
            &host(name),
            &Shape::new(resources.clone(), caps.clone()),
            at(0),
        );
    }
    placer
}

/// An exit with code 0 after ten seconds at 2.75 cores on average and 2 GiB, whose host
/// reported a five-second peak of `peak` millicores (`None`: an agent from before the peak).
fn compiled(peak: Option<u64>) -> JobEnd {
    let usage = ResourceUsage {
        cpu_time: Duration::from_millis(27_500),
        peak_memory_bytes: 2 * GIB,
        cpu_throttle: None,
        cpu_pressure_some: Some(Duration::ZERO),
        cpu_peak: peak.map(|millicores| CpuPeak {
            millicores,
            window_usec: 5_000_000,
        }),
    };
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(usage),
            ..Output::default()
        },
    }
}

/// Places a burst of [`BURST`] jobs of the class `node.tools`, whose history is its ten runs of
/// [`PEAKS`] (`peaks` false: the same runs with no peak reported), over the six hosts in one
/// pass; answers the CPU booked on each host, by host.
fn burst(peaks: bool) -> BTreeMap<String, u64> {
    let tables = Tables::new();
    let placer = six_hosts();
    for (run, peak) in PEAKS.into_iter().enumerate() {
        let spec = of_class("node.tools", needs(0, 0));
        let end = compiled(peaks.then_some(peak));
        ran(
            &tables,
            (&format!("seen-{run}"), None),
            spec,
            booked(4000, 4 * GIB),
            end,
        );
    }
    placer.learn_ended(&tables.jobs).unwrap();
    for at in 0..BURST {
        let new = NewJob {
            id: Some(JobId::parse(&format!("q-{at:02}")).unwrap()),
            spec: of_class("node.tools", needs(0, 0)),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
    }
    let open: BTreeMap<_, _> = HOSTS
        .iter()
        .map(|name| {
            placer.heard(&host(name), None);
            (host(name), OpenHost::default())
        })
        .collect();
    let hello = std::collections::BTreeSet::new();
    let tainted = std::collections::BTreeSet::new();
    let hosts = JobHosts {
        open: &open,
        hello: &hello,
        whole: None,
        tainted: &tainted,
    };
    let placed = placer
        .place_jobs_with(&tables.jobs, &hosts, (&Quiet, &Quiet))
        .unwrap();
    let mut by_host = BTreeMap::new();
    for id in &placed {
        let allocation = tables.jobs.get_allocation(id).unwrap().unwrap();
        let Booked::Shape { cpu_millis, .. } = allocation.booked else {
            panic!("a share")
        };
        *by_host
            .entry(allocation.host.as_str().to_owned())
            .or_insert(0) += cpu_millis;
    }
    by_host
}

/// Asserts that a burst of a heavy-tail class over six equal hosts books no host past its
/// cores (1.0x) while another host has room: the class's ten runs averaged 2.75 cores and
/// peaked at 16 to 70, so each job books the 90th percentile of the peaks (52 cores) held to a
/// quarter of a host (22 cores), and the 24 jobs of the burst are four on each host, 88 cores
/// booked on each. With the same runs reported without peaks, each job books its class's
/// median (2.75 cores) and the whole burst lands on one host, 66 cores booked for jobs whose
/// peaks are at least 16 cores each (384 together) on a host of 88, while the other five stay
/// empty. Catches a heavy-tail class booked at its mean, which lets a burst of its jobs run one
/// host far past its cores while the others stay idle.
#[test]
fn a_burst_of_a_heavy_tail_class_books_no_host_past_its_cores_while_another_fits() {
    let tailed = burst(true);
    assert_eq!(
        tailed.len(),
        HOSTS.len(),
        "every host takes a share: {tailed:?}"
    );
    for (name, cpu) in &tailed {
        assert_eq!(*cpu, HOST.0, "host {name} booked to its cores, not past");
    }
    let median = burst(false);
    let one_host = BTreeMap::from([("a".to_owned(), 2750 * BURST as u64)]);
    assert_eq!(median, one_host, "the burst at the median, on one host");
}
