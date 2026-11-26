//! Why a host was not given a queued job: what the walk books of the scheduler's refusals,
//! what a pass logs of them, and what it tells the metrics.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_jobs::{JobId, Needs};
use js_registry::Placer;
use js_sched::{LoadConfig, Refusal, RefusalKind};
use js_types::{HostId, ManualClock, Resources, Shape, Timestamp};

use crate::engine::jobs::{JobHosts, NoStarts, OpenHost, PassMetrics};
use crate::engine::rig::{Tables, at, engine_with, every, host};
use crate::engine::{Engine, Settings};
use crate::place::SchedPlacer;

const GIB: u64 = 1 << 30;

fn need(cpu_millis: u64, memory_bytes: u64) -> Needs {
    Needs::Shape(Resources::new(cpu_millis, memory_bytes, 0, None).unwrap())
}

/// The host `a` offered to a walk.
fn open() -> BTreeMap<HostId, OpenHost> {
    BTreeMap::from([(host("a"), OpenHost::default())])
}

/// Walks one new job of `needs` named `id` on `e`; whether it was placed.
fn walk(e: &mut Engine, tables: &Tables, id: &str, needs: Needs) -> bool {
    let job = tables.submit_with(id, needs, 600);
    let (decided, _) = e.walk_jobs(&[job], &open(), at(0));
    decided.len() == 1
}

/// An engine over the host `a` of 8 cores and 16 GiB, under `load`.
fn engine(load: LoadConfig) -> Engine {
    let settings = Settings {
        load,
        ..Settings::default()
    };
    engine_with(settings, &every(&[("a", &[])]))
}

/// Behaviour: a host that holds a job booking eight cores refuses a job of two cores for the
/// CPU slack, and the walk books it for the host with the job's name and the figures:
/// 8000 m charged, 2000 m new, 8800 m allowed (the default slack of 1.1).
#[test]
fn a_host_full_of_cpu_books_a_cpu_refusal_with_its_figures() {
    let (mut e, tables) = (engine(LoadConfig::default()), Tables::new());
    assert!(walk(&mut e, &tables, "j1", need(8_000, GIB)));
    assert!(!walk(&mut e, &tables, "j2", need(2_000, GIB)));
    let refused = e.take_refused();
    assert_eq!(refused.count(&host("a"), RefusalKind::Cpu), 1);
    let (job, latest) = refused.latest(&host("a"), RefusalKind::Cpu).unwrap();
    assert_eq!(job.to_string(), "j2");
    assert_eq!(
        *latest,
        Refusal {
            kind: RefusalKind::Cpu,
            counted: 8_000,
            new: 2_000,
            limit: 8_800,
            measured_cpu_millis: None,
        }
    );
    assert_eq!(refused.count(&host("a"), RefusalKind::PredictedMemory), 0);
    assert!(e.take_refused().is_empty(), "taking empties the book");
}

/// Behaviour: a host holding a job of 12 GiB refuses one of 8 GiB for predicted memory, with
/// the memory counted and the job's as the figures.
#[test]
fn a_host_full_of_memory_books_a_predicted_memory_refusal() {
    let (mut e, tables) = (engine(LoadConfig::default()), Tables::new());
    assert!(walk(&mut e, &tables, "j1", need(1_000, 12 * GIB)));
    assert!(!walk(&mut e, &tables, "j2", need(1_000, 8 * GIB)));
    let refused = e.take_refused();
    let (_, latest) = refused
        .latest(&host("a"), RefusalKind::PredictedMemory)
        .unwrap();
    assert_eq!((latest.counted, latest.new), (12 * GIB, 8 * GIB));
    assert_eq!(refused.count(&host("a"), RefusalKind::Cpu), 0);
}

/// Behaviour: with memory slack on the predicate, the flat memory guard is what refuses the
/// second job, and the walk books it as booked memory with the flat sum held.
#[test]
fn a_host_over_the_flat_guard_books_a_booked_memory_refusal() {
    let load = LoadConfig {
        s_mem_permille: 500,
        ..LoadConfig::default()
    };
    let (mut e, tables) = (engine(load), Tables::new());
    assert!(walk(&mut e, &tables, "j1", need(1_000, 12 * GIB)));
    assert!(!walk(&mut e, &tables, "j2", need(1_000, 8 * GIB)));
    let refused = e.take_refused();
    let (_, latest) = refused
        .latest(&host("a"), RefusalKind::BookedMemory)
        .unwrap();
    assert_eq!((latest.counted, latest.new), (12 * GIB, 8 * GIB));
    assert_eq!(refused.count(&host("a"), RefusalKind::PredictedMemory), 0);
}

/// What a test's own subscriber writes.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

impl Lines {
    fn take(&self) -> String {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(std::mem::take(&mut *held)).unwrap()
    }
}

