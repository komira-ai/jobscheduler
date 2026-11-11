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
//! - The record keeps the ISA level and the accelerator a submit was given among its
//!   capabilities, under the keys a lease request uses (`validate.rs`): they are shown in
//!   their fields. Any other capability has no field and is not shown.
//! - The host silence of a job (`host_silent_since_ms`) is not in the record: it is left
//!   absent here, and the read calls set it from the registry (`read.rs`, "Host silence").
//! - The record's cancel flag says the client asked to cancel the job, and it stays set after
//!   the job ends. The message's `cancel_requested` means the cancel was asked for and the job
//!   has not yet ended, or the job ended lost after the ask (`job.proto`), so a job that ended
//!   any other way, cancelled included, shows it false ([`cancel_requested`]).
//! - The record keeps the last queue code of a job in every state. The message shows it while
//!   the job is queued, and on a job past the queue only when it is `placed`: a job that ended
//!   without being placed shows none, since its last wait no longer holds ([`queue_status`]).
//!   Its attempts and events are shown in every state.
//! - A job without an image (a root of layers alone) shows an empty image.
//! - The record keeps every mode, propagation and protocol of a job's mounts and host ports
//!   in its explicit form, and every host port's job port, as a submit recorded them: they are
//!   shown so, never unspecified and never absent. An absent `sub_path` is shown empty. The
//!   `layered-rootfs` capability a submit records beside them has no field and is not shown.
//! - The record keeps the agent's code of a start it refused: it is the job's
//!   `start_failure_detail`, empty for every other job.

use std::time::Duration;

use js_agent_wire::{HostPort, JobMount, Layer, MountMode, MountSource, PortProtocol, Propagation};
use js_jobs::{
    Isolation, JobEvent, JobRecord, JobResult, JobSpec, JobState, Needs, NetworkPolicy,
    ResourceUsage, ResultReason, StartFailure,
};
use js_proto::v1;
use js_state::StoreTime;
use js_types::{
    ACCEL_COUNT_KEY, ACCEL_FORM_FACTOR_KEY, ACCEL_INTERCONNECT_KEY, ACCEL_KEYS,
    ACCEL_MEMORY_GIB_KEY, ACCEL_MODEL_KEY, ACCEL_VENDOR_KEY, ExecMode, JobEventKind, QueueCode,
};

use crate::job_api::validate::ISA_LEVEL_KEY;

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
        JobState::FailedToStart(StartFailure::InputUnavailable) => {
            v1::StartFailureCode::InputUnavailable
        }
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
        ResultReason::InputFailed => v1::ResultReason::InputFailed,
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

