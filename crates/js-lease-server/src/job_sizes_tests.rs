//! The history the server teaches after its start ([`super::begin`], [`super::Sizes::teach`]):
//! the server's own job tables over a store that can fail a read and counts the reads made.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Read;
use js_jobs::{
    Booked, EndReason, JobEnd, JobId, JobState, Needs, NewJob, Output, ResourceUsage, Retention,
};
use js_registry::Placer as _;
use js_sched_placer::engine::Settings;
use js_sched_placer::engine::jobs::{NoStarts, OpenHost};
use js_sched_placer::place::SchedPlacer;
use js_types::{HostId, Resources, Shape, Timestamp};

use js_state::ManualClock;

use super::{Sizes, begin};
use crate::agent_api::job_rig::{spec, tables_on};
use crate::api::rig::{Lines, process_sink};
use crate::job_store::ServerJobs;
use crate::metrics::NoMetrics;

/// Log lines go to `lines` from this thread until the guard drops; the process-wide sink is
/// installed first, so that a line another thread reached first still reaches `lines`
/// ([`process_sink`]).
fn capture(lines: &Lines) -> tracing::subscriber::DefaultGuard {
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::set_default(subscriber)
}

/// The one line of `text` that holds `message`, without the module path: its level, message
/// and fields.
fn said(text: &str, message: &str) -> String {
    let said: Vec<&str> = text.lines().filter(|line| line.contains(message)).collect();
    assert_eq!(said.len(), 1, "{text}");
    said[0].replace("js_lease_server::job_sizes: ", "")
}

/// What a host of a measured run reported: 40 s of CPU and a 512 MiB peak.
const MEASURED: ResourceUsage = ResourceUsage {
    cpu_time: Duration::from_secs(40),
    peak_memory_bytes: 1 << 29,
    cpu_throttle: None,
    cpu_pressure_some: None,
    cpu_peak: None,
    peak_swap_bytes: None,
};

/// Runs the job `name` to its exit with `usage` as what its host reported.
fn ran(clock: &ManualClock, jobs: &ServerJobs, name: &str, usage: Option<ResourceUsage>) {
    let id = JobId::parse(name).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    let booked = Booked::Shape {
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    };
    jobs.place(&id, HostId::new("a").unwrap(), booked).unwrap();
    jobs.transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&id, JobState::Starting, JobState::Running)
        .unwrap();
    clock.advance(Duration::from_secs(60));
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage,
            ..Output::default()
        },
    };
    jobs.end_and_free(&id, end, EndReason::Exited).unwrap();
}

/// A placer, and the server's tables on a store that counts its reads.
fn tables() -> (
    Arc<ManualClock>,
    Arc<js_jobs::interfere::Interfere<js_state::fake::Fake>>,
    Arc<ServerJobs>,
    Arc<SchedPlacer>,
) {
    let clock = Arc::<ManualClock>::default();
    let (store, jobs) = tables_on(clock.clone(), Retention::DEFAULT);
    (clock, store, Arc::new(jobs), Arc::default())
}

/// Asserts that a teaching counts the ended jobs the store holds that taught a class a memory
/// sample, and no others; that a taught teaching teaches nothing a second time; that a start
/// whose read of the live allocations fails begins none (it teaches 0, and the start is not
/// refused); and that a read of the ended jobs that fails teaches none of them. Each outcome
/// logs its one line, whole. Catches a start
/// that forgets the history, one that counts a job that ended with no usage as taught, a
/// history taught twice, and a failed read that stops the server.
#[test]
fn a_teaching_counts_the_ended_jobs_and_a_failed_read_teaches_none() {
    let lines = Lines::default();
    let _guard = capture(&lines);
    let (clock, store, jobs, _) = tables();
    ran(&clock, &jobs, "unmeasured", None);
    let sizes = begin(&Arc::default(), &jobs);
    assert_eq!(sizes.teach(), 0);
    ran(&clock, &jobs, "measured", Some(MEASURED));
    let placer_clock = js_types::ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), placer_clock.clone());
    let sizes = begin(&Arc::new(placer), &jobs);
    placer_clock.advance(Duration::from_secs(7));
    lines.take();
    assert_eq!(sizes.teach(), 1);
    assert_eq!(
        said(&lines.take(), "were taught"),
        " INFO the job sizes were taught the runs of the ended jobs taught=1 pages=1 bridged=2 \
         took_secs=7"
    );
    assert_eq!(sizes.teach(), 0, "taught: nothing more");
    // A begin reads the newest runs and then the live allocations: fail the first read of these.
    store.take_reads();
    let _probe = begin(&Arc::default(), &jobs);
    let live = |read: &Read| match read {
        Read::Scan(prefix) => prefix.starts_with("idx/alloc/host/"),
        Read::Get(_) => false,
    };
    let newest = store.take_reads().iter().position(live).unwrap();
    store.fail_read_after(newest);
    let sizes = begin(&Arc::default(), &jobs);
    assert_eq!(sizes.teach(), 0, "no live allocations read: none begun");
    let sizes = begin(&Arc::default(), &jobs);
    store.fail_read_after(1);
    lines.take();
    assert_eq!(sizes.teach(), 0, "the ended jobs' read failed");
    assert_eq!(
        said(&lines.take(), "were not read"),
        format!(
            " WARN the ended jobs were not read: the job sizes learn from the jobs that end from \
             now on error={} pages=0",
            js_jobs::JobStoreError::Store(js_state::StoreError::Failed(
                js_jobs::interfere::INJECTED.to_owned()
            ))
        )
    );
}

