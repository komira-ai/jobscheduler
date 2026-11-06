//! What `GetJob` and `GetResult` show of a record (`read/view.rs`): every field of a job and
//! its spec, every state and start failure, and every way a job ends. Each answer is read
//! over both transports by the rig of `read_tests.rs`.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    Constraints, DeclaredPort, JobEnd, JobId, JobSpec, Needs, NetworkPolicy, NewJob, Output,
    StartFailure,
};
use js_proto::v1;
use js_types::{ExecMode, Resources};

use super::read_tests::{
    BIG, Rig, Step, clock, id, image, over_fake, place, queue, run, running, spec, start, times_of,
};
use crate::job_store::ServerJobs;

/// A spec with every field set: a command of two items, a GPU share with disk, constraints
/// with a capability the message has no field for, a port, a file, the allow-list network.
fn full() -> JobSpec {
    let resources = Resources::new(2000, BIG, 2, Some("nvidia-a100".to_owned()))
        .expect("resources")
        .with_disk_bytes(8 << 30);
    JobSpec {
        command: vec!["sh".to_owned(), "-c".to_owned()],
        args: vec!["echo hi".to_owned()],
        needs: Needs::Shape(resources),
        constraints: Constraints {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            exec: Some(ExecMode::Container),
            capabilities: BTreeMap::from([("kvm".to_owned(), "yes".to_owned())]),
        },
        network: NetworkPolicy::AllowList,
        ports: vec![DeclaredPort {
            host: 8080,
            container: 80,
        }],
        files: js_jobs::JobFiles::Contents(BTreeMap::from([(
            "upload-url".to_owned(),
            b"secret".to_vec(),
        )])),
        ..spec(&[("team", "ml")])
    }
}

/// `full` as `GetJob` shows it for the job `full-1` with priority 5.
fn full_shown() -> v1::JobSpec {
    v1::JobSpec {
        job_id: "full-1".to_owned(),
        image: image(),
        command: "sh".to_owned(),
        args: vec!["-c".to_owned(), "echo hi".to_owned()],
        env: BTreeMap::from([("MODE".to_owned(), "test".to_owned())]),
        needs: Some(v1::Needs {
            cpu_millis: 2000,
            memory_bytes: BIG,
            disk_bytes: Some(8 << 30),
            gpus: 2,
            gpu_class: "nvidia-a100".to_owned(),
            whole_host: false,
        }),
        constraints: Some(v1::Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: v1::ExecMode::Container.into(),
            isa_level: String::new(),
            accelerator: None,
        }),
        wall_limit_seconds: Some(600),
        network: v1::NetworkLevel::AllowList.into(),
        ports: vec![v1::PortForward {
            host_port: 8080,
            container_port: 80,
        }],
        files: vec![v1::JobFile {
            name: "upload-url".to_owned(),
            contents: String::new(),
        }],
        priority: 5,
        labels: BTreeMap::from([("team".to_owned(), "ml".to_owned())]),
    }
}

/// Asserts that `GetJob` shows every field of a job's record and spec: the full spec as
/// [`full_shown`] (the command's first item as the program and the rest before the
/// arguments, a file named with empty contents, the priority); a whole-host job with no
/// command and the sandbox-exec mode on the open network; a job with no disk, GPU class,
/// exec mode or network as absent or unspecified; a placed job's host; and a running job
/// whose cancel was asked for. Catches a field dropped or mapped to the wrong value, and a
/// file's contents shown.
#[tokio::test]
async fn get_job_shows_every_field_of_the_record() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let jobs = &rig.jobs;
    let new = NewJob {
        id: Some(id("full-1")),
        spec: full(),
        priority: 5,
        client: None,
    };
    jobs.insert_queued(new).expect("recorded");
    let shown = v1::Job {
        id: "full-1".to_owned(),
        spec: Some(full_shown()),
        state: v1::JobState::Queued.into(),
        start_failure: v1::StartFailureCode::Unspecified.into(),
        start_failure_detail: String::new(),
        host_id: String::new(),
        times: Some(times_of(jobs, "full-1")),
        host_silent_since_ms: None,
        cancel_requested: false,
        client: None,
    };
    assert_eq!(rig.get_job("full-1").await.expect("read"), shown);

    let whole = JobSpec {
        command: Vec::new(),
        args: Vec::new(),
        needs: Needs::WholeHost,
        constraints: Constraints {
            exec: Some(ExecMode::SandboxExec),
            ..Constraints::default()
        },
        network: NetworkPolicy::Open,
        ..spec(&[])
    };
    queue(jobs, "whole-1", whole);
    let shown = rig
        .get_job("whole-1")
        .await
        .expect("read")
        .spec
        .expect("a spec");
    let expected = v1::JobSpec {
        job_id: "whole-1".to_owned(),
        image: image(),
        env: BTreeMap::from([("MODE".to_owned(), "test".to_owned())]),
        needs: Some(v1::Needs {
            whole_host: true,
            ..v1::Needs::default()
        }),
        constraints: Some(v1::Constraints {
            exec: v1::ExecMode::SandboxExec.into(),
            ..v1::Constraints::default()
        }),
        wall_limit_seconds: Some(600),
        network: v1::NetworkLevel::Open.into(),
        ..v1::JobSpec::default()
    };
    assert_eq!(shown, expected);

    let plain = queue(jobs, "plain-1", spec(&[]));
    place(jobs, &plain);
    let job = rig.get_job("plain-1").await.expect("read");
    assert_eq!(job.host_id, "host-a");
    assert_eq!(job.state, i32::from(v1::JobState::Placed));
    let plain = job.spec.expect("a spec");
    assert_eq!(
        (plain.command.as_str(), plain.args, plain.network),
        (
            "run",
            vec!["--fast".to_owned()],
            i32::from(v1::NetworkLevel::None)
        )
    );
    let needs = plain.needs.expect("needs");
    assert_eq!(
        (needs.disk_bytes, needs.gpus, needs.gpu_class.as_str()),
        (None, 0, "")
    );
    let constraints = plain.constraints.expect("constraints");
    assert_eq!(constraints, v1::Constraints::default());

    let asked = running(jobs, "asked-1");
    jobs.request_cancel(&asked).expect("the cancel is recorded");
    let job = rig.get_job("asked-1").await.expect("read");
    assert_eq!(
        (job.state, job.cancel_requested),
        (i32::from(v1::JobState::Running), true)
    );
}