/// Installs once per process a global subscriber that wants every line and writes it
/// nowhere, so no thread settles a line as never wanted before the capturing test reaches it.
fn process_sink() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(io::sink)
            .try_init();
    });
}

/// What a pass told the metrics of the refusals.
#[derive(Default)]
struct Told(Mutex<Vec<(HostId, &'static str)>>);

impl PassMetrics for Told {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {}

    fn refused(&self, host: &HostId, reason: &'static str) {
        self.0.lock().unwrap().push((host.clone(), reason));
    }
}

/// Behaviour: in a pass where a host refuses a queued job for CPU (the job behind it is not
/// offered the host: a waiting job closes it), the pass logs one debug line for the refusal
/// naming the job, the host, the reason and the figures, one info line for the host with the
/// count and the figures of the latest refusal (so the default log level says why the host is
/// not full), and tells the metrics the refusal by host and reason. A second pass logs its own
/// info line and tells the metrics again.
#[test]
fn a_pass_logs_one_line_per_host_and_counts_each_refusal() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(8_000, 16 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, at(0));
    placer.heard(&host("a"), None);
    let tables = Tables::new();
    let (open, hello) = (open(), BTreeSet::from([host("a")]));
    let on = JobHosts {
        open: &open,
        hello: &hello,
        tainted: &std::collections::BTreeSet::new(),
        whole: None,
    };
    let told = Told::default();
    let pass = || {
        placer
            .place_jobs_with(&tables.jobs, &on, (&NoStarts, &told))
            .unwrap()
    };
    tables.submit_with("j1", need(8_000, GIB), 600);
    assert_eq!(pass(), vec![JobId::parse("j1").unwrap()]);
    assert!(told.0.lock().unwrap().is_empty(), "nothing refused yet");
    let _ = lines.take();
    tables.submit_with("j2", need(2_000, GIB), 600);
    tables.submit_with("j3", need(2_000, GIB), 600);
    for round in 1..=2 {
        assert!(pass().is_empty());
        let logged = lines.take();
        let summary: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("a host refused queued jobs in this pass"))
            .collect();
        assert_eq!(summary.len(), 1, "{logged}");
        assert!(summary[0].starts_with(" INFO "), "{}", summary[0]);
        assert!(summary[0].contains("host=a"), "{}", summary[0]);
        assert!(
            summary[0].contains(
                "cpu x1 (latest job j2: counted 8000m + new 2000m > limit 8800m (no fresh load reading: charged by bookings))"
            ),
            "{}",
            summary[0]
        );
        let each: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("a host refused a queued job"))
            .collect();
        assert_eq!(each.len(), 1, "{logged}");
        assert!(each[0].starts_with("DEBUG "), "{}", each[0]);
        assert!(
            each[0].contains("job=j2 host=a reason=\"cpu\" detail=counted 8000m"),
            "{}",
            each[0]
        );
        assert_eq!(told.0.lock().unwrap().len(), round);
    }
    assert_eq!(told.0.lock().unwrap()[0], (host("a"), "cpu"));
}

/// Behaviour: a host whose live bookings fill the memory it offers (3 of 4 GiB booked by a job
/// that fit and was placed, a job of 2 GiB queued, its CPU free) is refused the job by the
/// walk's ledger before the scheduler sees it, and the pass says so as it says the scheduler's
/// refusals: one info line for the host, whose refusals read in full
/// `booked-memory x1 (latest job j2: counted 3072MiB + new 2048MiB > limit 4096MiB)`, and the
/// metrics told the refusal by host and reason. Catches a host full by memory dropped with
/// nothing said, so that every refusal the log shows is a CPU one although memory is what
/// filled the host.
#[test]
fn a_host_full_by_memory_bookings_is_refused_for_memory_and_says_so() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(8_000, 4 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, at(0));
    placer.heard(&host("a"), None);
    let tables = Tables::new();
    let offer = OpenHost {
        capacity: Some(js_jobs::Capacity {
            cpu_millis: 8_000,
            memory_bytes: 4 * GIB,
            disk_bytes: 0,
            accelerators: 0,
        }),
        ..OpenHost::default()
    };
    let (open, hello) = (
        BTreeMap::from([(host("a"), offer)]),
        BTreeSet::from([host("a")]),
    );
    let on = JobHosts {
        open: &open,
        hello: &hello,
        tainted: &std::collections::BTreeSet::new(),
        whole: None,
    };
    let told = Told::default();
    let pass = || {
        placer
            .place_jobs_with(&tables.jobs, &on, (&NoStarts, &told))
            .unwrap()
    };
    tables.submit_with("j1", need(1_000, 3 * GIB), 600);
    assert_eq!(pass(), vec![JobId::parse("j1").unwrap()]);
    let _ = lines.take();
    tables.submit_with("j2", need(1_000, 2 * GIB), 600);
    assert!(pass().is_empty());
    let logged = lines.take();
    let summary: Vec<&str> = logged
        .lines()
        .filter(|line| line.contains("a host refused queued jobs in this pass"))
        .collect();
    assert_eq!(summary.len(), 1, "{logged}");
    let refusals = summary[0].split("refusals=").nth(1).unwrap();
    assert_eq!(
        refusals,
        "booked-memory x1 (latest job j2: counted 3072MiB + new 2048MiB > limit 4096MiB)"
    );
    assert_eq!(*told.0.lock().unwrap(), vec![(host("a"), "booked-memory")]);
}

