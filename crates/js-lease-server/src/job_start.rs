//! The job start a host is sent for one placement: the job as its record holds it, with what
//! its allocation booked and its id ([`start_of`]), and the job's log sink when the host
//! uploads live logs.
//!
//! A start the wire cannot carry is not sent ([`Unsendable`]): the job then waits as placed
//! (or, for a whole-host job, as starting), and the start timeout ends it. Nothing here
//! guesses a value the records do not hold: a GPU device index, a network level the host's
//! agent does not report it can give, a job id or a file the wire's grammar refuses.
//!
//! An allocation that books the whole host books what the registry books for the host: its
//! CPU, memory and disk ([`BeatContext::shape`]). The wall limit is the allocation's own when
//! it records one (a job backfilled on a marked host is cut to the time left in the tail),
//! else the job's.

use js_agent_wire::{
    BUNDLE_PROGRAM_PREFIX, BookedResources, Isolation, JobStartSpec, NetworkLevel, RunFile, RunSpec,
};
use js_caps::{XCODE, is_mount_provider_key};
use js_jobs::{AllocationRecord, Booked, JobRecord, JobSpec, NetworkPolicy};
use js_types::{Arch, ExecMode, Os, PortMap, Resources};

use crate::job_api::read::view::millis;
use crate::per_job::BeatContext;

/// Why a placement's start cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Unsendable {
    /// The allocation books the whole host and the registry's shape of the host is not known
    /// to the beat, so what the start books is not known.
    #[error("the allocation books the whole host and the host's shape is not known")]
    NoHostShape,
    /// The allocation books GPUs by count (or the whole host, which has them), and a start
    /// names each device by its index, which no record holds yet.
    #[error("the allocation books {0} GPUs and no record names their device indices")]
    GpuIndices(u64),
    /// The job asks for more network, or a port, than the host's agent reports it gives; an
    /// agent from before the network field would run it with less.
    #[error("the job's network is above what the host reports it gives")]
    AboveCeiling,
    /// A file's contents are not UTF-8, which a start carries as text.
    #[error("the file {0} is not UTF-8 text")]
    FileNotText(String),
    /// The job's files hold their digests, not their contents: the job has ended, and its
    /// contents were dropped with the end.
    #[error("the job's files hold digests, not contents")]
    FilesDigested,
    /// The job runs on the machine itself and the host's hello did not report `host-jobs`; an
    /// agent that does not report it is never sent a host job.
    #[error("the job runs on the machine itself and the host does not report host-jobs")]
    HostJobsNotReported,
    /// The job names layers, mounts or host ports and the host's hello did not report
    /// `layered-rootfs`; an agent that does not report it would run the job without them, so
    /// it is never sent such a job.
    #[error(
        "the job names layers, mounts or host ports and the host does not report layered-rootfs"
    )]
    RootfsNotReported,
    /// The job requires the capability of a node-local mount provider (`mount-provider.<name>`,
    /// from a layer or a mount that names the provider) and the host's hello did not report it;
    /// an agent without the provider cannot make the tree, and one from before provider
    /// sources would not read the source, so it is never sent such a job. It names the key.
    #[error("the job requires {0} and the host does not report it")]
    MountProviderNotReported(String),
    /// The job names an Xcode build (`xcode`, [`js_caps::XCODE`]) and the host's hello did not
    /// list it among its builds; an agent that does not hold the build would refuse the start
    /// (`xcode-not-held`), and one from before the field would run it with the wrong Xcode, so
    /// it is never sent such a job. It names the build.
    #[error("the job requires Xcode {0} and the host does not report it")]
    XcodeNotReported(String),
    /// The wall limit in milliseconds overflows.
    #[error("the wall limit overflows")]
    WallLimit,
    /// The start breaks a rule of the wire (a job id the wire's id grammar refuses, a file
    /// name, a bound): the code and the field it names.
    #[error("the start breaks the wire's rule {code} on {field}")]
    Invalid {
        /// The refusal code.
        code: &'static str,
        /// The field at fault.
        field: &'static str,
    },
}

/// The wire's isolation of a job: `host` for a job that runs on the machine itself.
fn isolation_of(job: &JobRecord) -> Isolation {
    match job.spec.constraints.isolation {
        js_jobs::Isolation::Contained => Isolation::Contained,
        js_jobs::Isolation::Host => Isolation::Host,
    }
}

/// The network level a job's policy asks for, as the placer reads it.
pub(crate) use js_sched_placer::engine::jobs::job_network;

