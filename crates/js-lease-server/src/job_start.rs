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

use js_agent_wire::{BookedResources, JobStartSpec, RunFile};
use js_jobs::{AllocationRecord, Booked, JobRecord};
use js_types::{PortMap, Resources};

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

/// The network level a job's policy asks for, as the placer reads it.
pub(crate) use js_sched_placer::engine::jobs::job_network;

/// The start of the job `job` for its allocation `allocation`, to the host of `context`
/// (whose agent reports the ceiling `context` holds), and the one place a start is built: the
/// image, the command then its arguments, the environment, the files, what the
/// allocation booked, the wall limit, the network level and the declared ports, keyed by the
/// job id and the allocation id, and the job's log sink when `context` has sinks (the host
/// uploads live logs). The start is checked as the agent checks it
/// ([`JobStartSpec::validate`] and [`JobStartSpec::require_pinned`]) before it is returned; a
/// sink that fails the agent's check ([`JobStartSpec::log_sink_checked`]) is left out and the
/// start is sent without it, since a sink never refuses a start.
///
/// # Errors
/// [`Unsendable`] says why it cannot be sent.
pub(crate) fn start_of(
    job: &JobRecord,
    allocation: &AllocationRecord,
    context: &BeatContext<'_>,
) -> Result<JobStartSpec, Unsendable> {
    let ceiling = context.ceiling;
    let spec = &job.spec;
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
    let mut start = JobStartSpec {
        job_id: job.id.to_string(),
        allocation_id: allocation.allocation_id.to_string(),
        image: format!("{}@{}", spec.image.name(), spec.image.digest()),
        command: spec.command.iter().chain(&spec.args).cloned().collect(),
        run: None,
        os: None,
        arch: None,
        env: spec.env.clone(),
        files,
        booked,
        wall_ms,
        network,
        ports,
        log_sink,
        kind: Default::default(),
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