/// Behaviour: refusals the scheduler's buffer could not keep are said once in a warning that
/// gives their number, and a pass that saw none says nothing.
#[test]
fn dropped_refusals_are_announced_with_their_number() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let told = Told::default();
    super::Refused::default().announce(&told);
    assert_eq!(lines.take(), "");
    let dropped = super::Refused {
        dropped: 7,
        ..super::Refused::default()
    };
    assert!(!dropped.is_empty());
    dropped.announce(&told);
    let logged = lines.take();
    assert!(logged.starts_with(" WARN "), "{logged}");
    assert!(logged.contains("dropped=7"), "{logged}");
    assert!(told.0.lock().unwrap().is_empty());
}

/// Behaviour: a host refused for two kinds in one pass has one info line that names both,
/// each with its count and its latest figures in its own unit (a cold cap counts cold
/// placements), the metrics are told each refusal, and a book with a host in it is not empty.
#[test]
fn a_host_refused_for_two_kinds_says_both_on_one_line() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let refusal = |kind, counted, new, limit| js_sched::Refusal {
        kind,
        counted,
        new,
        limit,
        measured_cpu_millis: None,
    };
    let job = JobId::parse("j9").unwrap();
    let kinds = BTreeMap::from([
        (
            js_sched::RefusalKind::BookedMemory,
            (
                2,
                job.clone(),
                refusal(
                    js_sched::RefusalKind::BookedMemory,
                    20 * GIB,
                    16 * GIB,
                    32 * GIB,
                ),
            ),
        ),
        (
            js_sched::RefusalKind::ColdCap,
            (
                1,
                job.clone(),
                refusal(js_sched::RefusalKind::ColdCap, 3, 1, 3),
            ),
        ),
    ]);
    let refused = super::Refused {
        hosts: BTreeMap::from([(host("a"), kinds)]),
        dropped: 0,
    };
    assert!(!refused.is_empty());
    let told = Told::default();
    refused.announce(&told);
    let logged = lines.take();
    assert!(
        logged.contains(
            "booked-memory x2 (latest job j9: counted 20480MiB + new 16384MiB > limit 32768MiB); \
             cold-cap x1 (latest job j9: counted 3cold + new 1cold > limit 3cold)"
        ),
        "{logged}"
    );
    assert_eq!(told.0.lock().unwrap().len(), 3);
}

/// Behaviour: on a host whose fresh readings measure 9000 m while its one job books 4000 m,
/// a job of 200 m is refused for CPU, and the info line of the pass carries the figures the
/// predicate used: the measurement counted (9000 m) and the measurement itself, where a host
/// with no fresh reading says it was charged by its bookings (see
/// [`a_pass_logs_one_line_per_host_and_counts_each_refusal`]). Catches a refusal line that
/// prints a measurement of 0 m for a host whose load was measured.
#[test]
fn a_measured_hosts_refusal_line_carries_the_measurement_it_used() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (mut e, tables) = (engine(LoadConfig::default()), Tables::new());
    assert!(walk(&mut e, &tables, "j1", need(4_000, GIB)));
    let reading = |seq: u64| js_types::sched::usage::NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 45_000_000,
        memory_working_set_bytes: GIB,
        ..Default::default()
    };
    e.heard(&host("a"), Some(&reading(1)), at(0));
    e.heard(&host("a"), Some(&reading(2)), at(0));
    assert!(!walk(&mut e, &tables, "j2", need(200, GIB)));
    e.take_refused().announce(&Told::default());
    let logged = lines.take();
    assert!(
        logged.contains(
            "refusals=cpu x1 (latest job j2: counted 9000m + new 200m > limit 8800m \
             (measured 9000m))"
        ),
        "{logged}"
    );
}

/// Behaviour: a refusal of the kind a caller reports for a worker whose CPU it judges
/// (`cpu-curve`) is spelled in millicores as a CPU refusal is, with no reading note: the
/// figures read `counted 9000m + new 2000m > limit 10000m` and nothing else.
#[test]
fn a_cpu_curve_refusal_is_spelled_in_millicores() {
    let refusal = Refusal {
        kind: RefusalKind::CpuCurve,
        counted: 9_000,
        new: 2_000,
        limit: 10_000,
        measured_cpu_millis: None,
    };
    assert_eq!(
        super::figures(&refusal),
        "counted 9000m + new 2000m > limit 10000m"
    );
}
