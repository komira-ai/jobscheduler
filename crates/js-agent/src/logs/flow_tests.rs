//! The uploader's flow beyond one stream: the budget against a slow store, the two streams
//! apart, a handle dropped without an end, and the check of a start's sink. The helpers are
//! those of `tests`.

use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{BookedResources, JobStartSpec, LogSink};
use js_logstore::fake::FakeStore;
use js_logstore::{
    HttpRequest, HttpResponse, JobPrefix, ManualClock, Stream, Transport, TransportError,
};

use super::Sink;
use super::tests::{Host, Lines, MIB, bytes, endpoint, puts};
use super::wake::Settled;

/// A store whose every request takes `by` on the uploader's clock.
struct Slow {
    store: FakeStore,
    clock: Arc<ManualClock>,
    by: Duration,
}

impl Transport for Slow {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.clock.advance(self.by);
        self.store.send(request)
    }
}

/// Catches a budget checked only between steps: at the end four chunks wait and each request
/// takes two minutes; the step stops before the request that would start past the five
/// minutes, with three chunks put and no final.
#[test]
fn the_budget_is_checked_before_every_request() {
    let host = Host::new("slow-store");
    let (wake, mut logs) = host.logs_for_test();
    logs.transport = Arc::new(Slow {
        store: host.store.clone(),
        clock: host.clock.clone(),
        by: Duration::from_secs(120),
    });
    let mut job = host.job_with("slow", (wake, logs));
    job.write(Stream::Stdout, &bytes(4 * MIB, 1));
    job.uploader.finish((4 * MIB as u64, 0));
    assert_eq!(job.step(), Settled::Ended);
    let mib = MIB as u64;
    let want: Vec<(String, usize)> = (0..3)
        .map(|n| (job.chunk(Stream::Stdout, n * mib), MIB))
        .collect();
    assert_eq!(puts(&host.ops()), want);
}

/// Catches one stream held back by the other: stdout's chunk cannot be read (its file was cut
/// short after the end), and its request fails at every try; stderr's chunk, final and delete
/// go out all the same, and the failure is logged.
#[test]
fn a_stream_that_keeps_failing_does_not_hold_the_other() {
    let lines = Lines::default();
    let _guard = lines.capture();
    let host = Host::new("streams-apart");
    let mut job = host.job("apart");
    job.write(Stream::Stdout, &bytes(100, 1));
    job.write(Stream::Stderr, &bytes(50, 2));
    job.step();
    job.uploader.finish((100, 50));
    File::create(&job.files[0]).expect("cut short");
    assert_eq!(job.step(), Settled::Asleep);
    let objects = host.store.objects();
    assert_eq!(
        objects[&job.prefix.final_key(Stream::Stderr)],
        job.written[1]
    );
    assert_eq!(
        objects.len(),
        1,
        "stderr's chunk was deleted, stdout has nothing"
    );
    assert!(
        lines.text().contains("read the capture file"),
        "{}",
        lines.text()
    );
}

/// Catches an uploader that outlives a run removed before its end: the handle is dropped
/// without `finish`, and the thread ends at its next step with nothing put.
#[test]
fn a_handle_dropped_without_an_end_stops_the_thread() {
    let host = Host::new("dropped");
    let mut job = host.job("dropped");
    job.write(Stream::Stdout, &bytes(10, 1));
    let wake = job.wake.clone();
    drop(job);
    assert_eq!(wake.step(), Settled::Ended);
    assert!(host.ops().is_empty());
}

/// A job start of `job_id` whose sink has the prefix `prefix`, or none.
fn start(job_id: &str, prefix: Option<String>) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: format!("alloc-{job_id}"),
        image: "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        command: Vec::new(),
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: prefix.map(|prefix| LogSink {
            endpoint: "http://127.0.0.1:9000".to_owned(),
            region: "test-1".to_owned(),
            bucket: "logs".to_owned(),
            prefix,
            path_style: true,
        }),
        kind: Default::default(),
    }
}

/// Catches a sink the store client could not use taken as good, or a good one mangled: no
/// sink is none; a good one is the endpoint and the job's prefix; a prefix the wire accepts
/// whose configured part is longer than the layout allows is refused naming the field and
/// not its value.
#[test]
fn a_start_s_sink_is_checked_against_the_layout() {
    assert_eq!(Sink::of_start(&start("j1", None)), Ok(None));
    let good = Sink::of_start(&start("j1", Some("js/j1/".to_owned())));
    let want = Sink::new(endpoint(), JobPrefix::new("js", "j1").expect("a prefix"));
    assert_eq!(good, Ok(Some(want)));
    let long = format!("{}/j1/", "a".repeat(513));
    let why = Sink::of_start(&start("j1", Some(long))).expect_err("too long");
    assert!(why.starts_with("log_sink.prefix: "), "{why}");
    assert!(!why.contains("aaaa"), "{why}");
}

/// Catches a `Debug` of the plan, a run's live logs or the limits that fails to say what it
/// is: each names its type and its cap.
#[test]
fn the_plan_and_a_run_s_live_logs_show_what_they_hold() {
    let plan = super::LogPlan::new(super::DEFAULT_FILE_CAP);
    let shown = format!(
        "{plan:?} {:?} {:?}",
        super::RunLogs::default(),
        super::Limits::default()
    );
    for word in ["LogPlan", "file_cap", "RunLogs", "Limits", "budget"] {
        assert!(shown.contains(word), "{word} in {shown}");
    }
}