/// How the jobs of [`every_way_a_job_ends_is_shown`] are taken to their end.
#[derive(Clone, Copy)]
enum Before {
    /// Queued only.
    Queued,
    /// Placed.
    Placed,
    /// Starting.
    Starting,
    /// Running.
    Running,
}

/// The job `text`, taken to `before`.
fn taken(jobs: &ServerJobs, text: &str, before: Before) -> JobId {
    let job = queue(jobs, text, spec(&[]));
    let steps: [Step; 3] = [place, start, run];
    let count = match before {
        Before::Queued => 0,
        Before::Placed => 1,
        Before::Starting => 2,
        Before::Running => 3,
    };
    for step in &steps[..count] {
        step(jobs, &job);
    }
    job
}

/// What the host reported, with nothing written and no usage.
fn quiet() -> Output {
    Output::default()
}

/// Asserts, for every way a job ends, what `GetJob` and `GetResult` show: the state and the
/// start failure of the job, and the result's state, exit code, reason and usage. An exit of
/// 0 succeeds, a wall limit and a memory stop fail with their reasons, a cancel of a running
/// job is cancelled with its exit code; a lost job and each of the four start failures show
/// their state with no exit code, reason, tail or usage, and the record's times. Catches a
/// state, a start failure or a reason mapped to another, and a result invented for a job
/// that ended without one.
#[tokio::test]
async fn every_way_a_job_ends_is_shown() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let jobs = &rig.jobs;
    let cases = [
        (
            "ok-1",
            Before::Running,
            JobEnd::Exited {
                exit_code: 0,
                output: quiet(),
            },
            v1::JobState::Succeeded,
            v1::StartFailureCode::Unspecified,
            Some((Some(0), v1::ResultReason::Exit)),
        ),
        (
            "wall-1",
            Before::Running,
            JobEnd::WallLimit {
                exit_code: None,
                output: quiet(),
            },
            v1::JobState::Failed,
            v1::StartFailureCode::Unspecified,
            Some((None, v1::ResultReason::WallLimit)),
        ),
        (
            "memory-1",
            Before::Running,
            JobEnd::Memory {
                exit_code: Some(137),
                output: quiet(),
            },
            v1::JobState::Failed,
            v1::StartFailureCode::Unspecified,
            Some((Some(137), v1::ResultReason::Memory)),
        ),
        (
            "cancel-1",
            Before::Running,
            JobEnd::Cancelled {
                exit_code: Some(143),
                output: quiet(),
            },
            v1::JobState::Cancelled,
            v1::StartFailureCode::Unspecified,
            Some((Some(143), v1::ResultReason::Cancelled)),
        ),
        (
            "lost-1",
            Before::Running,
            JobEnd::Lost,
            v1::JobState::Lost,
            v1::StartFailureCode::Unspecified,
            None,
        ),
        (
            "capacity-1",
            Before::Queued,
            JobEnd::FailedToStart(StartFailure::NoCapacity),
            v1::JobState::FailedToStart,
            v1::StartFailureCode::NoCapacity,
            None,
        ),
        (
            "timeout-1",
            Before::Placed,
            JobEnd::FailedToStart(StartFailure::StartTimeout),
            v1::JobState::FailedToStart,
            v1::StartFailureCode::StartTimeout,
            None,
        ),
        (
            "exclusive-1",
            Before::Placed,
            JobEnd::FailedToStart(StartFailure::ExclusiveNotReady),
            v1::JobState::FailedToStart,
            v1::StartFailureCode::ExclusiveNotReady,
            None,
        ),
        (
            "refused-1",
            Before::Starting,
            JobEnd::FailedToStart(StartFailure::AgentRefused),
            v1::JobState::FailedToStart,
            v1::StartFailureCode::AgentRefused,
            None,
        ),
    ];
    for (text, before, end, state, failure, reported) in cases {
        let job = taken(jobs, text, before);
        rig.advance(Duration::from_secs(1));
        jobs.end_job(&job, end).expect("the job ends");
        let shown = rig.get_job(text).await.expect("read");
        assert_eq!(
            (shown.state, shown.start_failure),
            (i32::from(state), i32::from(failure)),
            "{text}"
        );
        let result = rig.get_result(text).await.expect("the result");
        let times = shown.times;
        let expected = match reported {
            Some((exit_code, reason)) => v1::JobResult {
                state: state.into(),
                exit_code,
                reason: reason.into(),
                truncated: false,
                times: result.times,
                usage: None,
                ..v1::JobResult::default()
            },
            None => v1::JobResult {
                state: state.into(),
                times,
                ..v1::JobResult::default()
            },
        };
        assert_eq!(result, expected, "{text}");
        assert_eq!(result.times, shown.times, "{text}");
    }
}