/// The isolation of the job service that `isolation` is shown as: contained is unspecified,
/// the default, so the read-back of a job that never asked for host isolation is unchanged.
fn isolation(isolation: Isolation) -> v1::Isolation {
    match isolation {
        Isolation::Contained => v1::Isolation::Unspecified,
        Isolation::Host => v1::Isolation::Host,
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

/// The capability `key` of `spec`, when it holds one.
fn capability(spec: &JobSpec, key: &str) -> Option<String> {
    spec.constraints.capabilities.get(key).cloned()
}

/// The accelerator the `accelerator.*` capabilities of `spec` state; `None` when they state
/// none.
fn accelerator(spec: &JobSpec) -> Option<v1::Accelerator> {
    let named = ACCEL_KEYS.iter().any(|key| capability(spec, key).is_some());
    let number = |key| capability(spec, key).and_then(|text| text.parse().ok());
    named.then(|| v1::Accelerator {
        vendor: capability(spec, ACCEL_VENDOR_KEY),
        model: capability(spec, ACCEL_MODEL_KEY),
        count: number(ACCEL_COUNT_KEY),
        memory_per_device_gib: number(ACCEL_MEMORY_GIB_KEY),
        interconnect: capability(spec, ACCEL_INTERCONNECT_KEY),
        form_factor: capability(spec, ACCEL_FORM_FACTOR_KEY),
    })
}

/// A layer as the job service shows it.
fn layer(layer: &Layer) -> v1::Layer {
    let source = match layer {
        Layer::HostPath { path } => v1::layer::Source::HostPath(path.clone()),
        Layer::Image { image } => v1::layer::Source::Image(image.clone()),
    };
    v1::Layer {
        source: Some(source),
    }
}

/// A mount as the job service shows it: its mode and propagation explicit, an absent
/// `sub_path` empty.
fn mount(mount: &JobMount) -> v1::Mount {
    let MountSource::HostPath { path } = &mount.source;
    let mode = match mount.mode {
        MountMode::ReadOnly => v1::MountMode::ReadOnly,
        MountMode::ReadWrite => v1::MountMode::ReadWrite,
        MountMode::CopyOnWrite => v1::MountMode::CopyOnWrite,
    };
    let Propagation::Private = mount.propagation;
    v1::Mount {
        source: Some(v1::mount::Source::HostPath(path.clone())),
        target: mount.target.clone(),
        mode: mode.into(),
        sub_path: mount.sub_path.clone().unwrap_or_default(),
        no_exec: mount.no_exec,
        propagation: v1::MountPropagation::Private.into(),
    }
}

/// A host port as the job service shows it: its job port and its protocol explicit.
fn host_port(port: &HostPort) -> v1::HostPort {
    let PortProtocol::Tcp = port.protocol;
    v1::HostPort {
        host_port: port.host_port.into(),
        container_port: Some(port.job_port().into()),
        protocol: v1::PortProtocol::Tcp.into(),
    }
}

/// The job `spec` of the job `id` with `priority`, as the job service shows it.
fn spec(id: &str, spec: &JobSpec, priority: i32) -> v1::JobSpec {
    let mut command = spec.command.iter().cloned();
    let program = command.next().unwrap_or_default();
    let args = command.chain(spec.args.iter().cloned()).collect();
    v1::JobSpec {
        job_id: id.to_owned(),
        image: spec
            .image
            .as_ref()
            .map(|image| format!("{}@{}", image.name(), image.digest()))
            .unwrap_or_default(),
        command: program,
        args,
        env: spec.env.clone(),
        needs: Some(needs(&spec.needs)),
        constraints: Some(v1::Constraints {
            os: spec.constraints.os.clone().unwrap_or_default(),
            arch: spec.constraints.arch.clone().unwrap_or_default(),
            exec: exec(spec.constraints.exec).into(),
            isa_level: capability(spec, ISA_LEVEL_KEY).unwrap_or_default(),
            accelerator: accelerator(spec),
            isolation: isolation(spec.constraints.isolation).into(),
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
            .names()
            .into_iter()
            .map(|name| v1::JobFile {
                name: name.to_owned(),
                contents: String::new(),
            })
            .collect(),
        priority,
        labels: spec
            .labels
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        host_ports: spec.host_ports.iter().map(host_port).collect(),
        layers: spec.layers.iter().map(layer).collect(),
        mounts: spec.mounts.iter().map(mount).collect(),
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

/// Whether `job` shows `cancel_requested`, as `job.proto` defines it: its record's cancel flag
/// while it has not ended, and on a job that ended lost; false on a job that ended any other
/// way, whatever its record holds.
fn cancel_requested(job: &JobRecord) -> bool {
    let open = !job.state.is_ended() | (job.state == JobState::Lost);
    job.cancel_requested & open
}

/// `job` as the job service shows it.
pub(crate) fn job(job: &JobRecord) -> v1::Job {
    v1::Job {
        id: job.id.to_string(),
        spec: Some(spec(job.id.as_str(), &job.spec, job.priority)),
        state: state(job.state).into(),
        start_failure: start_failure(job.state).into(),
        start_failure_detail: job.start_failure_detail.clone().unwrap_or_default(),
        host_id: job
            .host
            .as_ref()
            .map(|host| host.as_str().to_owned())
            .unwrap_or_default(),
        times: Some(times(job)),
        host_silent_since_ms: None,
        cancel_requested: cancel_requested(job),
        client: job.client.as_ref().map(|client| client.as_str().to_owned()),
        queue_status: queue_status(job),
        attempts: job.queue.attempts(),
        events: job.queue.events().iter().map(event).collect(),
        dropped_events: job.queue.dropped_events(),
    }
}

/// The queue status `job` shows: its record's code, detail and the time the code last
/// changed; none while nothing was recorded, and none on a job past the queue that was never
/// placed, whose last code (a wait) no longer holds.
fn queue_status(job: &JobRecord) -> Option<v1::QueueStatus> {
    let trail = &job.queue;
    let holds = (job.state == JobState::Queued) | (trail.code() == QueueCode::Placed);
    let code = queue_code(trail.code()).filter(|_| holds)?;
    Some(v1::QueueStatus {
        code: code.into(),
        detail: trail.detail().to_owned(),
        since_ms: trail.since().map_or(0, millis),
    })
}

/// The job service's value of `code`; none for unspecified, which shows no status.
fn queue_code(code: QueueCode) -> Option<v1::QueueStatusCode> {
    use v1::QueueStatusCode as V;
    let value = match code {
        QueueCode::Unspecified => return None,
        QueueCode::WaitingForCapacity => V::WaitingForCapacity,
        QueueCode::WaitingForWholeHost => V::WaitingForWholeHost,
        QueueCode::QuotaBackoff => V::QuotaBackoff,
        QueueCode::FailingOver => V::FailingOver,
        QueueCode::Placed => V::Placed,
        QueueCode::WaitingForMachine => V::WaitingForMachine,
    };
    Some(value)
}

/// One event of a job's history as the job service shows it.
fn event(event: &JobEvent) -> v1::JobEvent {
    use v1::JobEventKind as V;
    let kind = match event.kind {
        JobEventKind::Unspecified => V::Unspecified,
        JobEventKind::WaitingForCapacity => V::WaitingForCapacity,
        JobEventKind::WaitingForWholeHost => V::WaitingForWholeHost,
        JobEventKind::QuotaBackoff => V::QuotaBackoff,
        JobEventKind::FailingOver => V::FailingOver,
        JobEventKind::Placed => V::Placed,
        JobEventKind::Requeued => V::Requeued,
        JobEventKind::WaitingForMachine => V::WaitingForMachine,
    };
    v1::JobEvent {
        at_ms: millis(event.at),
        kind: kind.into(),
        detail: event.detail.clone(),
        host_id: event.host.as_ref().map(|host| host.as_str().to_owned()),
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
/// reason, tails, times, usage and the code of an input that failed. A job that ended without a result (lost, or failed to
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
        input_failure: result.input_failure.clone(),
    }
}
