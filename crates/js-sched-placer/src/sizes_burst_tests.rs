//! A burst of a class over six equal build-farm hosts: the class's runs average a few cores
//! and burst to many over five seconds, and the burst is placed in one pass with no load
//! reading.

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

/// The CPU time of each of those runs: 2.75 cores over ten seconds.
const CPU_TIME: Duration = Duration::from_millis(27_500);

/// The five-second peaks of ten runs of a compile shaped like the farm's Mojo compiles, in
/// millicores: 16 to 66 cores (the farm's were 20.8 at the median and 37.6 at the 90th
/// percentile), each run averaging 3.6 cores over its ten seconds.
const COMPILE_PEAKS: [u64; 10] = [
    16_000, 18_000, 20_000, 21_000, 24_000, 28_000, 31_000, 34_000, 38_000, 66_000,
];

/// The CPU time of each of those runs: 3.6 cores over ten seconds.
const COMPILE_CPU_TIME: Duration = Duration::from_secs(36);

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

/// An exit with code 0 that used `cpu_time` and 2 GiB (over the ten seconds [`ran`] gives a
/// run), whose host reported a five-second peak of `peak` millicores (`None`: an agent from
/// before the peak).
fn compiled(cpu_time: Duration, peak: Option<u64>) -> JobEnd {
    let usage = ResourceUsage {
        cpu_time,
        peak_memory_bytes: 2 * GIB,
        cpu_throttle: None,
        cpu_pressure_some: Some(Duration::ZERO),
        cpu_peak: peak.map(|millicores| CpuPeak {
            millicores,
            window_usec: 5_000_000,
        }),
        peak_swap_bytes: None,
    };
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(usage),
            ..Output::default()
        },
    }
}

/// Places a burst of `jobs` jobs of the class `node.tools`, whose history is one run for each
/// of `peaks` that used `cpu_time` (`reported` false: the same runs with no peak reported),
/// over the six hosts in one pass; answers the CPU booked on each host, by host.
fn burst(
    (cpu_time, peaks): (Duration, &[u64]),
    reported: bool,
    jobs: usize,
) -> BTreeMap<String, u64> {
    let tables = Tables::new();
    let placer = six_hosts();
    for (run, peak) in peaks.iter().enumerate() {
        let spec = of_class("node.tools", needs(0, 0));
        let end = compiled(cpu_time, reported.then_some(*peak));
        ran(
            &tables,
            (&format!("seen-{run}"), None),
            spec,
            booked(4000, 4 * GIB),
            end,
        );
    }
    placer.learn_ended(&tables.jobs).unwrap();
    for at in 0..jobs {
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

/// Asserts that a burst of a class whose runs burst far over their mean books the class's mean
/// and is spread by the pack limit, not by its peaks: the class's ten runs averaged 2.75 cores
/// and peaked at 16 to 70, and each of the 24 jobs books the median of the means (2.75 cores)
/// whether the runs reported their peaks or not. The pack limit fills the first host to half
/// its cores (16 jobs, 44 cores booked) and the other 8 go to the second (22 cores), each host
/// well under its cores by booking; a burst that uses its peaks at once shares the host's CPU
/// by weight and the measured load holds the next pass. Catches the peaks booked again (the 90th
/// percentile, 52 cores, held to a quarter of the host: four jobs of 22 cores on each host).
#[test]
fn a_burst_of_a_class_with_high_peaks_books_its_mean_and_is_spread_by_the_pack_limit() {
    let half = HOST.0 / 2;
    let two_hosts = BTreeMap::from([
        ("a".to_owned(), half),
        ("b".to_owned(), 2750 * BURST as u64 - half),
    ]);
    for reported in [true, false] {
        assert_eq!(
            burst((CPU_TIME, &PEAKS), reported, BURST),
            two_hosts,
            "peaks reported: {reported}"
        );
    }
}

/// Asserts that a compile shaped like the farm's Mojo compiles, whose ten runs averaged 3.6
/// cores and peaked at 16 to 66 cores over five seconds, books 3.75 cores (its mean rounded up
/// to a quarter core) on the farm's 88-core hosts, the same as when its runs reported no peak.
/// The boundaries: the median of the means (3.6 cores) is what is booked, not the 90th
/// percentile of the peaks (38 cores, over twice the mean) held to the quarter of the host
/// (22 cores), and a peak that is reported changes nothing. Catches the compile booked at
/// 22 cores, which held each host to four of them while they used about 15 of its 88.
#[test]
fn a_compile_averaging_3_6_cores_with_high_five_second_peaks_books_3_75_cores_not_22() {
    for reported in [true, false] {
        let booked = burst((COMPILE_CPU_TIME, &COMPILE_PEAKS), reported, 1);
        assert_eq!(
            booked,
            BTreeMap::from([("a".to_owned(), 3750)]),
            "peaks reported: {reported}"
        );
    }
}
