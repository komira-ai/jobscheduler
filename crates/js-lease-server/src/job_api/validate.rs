//! The checks of a submitted job, for `SubmitJob` (`submit.rs`): a job of the job service
//! (`v1::JobSpec`) becomes the job the tables record ([`js_jobs::JobSpec`]), its id and
//! priority, and what admission asks of a host ([`Requirements`]), or the first field that
//! is not valid. Every function here is pure: it reads the request and the ceiling, and
//! nothing else.
//!
//! The fields are checked in the order the message declares them, and the first fault is
//! the answer:
//!
//! | field | rule | refused as |
//! |---|---|---|
//! | `spec` | present | `field: spec` |
//! | `spec.jobId` | empty (the store mints an id), or 1 to 63 bytes of letters, digits, `.`, `_` and `-` | `field: spec.jobId` |
//! | `spec.image` | `NAME[:TAG]@sha256:<64 hex>`: a tag alone is refused | `field: spec.image` |
//! | `spec.env` | names and values as given; nothing to refuse | |
//! | `spec.needs` | `whole_host` with no other need; a GPU count with a class (from `gpu_class`, else `constraints.accelerator.model`) and a class with a count | `field: spec.needs`, `spec.needs.gpus`, `spec.needs.gpuClass` |
//! | `spec.constraints` | `os` empty, `linux` or `macos`; `arch` any name, `aarch64` stored as `arm64`; `exec` a known mode; `isaLevel` empty or a level; `accelerator` keys valid and not contradicting the booked class | `field: spec.constraints.<name>` |
//! | `spec.wallLimitSeconds` | present and above 0, and at most the ceiling | the refusals `wall-time-not-positive` and `wall-time-above-ceiling` |
//! | `spec.network` | a known level | `field: spec.network` |
//! | `spec.ports` | each port 1 to 65535, no host port twice | `field: spec.ports[i].hostPort`, `spec.ports[i].containerPort` |
//! | `spec.files` | as a start's run files are (`js_agent_wire::validate_files`) | `field: spec.files` |
//! | `spec.labels` | as a lease's labels are (`Labels::from_caller`) | `field: spec.labels` |
//!
//! A field is named by its JSON name, as the gateway names it. A refusal never repeats a
//! value of the request: not the id, not the image, not a file.
//!
//! The capability vocabulary is the one a lease request speaks (design section 3.8): the ISA
//! level and the structured accelerator are recorded among the job's capabilities under the
//! keys a lease request uses (`isa_level` and the `accelerator.*` keys), where placement reads
//! them, and the old `gpus` and `gpu_class` pair is the booked count and class, which the
//! alias table resolves; an accelerator model this release does not know passes through.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{RunFile, validate_files};
use js_jobs::{Constraints, DeclaredPort, JobId, JobSpec, Needs, NetworkPolicy};
use js_proto::v1;
use js_types::{
    ACCEL_COUNT_KEY, ACCEL_FORM_FACTOR_KEY, ACCEL_INTERCONNECT_KEY, ACCEL_MEMORY_GIB_KEY,
    ACCEL_MODEL_KEY, ACCEL_VENDOR_KEY, AcceleratorNeed, ExecMode, ImageDigestRef, IsaLevel,
    JobNetwork, Labels, Os, Requirements, Resources, normalise_arch,
};

/// The capability key a job's least ISA level is recorded under: the key a lease request
/// names it by.
pub(crate) const ISA_LEVEL_KEY: &str = "isa_level";

/// A submitted job that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Valid {
    /// The client's id; `None` for the store to mint one.
    pub(crate) id: Option<JobId>,
    /// The job as the tables record it.
    pub(crate) spec: JobSpec,
    /// Its priority, which is not part of the job.
    pub(crate) priority: i32,
    /// What admission asks of a host.
    pub(crate) wanted: Requirements,
}

