//! Fully populated messages the wire and stub tests share: every field holds a value that
//! is not its default, so a field that is lost on the way shows as a difference. The
//! goldens `golden/submit_job_request.hex` and `golden/job.hex` are the encodings of
//! `spec("presigned-url-text")` in a submit and of [`job`].

use std::collections::BTreeMap;

use js_proto::v1::{
    Constraints, ExecMode, Job, JobFile, JobSpec, JobState, JobTimes, Needs, NetworkLevel,
    PortForward, StartFailureCode,
};

/// A time in milliseconds since the Unix epoch that the samples count from.
pub const T0: u64 = 1_800_000_000_000;

/// A map of the pairs, inserted in the order given. A test that gives the pairs in reverse
/// key order shows that the encoding follows the keys, not the insertion.
pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (key, value) in pairs {
        map.insert((*key).to_owned(), (*value).to_owned());
    }
    map
}

/// A job spec with every field set, its one file holding `contents`.
pub fn spec(contents: &str) -> JobSpec {
    JobSpec {
        job_id: "job-1".to_owned(),
        image: format!(
            "registry.example/app@sha256:{}",
            "0123456789abcdef".repeat(4)
        ),
        command: "/bin/run".to_owned(),
        args: vec!["--fast".to_owned(), "input".to_owned()],
        env: map(&[("B_VAR", "2"), ("A_VAR", "1")]),
        needs: Some(Needs {
            cpu_millis: 1500,
            memory_bytes: 2 << 30,
            disk_bytes: Some(1 << 30),
            gpus: 1,
            gpu_class: "nvidia-a100-sxm4-80gb".to_owned(),
            whole_host: true,
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container.into(),
        }),
        wall_limit_seconds: Some(3600),
        network: NetworkLevel::AllowList.into(),
        ports: vec![PortForward {
            host_port: 8080,
            container_port: 80,
        }],
        files: vec![JobFile {
            name: "upload-url".to_owned(),
            contents: contents.to_owned(),
        }],
        priority: 7,
        labels: map(&[("team", "build"), ("kind", "test")]),
    }
}

/// A job with every field set, as a read returns it: its file's contents are empty.
pub fn job() -> Job {
    Job {
        id: "job-1".to_owned(),
        spec: Some(spec("")),
        state: JobState::FailedToStart.into(),
        start_failure: StartFailureCode::AgentRefused.into(),
        start_failure_detail: "image-fetch-failed".to_owned(),
        host_id: "host-a".to_owned(),
        times: Some(JobTimes {
            queued_at_ms: T0,
            placed_at_ms: Some(T0 + 1000),
            started_at_ms: Some(T0 + 2000),
            ended_at_ms: Some(T0 + 3000),
        }),
        host_silent_since_ms: Some(T0 + 4000),
        cancel_requested: true,
    }
}
