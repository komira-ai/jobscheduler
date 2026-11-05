//! The job tables' records as the job service's messages: a [`JobRecord`] as a `Job`, an
//! ended job's [`JobResult`] as a `JobResult`. Every function here is total: a record always
//! has a message, and nothing is read but the record.
//!
//! Where the record and the message differ in shape:
//!
//! - The record's command is a list and the message's a single program: the program is the
//!   list's first item, and the rest of the list goes before the arguments, so a command of
//!   one item (what a submit records) reads back as it was given.
//! - A file's contents are secret: every file is named, with empty contents.
//! - The record keeps capabilities among its constraints that the message has no field for;
//!   they are not shown.
//! - The host silence of a job (`host_silent_since_ms`) is not read from the record and is
//!   left absent.

use std::time::Duration;

use js_jobs::{
    JobRecord, JobResult, JobSpec, JobState, Needs, NetworkPolicy, ResourceUsage, ResultReason,
    StartFailure,
};
use js_proto::v1;
use js_state::StoreTime;
use js_types::ExecMode;

/// `time` in milliseconds since the store clock's origin (the Unix epoch for the server's
/// stores), the largest value when it does not fit.
pub(crate) fn millis(time: StoreTime) -> u64 {
    duration_millis(time.as_duration())
}

/// `duration` in whole milliseconds, the largest value when it does not fit.
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The state of the job service that `state` is shown as.
pub(crate) fn state(state: JobState) -> v1::JobState {
    match state {
        JobState::Queued => v1::JobState::Queued,
        JobState::Placed => v1::JobState::Placed,
        JobState::Starting => v1::JobState::Starting,
        JobState::Running => v1::JobState::Running,
        JobState::Succeeded => v1::JobState::Succeeded,
        JobState::Failed => v1::JobState::Failed,
        JobState::Cancelled => v1::JobState::Cancelled,
        JobState::Lost => v1::JobState::Lost,
        JobState::FailedToStart(_) => v1::JobState::FailedToStart,
    }
}

/// Why a job in `state` failed to start; unspecified for every other state.
fn start_failure(state: JobState) -> v1::StartFailureCode {
    match state {
        JobState::FailedToStart(StartFailure::NoCapacity) => v1::StartFailureCode::NoCapacity,
        JobState::FailedToStart(StartFailure::StartTimeout) => v1::StartFailureCode::StartTimeout,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady) => {
            v1::StartFailureCode::ExclusiveNotReady
        }
        JobState::FailedToStart(StartFailure::AgentRefused) => v1::StartFailureCode::AgentRefused,
        _ => v1::StartFailureCode::Unspecified,
    }
}

/// The reason of the job service that `reason` is shown as.
fn reason(reason: ResultReason) -> v1::ResultReason {
    match reason {
        ResultReason::Exit => v1::ResultReason::Exit,
        ResultReason::WallLimit => v1::ResultReason::WallLimit,
        ResultReason::Memory => v1::ResultReason::Memory,
        ResultReason::Cancelled => v1::ResultReason::Cancelled,
    }
}

/// The network level of the job service that `policy` is shown as.
fn network(policy: NetworkPolicy) -> v1::NetworkLevel {
    match policy {
        NetworkPolicy::None => v1::NetworkLevel::None,
        NetworkPolicy::AllowList => v1::NetworkLevel::AllowList,
        NetworkPolicy::Open => v1::NetworkLevel::Open,
    }
}

/// The exec mode of the job service that `exec` is shown as: unspecified for either.
fn exec(exec: Option<ExecMode>) -> v1::ExecMode {
    match exec {
        None => v1::ExecMode::Unspecified,
        Some(ExecMode::Container) => v1::ExecMode::Container,
        Some(ExecMode::SandboxExec) => v1::ExecMode::SandboxExec,
    }
}

/// What a job needs, as the job service shows it: a whole-host job has `whole_host` set and
/// no other need; a disk of 0 is absent.
fn needs(needs: &Needs) -> v1::Needs {
    match needs {
        Needs::Shape(resources) => v1::Needs {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            disk_bytes: Some(resources.disk_bytes()).filter(|bytes| *bytes > 0),
            gpus: u32::try_from(resources.gpus()).unwrap_or(u32::MAX),
            gpu_class: resources.gpu_class().unwrap_or_default().to_owned(),
            whole_host: false,
        },
        Needs::WholeHost => v1::Needs {
            whole_host: true,
            ..v1::Needs::default()
        },
    }
}