/// Why a submitted job is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The field, by its JSON name, is not valid.
    Field(&'static str),
    /// The field, by its JSON name, of the port at this index of `spec.ports`.
    Port(usize, &'static str),
    /// The wall limit is absent or 0.
    WallNotPositive,
    /// The wall limit is above the ceiling.
    WallAboveCeiling,
}

/// The job `spec` checked against `ceiling`, the most a wall limit may be.
///
/// # Errors
/// The [`Fault`] of the first field, in the message's order, that is not valid.
pub(crate) fn job(spec: Option<&v1::JobSpec>, ceiling: Duration) -> Result<Valid, Fault> {
    let spec = spec.ok_or(Fault::Field("spec"))?;
    let id = id(&spec.job_id)?;
    let image = ImageDigestRef::parse(&spec.image).map_err(|_| Fault::Field("spec.image"))?;
    let constraints = spec.constraints.clone().unwrap_or_default();
    let resources = resources(spec.needs.as_ref(), &constraints)?;
    let (kept, capabilities) = host(&constraints)?;
    let wall_limit_secs = wall(spec.wall_limit_seconds, ceiling)?;
    let network = network(spec.network)?;
    let ports = ports(&spec.ports)?;
    let files = files(&spec.files)?;
    let labels = labels(&spec.labels)?;
    let accelerator = AcceleratorNeed::requested(&capabilities, resources.shape())
        .map_err(|_| Fault::Field("spec.constraints.accelerator"))?;
    let wanted = Requirements {
        resources: resources.shape().clone(),
        disk_bytes: Some(resources.shape().disk_bytes()).filter(|bytes| *bytes > 0),
        os: kept.os.clone(),
        arch: kept.arch.clone(),
        isa_level: kept.isa_level,
        exec: kept.exec,
        accelerator,
        network: job_network(network),
        host_ports: ports
            .iter()
            .map(|port| port.host)
            .collect::<BTreeSet<u16>>()
            .into_iter()
            .collect(),
    };
    let command = Some(spec.command.clone()).filter(|command| !command.is_empty());
    let job = JobSpec {
        image,
        command: command.into_iter().collect(),
        args: spec.args.clone(),
        env: spec.env.clone(),
        needs: resources.needs(),
        constraints: Constraints {
            os: kept.os,
            arch: kept.arch,
            exec: kept.exec,
            capabilities,
        },
        wall_limit_secs,
        network,
        ports,
        files,
        labels,
    };
    Ok(Valid {
        id,
        spec: job,
        priority: spec.priority,
        wanted,
    })
}

/// The client's id: none when empty, else a job id.
fn id(text: &str) -> Result<Option<JobId>, Fault> {
    match text.is_empty() {
        true => Ok(None),
        false => JobId::parse(text)
            .map(Some)
            .map_err(|_| Fault::Field("spec.jobId")),
    }
}

/// What the job needs: a share of a host, or the whole host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    /// A share: these resources.
    Shape(Resources),
    /// The whole host: admission asks for a host of any size.
    WholeHost(Resources),
}

impl Asked {
    /// The resources admission asks a host to hold.
    fn shape(&self) -> &Resources {
        match self {
            Asked::Shape(resources) | Asked::WholeHost(resources) => resources,
        }
    }

    /// The needs the tables record.
    fn needs(&self) -> Needs {
        match self {
            Asked::Shape(resources) => Needs::Shape(resources.clone()),
            Asked::WholeHost(_) => Needs::WholeHost,
        }
    }
}

/// The resources of `needs` (absent: none), the GPU class taken from `gpu_class`, else from
/// the accelerator model of `constraints`.
fn resources(needs: Option<&v1::Needs>, constraints: &v1::Constraints) -> Result<Asked, Fault> {
    let needs = needs.cloned().unwrap_or_default();
    if needs.whole_host {
        let alone = v1::Needs {
            whole_host: true,
            ..v1::Needs::default()
        };
        return match needs == alone {
            true => Ok(Asked::WholeHost(Resources::default())),
            false => Err(Fault::Field("spec.needs")),
        };
    }
    // The accelerator's model is the class only of a job that counts GPUs: a class without
    // a count is refused by name, a model without one asks for no GPU.
    let model = constraints
        .accelerator
        .as_ref()
        .and_then(|accelerator| accelerator.model.clone())
        .filter(|_| needs.gpus > 0);
    let class = Some(needs.gpu_class.clone())
        .filter(|class| !class.is_empty())
        .or(model);
    let resources = match Resources::new(
        needs.cpu_millis,
        needs.memory_bytes,
        needs.gpus.into(),
        class,
    ) {
        Ok(resources) => resources,
        // A class with no GPU, or GPUs of no class.
        Err(_) => {
            let field = match needs.gpus {
                0 => "spec.needs.gpus",
                _ => "spec.needs.gpuClass",
            };
            return Err(Fault::Field(field));
        }
    };
    Ok(Asked::Shape(
        resources.with_disk_bytes(needs.disk_bytes.unwrap_or(0)),
    ))
}

/// The constraints admission reads, each `None` when the job leaves it empty.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    os: Option<String>,
    arch: Option<String>,
    exec: Option<ExecMode>,
    isa_level: Option<IsaLevel>,
}

