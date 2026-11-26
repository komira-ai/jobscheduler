//! Job rows of a real job store, exported as `key<TAB>base64(value)` lines, read back.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use js_cpu_replay::rows::{Facts, RowsError, read};
use js_cpu_replay::trace::End;
use js_jobs::{
    Booked, ClientName, Constraints, CpuPeak, EndReason, JobEnd, JobFiles, JobId, JobSpec,
    JobState, JobStore, Needs, NetworkPolicy, NewJob, Output, RandomIds, ResourceUsage,
};
use js_sched_placer::sizes::{JobSizes, SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{ManualClock, Store as _};
use js_types::{HostId, ImageDigestRef, Labels, Resources};

/// A scratch directory of one test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("rows_{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A job of class `class` (fallback `rust`) that books nothing.
fn spec(class: &str) -> JobSpec {
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), class.to_owned()),
        (SIZE_FALLBACK_LABEL.to_owned(), "rust".to_owned()),
    ]);
    JobSpec {
        image: ImageDigestRef::parse(&format!("team/app@sha256:{}", "a".repeat(64))).ok(),
        command: vec!["run".to_owned()],
        args: Vec::new(),
        env: BTreeMap::new(),
        needs: Needs::Shape(Resources::new(0, 0, 0, None).unwrap()),
        constraints: Constraints::default(),
        wall_limit_secs: 600,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: JobFiles::default(),
        labels: Labels::from_caller(&labels).unwrap(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        history_key: Default::default(),
        callback: None,
    }
}

/// A job store over a SQLite file in `dir`, on a clock that starts at 0.
fn source(
    dir: &std::path::Path,
) -> (
    Arc<SqliteStore>,
    JobStore<SqliteStore, RandomIds>,
    Arc<ManualClock>,
) {
    let clock = Arc::new(ManualClock::default());
    let store = SqliteStore::open(
        &dir.join("source.sqlite"),
        clock.clone(),
        Options::default(),
    )
    .unwrap();
    let store = Arc::new(store);
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).unwrap();
    (store, jobs, clock)
}

/// Queues job `id` of class `class` from client `farm`.
fn queue(jobs: &JobStore<SqliteStore, RandomIds>, id: &str, class: &str) -> JobId {
    let id = JobId::parse(id).unwrap();
    jobs.insert_queued(NewJob {
        id: Some(id.clone()),
        spec: spec(class),
        priority: 0,
        client: Some(ClientName::parse("farm").unwrap()),
    })
    .unwrap();
    id
}

/// The export line of `id`'s row.
fn line(store: &SqliteStore, id: &str) -> String {
    let row = store.get(&format!("job/{id}")).unwrap().unwrap();
    format!("job/{id}\t{}", STANDARD.encode(row.value))
}

#[test]
fn a_jobs_row_says_its_classes_times_end_and_usage() {
    let scratch = Scratch::new("facts");
    let (store, jobs, clock) = source(&scratch.0);
    let id = queue(&jobs, "a-1", "compile");
    clock.advance(Duration::from_secs(2));
    let booked = Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    };
    jobs.place(&id, HostId::new("node-1").unwrap(), booked)
        .unwrap();
    clock.advance(Duration::from_secs(3));
    jobs.transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&id, JobState::Starting, JobState::Running)
        .unwrap();
    clock.advance(Duration::from_secs(10));
    let usage = ResourceUsage {
        cpu_time: Duration::from_secs(20),
        peak_memory_bytes: 1 << 30,
        cpu_throttle: None,
        cpu_pressure_some: Some(Duration::from_millis(1500)),
        cpu_peak: Some(CpuPeak {
            millicores: 8000,
            window_usec: 5_000_000,
        }),
    };
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(usage),
            ..Output::default()
        },
    };
    let (ended, _) = jobs.end_and_free(&id, end, EndReason::Exited).unwrap();
    // Killed for memory after a second of run.
    let killed = queue(&jobs, "b-1", "link");
    jobs.place(&killed, HostId::new("node-2").unwrap(), booked)
        .unwrap();
    jobs.transition(&killed, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&killed, JobState::Starting, JobState::Running)
        .unwrap();
    clock.advance(Duration::from_secs(1));
    let memory = JobEnd::Memory {
        exit_code: None,
        output: Output::default(),
    };
    jobs.end_and_free(&killed, memory, EndReason::Memory)
        .unwrap();
    // Still queued: no result, no placement.
    queue(&jobs, "c-1", "test");

    let text = [
        line(&store, "a-1"),
        String::new(),
        "usage/node/node-1/0000000001\tAAAA".to_owned(),
        line(&store, "b-1"),
        line(&store, "c-1"),
    ]
    .join("\n");
    let read = read(&text, &scratch.0).unwrap();
    assert_eq!(read.len(), 3);
    assert_eq!(
        read["a-1"],
        Facts {
            class: format!("{:?}", JobSizes::key_of(&ended)),
            fallback: JobSizes::fallback_key_of(&ended).map(|key| format!("{key:?}")),
            client: format!("{:?}", JobSizes::client_key_of(&ended)),
            end: End::Ok,
            cpu_usec: Some(20_000_000),
            peak_milli: Some(8000),
            pressure_usec: Some(1_500_000),
            queued_ms: 0,
            placed_ms: Some(2000),
            started_ms: Some(5000),
            ended_ms: Some(15_000),
        }
    );
    assert!(read["a-1"].fallback.is_some());
    assert_ne!(read["a-1"].class, read["b-1"].class);
    assert_eq!(read["a-1"].client, read["b-1"].client);
    assert_eq!(read["b-1"].end, End::Memory);
    assert_eq!(read["b-1"].cpu_usec, None);
    assert_eq!(read["c-1"].end, End::Failed);
    assert_eq!(
        (
            read["c-1"].placed_ms,
            read["c-1"].started_ms,
            read["c-1"].ended_ms
        ),
        (None, None, None)
    );
    // A second read over the same scratch store writes its rows again.
    assert_eq!(super_read(&text, &scratch), read);
}

/// [`read`] again, into the same directory.
fn super_read(text: &str, scratch: &Scratch) -> BTreeMap<String, Facts> {
    read(text, &scratch.0).unwrap()
}

#[test]
fn each_bad_row_is_refused() {
    let scratch = Scratch::new("bad");
    assert_eq!(
        read("job/a no tab here", &scratch.0),
        Err(RowsError::Line {
            line: 1,
            what: "no tab"
        })
    );
    assert_eq!(
        read("\njob/a\tnot base64!", &scratch.0),
        Err(RowsError::Line {
            line: 2,
            what: "not base64"
        })
    );
    let garbage = format!("job/a\t{}", STANDARD.encode(b"not a row"));
    assert!(matches!(
        read(&garbage, &scratch.0),
        Err(RowsError::Row { job, .. }) if job == "a"
    ));
    let bad_id = format!("job/{}\t{}", "x".repeat(300), STANDARD.encode(b"not a row"));
    assert!(matches!(
        read(&bad_id, &scratch.0),
        Err(RowsError::Row { .. })
    ));
    let missing = scratch.0.join("missing");
    let error = read("", &missing).unwrap_err();
    assert!(matches!(error, RowsError::Store(_)), "{error}");
    assert!(error.to_string().starts_with("the scratch store: "));
    assert_eq!(
        RowsError::Row {
            job: "a".to_owned(),
            reason: "why".to_owned()
        }
        .to_string(),
        "the row of a: why"
    );
    assert_eq!(
        RowsError::Line {
            line: 2,
            what: "no tab"
        }
        .to_string(),
        "line 2 of the rows: no tab"
    );
}