/// Asserts that a start reads the newest runs and not the job table: 1001 ended jobs that
/// teach nothing and one measured run are two pages of freed allocations, read newest first
/// (at most [`js_sched_placer::sizes::BRIDGE_PAGES`]), with no scan of the job table; and that
/// the read after it pages through every freed allocation, two pages, and teaches the run.
/// Catches a start that reads the whole history before it listens, and a read that stops at
/// its first page.
#[test]
fn a_start_reads_the_newest_runs_alone_and_the_teaching_reads_every_page() {
    let (clock, store, jobs, placer) = tables();
    for at in 0..1001 {
        ran(&clock, &jobs, &format!("unmeasured-{at:04}"), None);
    }
    ran(&clock, &jobs, "measured", Some(MEASURED));
    let scans = |reads: Vec<Read>, of: &str| {
        let scanned = |read: &Read| matches!(read, Read::Scan(prefix) if prefix == of);
        reads.iter().filter(|&read| scanned(read)).count()
    };
    store.take_reads();
    let sizes = begin(&placer, &jobs);
    let begun = store.take_reads();
    assert_eq!(scans(begun.clone(), "job/"), 0, "the job table: {begun:?}");
    assert_eq!(scans(begun, "idx/alloc/freed/"), 2, "the newest runs");
    assert_eq!(sizes.teach(), 1);
    let read = store.take_reads();
    assert_eq!(scans(read.clone(), "job/"), 0, "the job table: {read:?}");
    assert_eq!(scans(read, "idx/alloc/freed/"), 2, "every page");
}

/// Asserts that a start after a read stopped before its last page (the server stopped, or
/// was released again, before its read of the ended jobs ended) books a class with runs from
/// them at its first pass after it listens: the store holds 1001 runs that teach nothing and
/// one measured run of the class (40 s of CPU over 60 s and a 512 MiB peak), the first start
/// is stopped before its first page, and the next start's first pass books a job of the class
/// that names nothing three quarters of a core and 1 GiB, not the cold size (two cores, a
/// quarter of the 8-core host, and 4 GiB). Catches a start whose jobs book the cold size until
/// a read of every ended job ends, which releases a few minutes apart never let end.
#[test]
fn a_start_after_a_read_stopped_before_its_last_page_books_a_class_with_runs_at_once() {
    let (clock, _store, jobs, first) = tables();
    for at in 0..1001 {
        ran(&clock, &jobs, &format!("unmeasured-{at:04}"), None);
    }
    ran(&clock, &jobs, "measured", Some(MEASURED));
    let stopped = begin(&first, &jobs);
    stopped.stop();
    assert_eq!(stopped.teach(), 0, "stopped before its first page");
    let placer = Arc::<SchedPlacer>::default();
    let host = HostId::new("a").unwrap();
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let resources = Resources::new(8000, 16 << 30, 0, None).unwrap();
    placer.host_up(&host, &Shape::new(resources, caps));
    let _sizes = begin(&placer, &jobs);
    let id = JobId::parse("next").unwrap();
    let mut naming_nothing = spec();
    naming_nothing.needs = Needs::Shape(Resources::new(0, 0, 0, None).unwrap());
    let new = NewJob {
        id: Some(id.clone()),
        spec: naming_nothing,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    let open = BTreeMap::from([(host, OpenHost::default())]);
    let placed = placer.place_jobs(jobs.as_ref(), &open, (&NoStarts, &NoMetrics));
    assert_eq!(placed.unwrap(), vec![id.clone()]);
    let booked = jobs.get_allocation(&id).unwrap().unwrap().booked;
    let learned = Booked::Shape {
        cpu_millis: 750,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    };
    assert_eq!(booked, learned);
}

/// Asserts that a stop before the read ends it before its first page: the teaching reads no
/// job and teaches none, logs the one line that says so, and a later call does nothing. Catches a stop that waits for the
/// whole history to be read.
#[test]
fn a_stop_ends_the_read_before_its_next_page() {
    let lines = Lines::default();
    let _guard = capture(&lines);
    let (clock, store, jobs, placer) = tables();
    ran(&clock, &jobs, "measured", Some(MEASURED));
    let sizes = begin(&placer, &jobs);
    sizes.stop();
    store.take_reads();
    lines.take();
    assert_eq!(sizes.teach(), 0);
    assert_eq!(store.take_reads(), Vec::new(), "no page read");
    assert_eq!(
        said(&lines.take(), "stopped"),
        " INFO the server stopped before the ended jobs were read: their runs were not taught \
         pages=0"
    );
    assert_eq!(sizes.teach(), 0);
}

/// Asserts that a server with no teaching begun (no job service) teaches nothing.
#[test]
fn no_teaching_begun_teaches_nothing() {
    assert_eq!(Sizes::default().teach(), 0);
}