/// The constraints of a job, and the capabilities the tables record beside them: the ISA
/// level and the accelerator's keys, which [`AcceleratorNeed::requested`] checks with the
/// booked GPUs.
fn host(constraints: &v1::Constraints) -> Result<(Kept, BTreeMap<String, String>), Fault> {
    let given = |text: &str| Some(text.to_owned()).filter(|text| !text.is_empty());
    let os = given(&constraints.os)
        .map(|os| os.parse::<Os>().map(|_| os))
        .transpose()
        .map_err(|_| Fault::Field("spec.constraints.os"))?;
    let arch = given(&constraints.arch).map(|arch| normalise_arch(&arch).to_owned());
    let exec = match v1::ExecMode::try_from(constraints.exec) {
        Ok(v1::ExecMode::Unspecified) => None,
        Ok(v1::ExecMode::Container) => Some(ExecMode::Container),
        Ok(v1::ExecMode::SandboxExec) => Some(ExecMode::SandboxExec),
        Err(_) => return Err(Fault::Field("spec.constraints.exec")),
    };
    let isa_level = given(&constraints.isa_level)
        .map(|level| level.parse::<IsaLevel>())
        .transpose()
        .map_err(|_| Fault::Field("spec.constraints.isaLevel"))?;
    let mut capabilities = BTreeMap::new();
    if let Some(level) = given(&constraints.isa_level) {
        capabilities.insert(ISA_LEVEL_KEY.to_owned(), level);
    }
    if let Some(accelerator) = &constraints.accelerator {
        capabilities.extend(accelerator_keys(accelerator));
    }
    let kept = Kept {
        os,
        arch,
        exec,
        isa_level,
    };
    Ok((kept, capabilities))
}

/// The `accelerator.*` capability entries of `accelerator`, one per field it gives.
pub(crate) fn accelerator_keys(accelerator: &v1::Accelerator) -> Vec<(String, String)> {
    let number = |value: Option<u32>| value.map(|value| value.to_string());
    [
        (ACCEL_VENDOR_KEY, accelerator.vendor.clone()),
        (ACCEL_MODEL_KEY, accelerator.model.clone()),
        (ACCEL_COUNT_KEY, number(accelerator.count)),
        (
            ACCEL_MEMORY_GIB_KEY,
            number(accelerator.memory_per_device_gib),
        ),
        (ACCEL_INTERCONNECT_KEY, accelerator.interconnect.clone()),
        (ACCEL_FORM_FACTOR_KEY, accelerator.form_factor.clone()),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
    .collect()
}

/// The wall limit in seconds: present, above 0, and at most `ceiling`.
fn wall(seconds: Option<u64>, ceiling: Duration) -> Result<u64, Fault> {
    match seconds {
        None | Some(0) => Err(Fault::WallNotPositive),
        Some(seconds) if seconds > ceiling.as_secs() => Err(Fault::WallAboveCeiling),
        Some(seconds) => Ok(seconds),
    }
}

/// The network policy of the level `level`; unspecified is none.
fn network(level: i32) -> Result<NetworkPolicy, Fault> {
    match v1::NetworkLevel::try_from(level) {
        Ok(v1::NetworkLevel::Unspecified | v1::NetworkLevel::None) => Ok(NetworkPolicy::None),
        Ok(v1::NetworkLevel::AllowList) => Ok(NetworkPolicy::AllowList),
        Ok(v1::NetworkLevel::Open) => Ok(NetworkPolicy::Open),
        Err(_) => Err(Fault::Field("spec.network")),
    }
}

/// The network level admission asks a host's ceiling to cover.
pub(crate) fn job_network(policy: NetworkPolicy) -> JobNetwork {
    match policy {
        NetworkPolicy::None => JobNetwork::None,
        NetworkPolicy::AllowList => JobNetwork::AllowList,
        NetworkPolicy::Open => JobNetwork::Open,
    }
}

/// A port of 1 to 65535.
fn port(port: u32) -> Option<u16> {
    u16::try_from(port).ok().filter(|port| *port > 0)
}

/// The declared ports: each port 1 to 65535, and no host port twice.
fn ports(ports: &[v1::PortForward]) -> Result<Vec<DeclaredPort>, Fault> {
    let mut seen = BTreeSet::new();
    ports
        .iter()
        .enumerate()
        .map(|(at, forward)| {
            let host = port(forward.host_port)
                .filter(|host| seen.insert(*host))
                .ok_or(Fault::Port(at, "hostPort"))?;
            let container = port(forward.container_port).ok_or(Fault::Port(at, "containerPort"))?;
            Ok(DeclaredPort { host, container })
        })
        .collect()
}

/// The files, checked as a start's run files are, by name.
fn files(files: &[v1::JobFile]) -> Result<BTreeMap<String, Vec<u8>>, Fault> {
    let run: Vec<RunFile> = files
        .iter()
        .map(|file| RunFile {
            name: file.name.clone(),
            contents: file.contents.clone(),
        })
        .collect();
    validate_files(&run).map_err(|_| Fault::Field("spec.files"))?;
    Ok(run
        .into_iter()
        .map(|file| (file.name, file.contents.into_bytes()))
        .collect())
}

/// The labels, checked as a lease's are.
fn labels(labels: &BTreeMap<String, String>) -> Result<Labels, Fault> {
    Labels::from_caller(labels).map_err(|_| Fault::Field("spec.labels"))
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
