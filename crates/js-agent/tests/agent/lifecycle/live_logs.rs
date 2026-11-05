//! The live-log sink of a job start through the lifecycle: checked once, handed to the
//! runtime in the run's spec, and the runtime's report carried in the job's result. The
//! runtime is the fake; the uploader itself is the `logs` module's and the stand-in's.

use js_agent::lifecycle::code;
use js_agent::logs::Sink;
use js_agent_wire::{BookedResources, Command, JobStartSpec, LogReport, LogSink, Outcome};

use super::{Harness, job};

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start of `job_id` whose sink names the prefix `prefix` in the bucket `bucket`.
fn start(job_id: &str, bucket: &str, prefix: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: format!("alloc-{job_id}"),
        image: IMAGE.to_owned(),
        command: vec!["--go".to_owned()],
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
        log_sink: Some(LogSink {
            endpoint: "https://store.example".to_owned(),
            region: "test-1".to_owned(),
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            path_style: false,
        }),
    }
}

fn report() -> LogReport {
    LogReport {
        stdout_bytes: 3_000_000,
        stderr_bytes: 12,
        stdout_dropped: 900_000,
        stderr_dropped: 0,
    }
}

/// Proves a checked sink reaches the runtime and the runtime's report reaches the result: the
/// run's spec holds the sink the start's checks give, and the job's listed result carries
/// `logs` beside its outcome. A start that waits for its image keeps its sink until the
/// container is made; a run start and a job start without a sink hand none, and a job
/// whose runtime reports nothing lists no `logs`. Catches a sink lost on the way, a report
/// left out of the result, or one made up for a run that has none.
#[test]
fn a_checked_sink_reaches_the_runtime_and_its_report_the_result() {
    let mut h = Harness::new("live-sink");
    let given = start("j1", "logs", "js/j1/");
    let want = Sink::of_start(&given).expect("a good sink");
    h.apply(vec![Command::JobStart(Box::new(given))]);
    h.fake.report_logs("j1", report());
    h.fake.exit("j1", 0);
    h.life.step();
    let listed = h.life.held_jobs();
    let result = listed[0].result.as_ref().expect("a result");
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(result.logs, Some(report()));

    h.fake.pull_pending(IMAGE);
    let mut bare = start("j2", "logs", "js/j2/");
    bare.log_sink = None;
    h.apply(vec![
        Command::JobStart(Box::new(start("j3", "logs", "js/j3/"))),
        Command::JobStart(Box::new(bare)),
    ]);
    h.fake.pull_done(IMAGE);
    h.life.step();
    h.start(job("r4"));
    let sinks: Vec<(String, Option<Sink>)> = h
        .fake
        .created()
        .into_iter()
        .map(|spec| (spec.run_id, spec.log_sink))
        .collect();
    let of = |job_id: &str| {
        Sink::of_start(&start(job_id, "logs", &format!("js/{job_id}/"))).expect("a good sink")
    };
    assert_eq!(
        sinks,
        [
            ("j1".to_owned(), want),
            ("j3".to_owned(), of("j3")),
            ("j2".to_owned(), None),
            ("r4".to_owned(), None),
        ]
    );
    h.fake.exit("j2", 0);
    h.life.step();
    let listed = h.life.held_jobs();
    let j2 = listed
        .iter()
        .find(|job| job.job_id == "j2")
        .expect("listed");
    assert_eq!(j2.result.as_ref().expect("a result").logs, None);
}

/// Proves a sink that fails its check costs the job its live logs and nothing else: a prefix
/// that is not the job's, and a bucket the store client refuses, each start the job with no
/// sink in its run and one `log-sink-ignored` note naming the field, never its value; the
/// job runs to its normal result. Catches a bad sink that refuses the job, one that reaches
/// the runtime, a note that repeats or one that prints what was sent.
#[test]
fn a_bad_sink_starts_the_job_without_live_logs() {
    let mut h = Harness::new("live-bad-sink");
    h.apply(vec![
        Command::JobStart(Box::new(start("j1", "logs", "js/another-job/"))),
        Command::JobStart(Box::new(start("j2", "UPPER-CASE", "js/j2/"))),
    ]);
    let sinks: Vec<_> = h
        .fake
        .created()
        .into_iter()
        .map(|spec| spec.log_sink)
        .collect();
    assert_eq!(sinks, [None, None]);
    let notes: Vec<(Option<String>, String)> = h
        .life
        .findings()
        .into_iter()
        .filter(|finding| finding.code == code::LOG_SINK_IGNORED)
        .map(|finding| (finding.run_id, finding.message))
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(notes[0].0.as_deref(), Some("j1"));
    assert!(notes[0].1.contains("log_sink.prefix"), "{notes:?}");
    assert!(!notes[0].1.contains("another-job"), "{notes:?}");
    assert_eq!(notes[1].0.as_deref(), Some("j2"));
    assert!(notes[1].1.contains("bucket"), "{notes:?}");
    assert!(!notes[1].1.contains("UPPER-CASE"), "{notes:?}");
    for job_id in ["j1", "j2"] {
        h.fake.exit(job_id, 0);
    }
    h.life.step();
    for listed in h.life.held_jobs() {
        let result = listed.result.expect("a result");
        assert_eq!(result.outcome, Outcome::Succeeded);
        assert_eq!(result.logs, None);
    }
}