/// The start of the job `job` for its allocation `allocation`, to the host of `context`
/// (whose agent reports the ceiling `context` holds), and the one place a start is built: the
/// image (empty for a job of layers alone), the command then its arguments, the environment,
/// the files, what the allocation booked, the wall limit, the network level, the declared
/// ports, the isolation (left out of the wire for a contained job), and the layers, mounts and
/// host ports as the job's record holds them (each left out of the wire when empty, so a job
/// without them starts as before), keyed by the job id and the allocation id, and
/// the job's log sink when `context` has sinks (the host uploads live logs). The start is
/// checked as the agent checks it ([`JobStartSpec::validate`] and
/// [`JobStartSpec::require_pinned`]) before it is returned; a sink that fails the agent's check
/// ([`JobStartSpec::log_sink_checked`]) is left out and the start is sent without it, since a
/// sink never refuses a start.
///
/// A job that runs on the machine itself is sent only to a host whose agent reported
/// `host-jobs` ([`BeatContext::host_jobs`]), and one with layers, mounts or host ports only to
/// a host whose agent reported `layered-rootfs` ([`BeatContext::layered_rootfs`]); a job that
/// requires a `mount-provider.<name>` capability is sent only to a host whose agent reported
/// each such key ([`BeatContext::mount_providers`]); a job that names an Xcode build (the
/// capability `xcode`) is sent only to a host whose agent listed that build
/// ([`BeatContext::xcodes`]), and its start carries it as `xcode`.
///
/// # Errors
/// [`Unsendable`] says why it cannot be sent.
///
/// # Native jobs
/// A job whose `exec` constraint is `sandbox-exec` and that has an image runs a bundle, not a
/// container: its start has `command` empty (the wire requires it beside a `run`) and a
/// `run` with no files, no environment of its own (the job's environment stays in `env`, which
/// the agent lays over the bundle's), no inputs and no outputs, the network level mapped from
/// the job's ([`run_network`]) and the program of [`bundle_program`]. The start's `os` and
/// `arch` are the job's constraints when they name a known system and architecture. Every
/// other job (`exec` unspecified or `container`, or no image) is built as a container job, with
/// the command then its arguments in `command` and `run`, `os` and `arch` left out.
pub(crate) fn start_of(
    job: &JobRecord,
    allocation: &AllocationRecord,
    context: &BeatContext<'_>,
) -> Result<JobStartSpec, Unsendable> {
    let ceiling = context.ceiling;
    let spec = &job.spec;
    let isolation = isolation_of(job);
    if !isolation.is_contained() & !context.host_jobs {
        return Err(Unsendable::HostJobsNotReported);
    }
    if spec.uses_layered_rootfs() & !context.layered_rootfs {
        return Err(Unsendable::RootfsNotReported);
    }
    let unreported = spec
        .constraints
        .capabilities
        .keys()
        .filter(|key| is_mount_provider_key(key))
        .find(|key| !context.mount_providers.contains(key));
    if let Some(key) = unreported {
        return Err(Unsendable::MountProviderNotReported(key.clone()));
    }
    let xcode = spec.constraints.capabilities.get(XCODE).cloned();
    let listed = |build: &String| context.xcodes.contains(build);
    if !xcode.as_ref().is_none_or(listed) {
        return Err(Unsendable::XcodeNotReported(xcode.unwrap_or_default()));
    }
    let booked = match allocation.booked {
        Booked::WholeHost => whole_host(context.shape)?,
        Booked::Shape {
            accelerators: gpus @ 1..,
            ..
        } => return Err(Unsendable::GpuIndices(gpus)),
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes,
            accelerators: 0,
        } => BookedResources {
            cpu_millis,
            memory_bytes,
            disk_bytes: (disk_bytes > 0).then_some(disk_bytes),
            gpu_indices: Vec::new(),
        },
    };
    let network = job_network(spec.network);
    let ports: Vec<PortMap> = spec
        .ports
        .iter()
        .map(|port| PortMap {
            host: port.host,
            container: port.container,
        })
        .collect();
    let covered = ceiling.allows(network) & ports.iter().all(|port| ceiling.forwards(port.host));
    if !covered {
        return Err(Unsendable::AboveCeiling);
    }
    let files = spec
        .files
        .contents()
        .ok_or(Unsendable::FilesDigested)?
        .iter()
        .map(|(name, contents)| {
            String::from_utf8(contents.clone())
                .map(|contents| RunFile {
                    name: name.clone(),
                    contents,
                })
                .map_err(|_| Unsendable::FileNotText(name.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let wall_ms = allocation
        .wall_limit_secs
        .unwrap_or(spec.wall_limit_secs)
        .checked_mul(1000)
        .ok_or(Unsendable::WallLimit)?;
    let log_sink = context
        .logs
        .and_then(|logs| logs.sink_for(context.host.as_str(), job.id.as_str()));
    let native = spec.constraints.exec == Some(ExecMode::SandboxExec) && spec.image.is_some();
    let (command, run, os, arch) = if native {
        (
            Vec::new(),
            spec.image.as_ref().map(|image| run_of(spec, image.name())),
            spec.constraints
                .os
                .as_deref()
                .and_then(|os| os.parse::<Os>().ok()),
            spec.constraints
                .arch
                .as_deref()
                .and_then(|arch| arch.parse::<Arch>().ok()),
        )
    } else {
        (
            spec.command.iter().chain(&spec.args).cloned().collect(),
            None,
            None,
            None,
        )
    };
    let mut start = JobStartSpec {
        job_id: job.id.to_string(),
        allocation_id: allocation.allocation_id.to_string(),
        image: spec
            .image
            .as_ref()
            .map(|image| format!("{}@{}", image.name(), image.digest()))
            .unwrap_or_default(),
        command,
        run,
        os,
        arch,
        env: spec.env.clone(),
        files,
        booked,
        wall_ms,
        network,
        ports,
        log_sink,
        kind: Default::default(),
        isolation,
        layers: spec.layers.clone(),
        mounts: spec.mounts.clone(),
        host_ports: spec.host_ports.clone(),
        placed_at_ms: job.placed_at.map(millis),
        fuse: false,
        xcode,
    };
    let sink_ok = start.log_sink_checked().is_ok();
    start.log_sink = start.log_sink.filter(|_| sink_ok);
    start
        .validate()
        .and_then(|()| start.require_pinned())
        .map_err(|error| Unsendable::Invalid {
            code: error.code(),
            field: error.field(),
        })?;
    Ok(start)
}

/// The network level of a native job's [`RunSpec`]: none is off, an allow-list is on (limited
/// to the pool's packet filter), open is open.
fn run_network(policy: NetworkPolicy) -> NetworkLevel {
    match policy {
        NetworkPolicy::None => NetworkLevel::Off,
        NetworkPolicy::AllowList => NetworkLevel::On,
        NetworkPolicy::Open => NetworkLevel::Open,
    }
}

/// The run of a native job of the image named `image_name`: the program of [`bundle_program`], then the rest of the job's
/// command, then its arguments; nothing else (the job's environment stays in the start's `env`).
fn run_of(spec: &JobSpec, image_name: &str) -> RunSpec {
    let (program, rest) = bundle_program(spec, image_name);
    RunSpec {
        program,
        args: rest.iter().chain(&spec.args).cloned().collect(),
        env: Default::default(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: run_network(spec.network),
    }
}

/// The program a native job runs, and the arguments of its command after the program.
///
/// With a command, its first entry is the program: kept as it is when it starts with `@bundle/`
/// or is an absolute path, else taken as the name of an entrypoint of the bundle (`@bundle/`
/// then the entry). With no command, the program is the bundle's default entrypoint, the one
/// named as the bundle (the last `/`-separated part of the image's name, without its tag), as
/// an OCI image's entrypoint runs when no command is given. A program the wire's grammar
/// refuses is left for [`JobStartSpec::validate`] to refuse.
fn bundle_program<'a>(spec: &'a JobSpec, image_name: &str) -> (String, &'a [String]) {
    match spec.command.split_first() {
        Some((first, rest)) => {
            let kept = first.starts_with(BUNDLE_PROGRAM_PREFIX) || first.starts_with('/');
            let program = if kept {
                first.clone()
            } else {
                format!("{BUNDLE_PROGRAM_PREFIX}{first}")
            };
            (program, rest)
        }
        None => {
            let base = image_name
                .rsplit_once('/')
                .map_or(image_name, |(_, last)| last);
            let bundle = base.split_once(':').map_or(base, |(bundle, _)| bundle);
            (format!("{BUNDLE_PROGRAM_PREFIX}{bundle}"), &[])
        }
    }
}

/// What an allocation of the whole host books: the CPU, memory and disk of `shape`, the
/// registry's shape of the host. A host with GPUs cannot be started on: a start names each
/// device by its index.
fn whole_host(shape: Option<&Resources>) -> Result<BookedResources, Unsendable> {
    let shape = shape.ok_or(Unsendable::NoHostShape)?;
    match shape.gpus() {
        0 => Ok(BookedResources {
            cpu_millis: shape.cpu_millis(),
            memory_bytes: shape.memory_bytes(),
            disk_bytes: (shape.disk_bytes() > 0).then_some(shape.disk_bytes()),
            gpu_indices: Vec::new(),
        }),
        gpus => Err(Unsendable::GpuIndices(gpus)),
    }
}

#[cfg(test)]
#[path = "job_start_tests.rs"]
mod tests;