/// The job `spec` of the job `id` with `priority`, as the job service shows it.
fn spec(id: &str, spec: &JobSpec, priority: i32) -> v1::JobSpec {
    let mut command = spec.command.iter().cloned();
    let program = command.next().unwrap_or_default();
    let args = command.chain(spec.args.iter().cloned()).collect();
    v1::JobSpec {
        job_id: id.to_owned(),
        image: format!("{}@{}", spec.image.name(), spec.image.digest()),
        command: program,
        args,
        env: spec.env.clone(),
        needs: Some(needs(&spec.needs)),
        constraints: Some(v1::Constraints {
            os: spec.constraints.os.clone().unwrap_or_default(),
            arch: spec.constraints.arch.clone().unwrap_or_default(),
            exec: exec(spec.constraints.exec).into(),
            // A stored job holds no ISA level or accelerator of its own yet: no submit
            // writes them, so the view shows neither.
            isa_level: String::new(),
            accelerator: None,
        }),
        wall_limit_seconds: Some(spec.wall_limit_secs),
        network: network(spec.network).into(),
        ports: spec
            .ports
            .iter()
            .map(|port| v1::PortForward {
                host_port: port.host.into(),
                container_port: port.container.into(),
            })
            .collect(),
        files: spec
            .files
            .keys()
            .map(|name| v1::JobFile {
                name: name.clone(),
                contents: String::new(),
            })
            .collect(),
        priority,
        labels: spec
            .labels
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    }
}

/// The times of `job` as its record holds them.
fn times(job: &JobRecord) -> v1::JobTimes {
    v1::JobTimes {
        queued_at_ms: millis(job.submitted_at),
        placed_at_ms: job.placed_at.map(millis),
        started_at_ms: job.started_at.map(millis),
        ended_at_ms: job.ended_at.map(millis),
    }
}

/// `job` as the job service shows it.
pub(crate) fn job(job: &JobRecord) -> v1::Job {
    v1::Job {
        id: job.id.to_string(),
        spec: Some(spec(job.id.as_str(), &job.spec, job.priority)),
        state: state(job.state).into(),
        start_failure: start_failure(job.state).into(),
        start_failure_detail: String::new(),
        host_id: job
            .host
            .as_ref()
            .map(|host| host.as_str().to_owned())
            .unwrap_or_default(),
        times: Some(times(job)),
        host_silent_since_ms: None,
        cancel_requested: job.cancel_requested,
    }
}

/// What a job used, as the job service shows it. The record keeps no disk or GPU count.
fn usage(usage: ResourceUsage) -> v1::ResourceUsage {
    v1::ResourceUsage {
        cpu_usec: u64::try_from(usage.cpu_time.as_micros()).unwrap_or(u64::MAX),
        peak_memory_bytes: usage.peak_memory_bytes,
        disk_bytes: None,
        gpus: 0,
    }
}

/// The result of `job`, which has ended, from the result it stored: its state, exit code,
/// reason, tails, times and usage. A job that ended without a result (lost, or failed to
/// start) shows its state and its record's times, and no exit code, reason, tail or usage.
pub(crate) fn result(job: &JobRecord) -> v1::JobResult {
    let state = state(job.state).into();
    match &job.result {
        Some(result) => stored(state, result),
        None => v1::JobResult {
            state,
            times: Some(times(job)),
            ..v1::JobResult::default()
        },
    }
}

/// The stored `result` of a job that ended in `state`.
fn stored(state: i32, result: &JobResult) -> v1::JobResult {
    let timing = &result.timing;
    v1::JobResult {
        state,
        exit_code: result.exit_code,
        reason: reason(result.reason).into(),
        stdout_tail: result.stdout_tail.as_bytes().to_vec(),
        stderr_tail: result.stderr_tail.as_bytes().to_vec(),
        truncated: result.truncated,
        times: Some(v1::JobTimes {
            queued_at_ms: millis(timing.queued_at),
            placed_at_ms: timing.placed_at.map(millis),
            started_at_ms: timing.started_at.map(millis),
            ended_at_ms: Some(millis(timing.ended_at)),
        }),
        usage: result.usage.map(usage),
    }
}
