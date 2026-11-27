//! The history the server teaches after its start ([`super::begin`], [`super::Sizes::teach`]):
//! the server's own job tables over a store that can fail a read and counts the reads made.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Read;
use js_jobs::{
    Booked, EndReason, JobEnd, JobId, JobState, NewJob, Output, ResourceUsage, Retention,
};
use js_sched_placer::place::SchedPlacer;
use js_types::HostId;

use js_state::ManualClock;

use super::{Sizes, begin};
use crate::agent_api::job_rig::{spec, tables_on};
use crate::api::rig::{Lines, process_sink};
use crate::job_store::ServerJobs;

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
    let sizes = begin(&Arc::default(), &jobs);
    lines.take();
    assert_eq!(sizes.teach(), 1);
    assert_eq!(
        said(&lines.take(), "were taught"),
        " INFO the job sizes were taught the runs of the ended jobs taught=1 pages=1"
    );
    assert_eq!(sizes.teach(), 0, "taught: nothing more");
    store.fail_read_after(0);
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

/// Asserts that a start begins its teaching with no read of an ended job, and that the read
/// after it pages through every job: 1001 ended jobs that teach nothing and one measured run
/// are two pages, and the run is taught. Catches a start that reads the history before it
/// listens, and a read that stops at its first page.
#[test]
fn a_start_reads_no_ended_job_and_the_teaching_reads_every_page() {
    let (clock, store, jobs, placer) = tables();
    for at in 0..1001 {
        ran(&clock, &jobs, &format!("unmeasured-{at:04}"), None);
    }
    ran(&clock, &jobs, "measured", Some(MEASURED));
    store.take_reads();
    let sizes = begin(&placer, &jobs);
    let begun = store.take_reads();
    assert!(
        begun.iter().all(|read| match read {
            Read::Get(key) => !key.starts_with("job/") && !key.starts_with("alloc/"),
            Read::Scan(prefix) => !prefix.starts_with("job/") && !prefix.starts_with("alloc/"),
        }),
        "no ended job read at the start: {begun:?}"
    );
    assert_eq!(sizes.teach(), 1);
    let pages = store
        .take_reads()
        .into_iter()
        .filter(|read| matches!(read, Read::Scan(prefix) if prefix.starts_with("job/")))
        .count();
    assert_eq!(pages, 2);
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
