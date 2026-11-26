//! The checks of a submitted job, for `SubmitJob` (`submit.rs`): a job of the job service
//! (`v1::JobSpec`) becomes the job the tables record ([`js_jobs::JobSpec`]), its id and
//! priority, and what admission asks of a host ([`Requirements`]), or the first field that
//! is not valid. Every function here is pure: it reads the request and the ceiling, and
//! nothing else, and the address rule a callback's literal address is judged by.
//!
//! The fields are checked in the order the message declares them, and the first fault is
//! the answer:
//!
//! | field | rule | refused as |
//! |---|---|---|
//! | `spec` | present | `field: spec` |
//! | `spec.jobId` | empty (the store mints an id), or 1 to 63 bytes of letters, digits, `.`, `_` and `-` | `field: spec.jobId` |
//! | `spec.image` | `NAME[:TAG]@sha256:<64 hex>`: a tag alone is refused; empty only beside `spec.layers` | `field: spec.image` |
//! | `spec.command` | with an empty image, an absolute path in normal form ([`js_types::path::absolute`]) | `field: spec.command` |
//! | `spec.env` | as a start's environment is (`js_agent_wire::check_env`): at most `js_agent_wire::limits::MAX_ENV` entries; each name non-empty, of `[A-Za-z_][A-Za-z0-9_]*`, at most `MAX_ENV_KEY_LEN` bytes and not one the agent owns or that steers the loader, the agent or a shell ([`js_types::env::env_name_reserved`]: `HOME`, `TMPDIR`, `LD_*`, `JS_*`, `IFS` and the rest); each value without NUL; the values and names within the wire's caps | `field: spec.env: invalid-env` |
//! | `spec.needs` | `whole_host` with no other need; a GPU count with a class (from `gpu_class`, else `constraints.accelerator.model`) and a class with a count | `field: spec.needs`, `spec.needs.gpus`, `spec.needs.gpuClass` |
//! | `spec.constraints` | `os` empty, `linux` or `macos`; `arch` any name, `aarch64` stored as `arm64`; `exec` a known mode, and not `sandbox-exec` for a job with layers or a host port at a job port of its own; `isaLevel` empty or a level; `osVersion` empty or a whole major version of one to four digits without a leading zero; `xcode` empty or a build id ([`js_agent_wire::is_xcode_build`]: 1 to 32 bytes of letters, digits, `.`, `_` and `-`, the first a letter or digit); `accelerator` keys valid and not contradicting the booked class; `isolation` a known value (unspecified is contained), and not `host` for a job with layers, mounts or host ports | `field: spec.constraints.<name>` |
//! | `spec.wallLimitSeconds` | present and above 0, and at most the ceiling | the refusals `wall-time-not-positive` and `wall-time-above-ceiling` |
//! | `spec.network` | a known level | `field: spec.network` |
//! | `spec.ports` | each port 1 to 65535, no host port twice | `field: spec.ports[i].hostPort`, `spec.ports[i].containerPort` |
//! | `spec.hostPorts` | the rules of `rootfs` | `field: spec.hostPorts[i].<name>` |
//! | `spec.files` | as a start's run files are (`js_agent_wire::validate_files`) | `field: spec.files` |
//! | `spec.labels` | as a lease's labels are (`Labels::from_caller`) | `field: spec.labels` |
//! | `spec.historyKey` | empty, or 1 to 512 bytes of printable ASCII (`0x20` to `0x7e`), kept as sent ([`HistoryKey`]) | `field: spec.historyKey: bad-length`, `field: spec.historyKey: not-printable-ascii` |
//! | `spec.layers`, `spec.mounts` | the rules of `rootfs` | `field: spec.layers[i].<name>`, `spec.mounts[i].<name>` |
//! | `spec.retry` | absent: this release does not retry a job, so it refuses one that asks, whatever the policy says | `field: spec.retry: retry-not-enabled` |
//! | `spec.callback` | absent while the server takes no callback (until the release that accepts one), whatever it says; else the rules of `callback_check` | `field: spec.callback: callback-not-enabled`; `field: spec.callback.url: <code>`, `spec.callback.signingKey: bad-length` |
//!
//! The cross-field rules (an empty image, its command, `exec`, `needs.wholeHost` and
//! `isolation` beside the layers, mounts and host ports) are checked once those fields are, and
//! before `isolation`. A whole-host job may name none of them (`field: spec.needs`): the
//! whole-host pass does not hold a job to the hosts that report `layered-rootfs`.
//!
//! A job with layers, mounts or host ports records `layered-rootfs=true` among its
//! capabilities and asks admission for it, so it is placed and started only on a host whose
//! agent reports it; one with layers, or a host port at a job port of its own, records
//! `exec=container` when it left `exec` unspecified.
//!
//! A job that names a provider for a layer or a mount (`docs/design/mount-providers.md`) also
//! records `mount-provider.<name>=true` for each distinct provider name
//! ([`js_caps::mount_provider_key`]) and lists the names in [`Requirements::mount_providers`], so
//! admission refuses at submit a job no host or catalog could serve, and it is placed and
//! started only on a host whose agent reports each key.
//!
//! A field is named by its JSON name, as the gateway names it. A refusal never repeats a
//! value of the request: not the id, not the image, not a file.
//!
//! The capability vocabulary is the one a lease request speaks (design section 3.8): the ISA
//! level, the least operating-system version, the Xcode build and the structured accelerator
//! are recorded among the job's capabilities under the keys a lease request uses (`isa_level`,
//! `os_version`, `xcode` and the `accelerator.*` keys), where placement reads them, and the old `gpus` and `gpu_class` pair is the booked count and class, which the
//! alias table resolves; an accelerator model this release does not know passes through.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{Layer, RunFile, check_env, is_xcode_build, validate_files};
use js_callback::AddressRule;
use js_caps::{FLAG_SET, LAYERED_ROOTFS, XCODE, mount_provider_key};
use js_jobs::{
    Constraints, DeclaredPort, Isolation, JobFiles, JobId, JobSpec, Needs, NetworkPolicy,
};
use js_proto::v1;
use js_types::{
    ACCEL_COUNT_KEY, ACCEL_FORM_FACTOR_KEY, ACCEL_INTERCONNECT_KEY, ACCEL_MEMORY_GIB_KEY,
    ACCEL_MODEL_KEY, ACCEL_VENDOR_KEY, AcceleratorNeed, ExecMode, HistoryKey, ImageDigestRef,
    IsaLevel, Labels, OS_VERSION_KEY, Os, Requirements, Resources, normalise_arch,
};
// The network level a job asks a host for, by a name of its own: the type's own name followed
// by a path separator reads as the server's network module to the source scan of design
// section 8.6, which only the files that launch or check through a mode may name.
use js_types::JobNetwork as Reach;

use super::{callback_check, rootfs};

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
    /// The field, by its JSON name, breaks a rule whose code the answer carries: one of the
    /// wire's check of a start (`js_agent_wire::refusal`), or of this module's own (the
    /// history key's, `retry-not-enabled`, and the callback's, `callback_check`).
    Wire(&'static str, &'static str),
    /// The field, by its JSON name, of the port at this index of `spec.ports`.
    Port(usize, &'static str),
    /// The field, by its JSON name (a path below the entry for a provider source's field:
    /// `provider.provider`, `provider.ref`), of the entry at this index of the list
    /// `spec.<list>` (`layers`, `mounts`, `hostPorts`).
    Entry(&'static str, usize, &'static str),
    /// The entry at this index of the list `spec.<list>` has no source this server knows: none
    /// was given, or one a later release added.
    NoSource(&'static str, usize),
    /// The wall limit is absent or 0.
    WallNotPositive,
    /// The wall limit is above the ceiling.
    WallAboveCeiling,
}

/// The job `spec` checked against `ceiling`, the most a wall limit may be, with its callback,
/// if it asks for one, judged by `callbacks`: refused while it is `None`, which is the server
/// that takes no callback.
///
/// # Errors
/// The [`Fault`] of the first field, in the message's order, that is not valid.
pub(crate) fn job(
    spec: Option<&v1::JobSpec>,
    ceiling: Duration,
    callbacks: Option<&AddressRule>,
) -> Result<Valid, Fault> {
    let spec = spec.ok_or(Fault::Field("spec"))?;
    let id = id(&spec.job_id)?;
    let image = Some(spec.image.as_str())
        .filter(|image| !image.is_empty())
        .map(ImageDigestRef::parse)
        .transpose()
        .map_err(|_| Fault::Field("spec.image"))?;
    env(&spec.env)?;
    let constraints = spec.constraints.clone().unwrap_or_default();
    let resources = resources(spec.needs.as_ref(), &constraints)?;
    let (mut kept, mut capabilities) = host(&constraints)?;
    let wall_limit_secs = wall(spec.wall_limit_seconds, ceiling)?;
    let network = network(spec.network)?;
    let ports = ports(&spec.ports)?;
    let host_ports = rootfs::host_ports(&spec.host_ports, &ports)?;
    let files = files(&spec.files)?;
    let labels = labels(&spec.labels)?;
    let history_key = history_key(&spec.history_key)?;
    let layers = rootfs::layers(&spec.layers)?;
    let mounts = rootfs::mounts(&spec.mounts)?;
    not_enabled(spec)?;
    let callback = callback_check::callback(spec.callback.as_ref(), callbacks)?;
    let accelerator = AcceleratorNeed::requested(&capabilities, resources.shape())
        .map_err(|_| Fault::Field("spec.constraints.accelerator"))?;
    imageless(image.as_ref(), &spec.command, &layers)?;
    let layered = !(layers.is_empty() & mounts.is_empty() & host_ports.is_empty());
    if rootfs::needs_container(&layers, &host_ports) {
        kept.exec = match kept.exec {
            None | Some(ExecMode::Container) => Some(ExecMode::Container),
            Some(ExecMode::SandboxExec) => return Err(Fault::Field("spec.constraints.exec")),
        };
    }
    if layered {
        if let Asked::WholeHost(_) = resources {
            return Err(Fault::Field("spec.needs"));
        }
        capabilities.insert(LAYERED_ROOTFS.to_owned(), FLAG_SET.to_owned());
    }
    let mount_providers = rootfs::providers(&layers, &mounts);
    for name in &mount_providers {
        capabilities.insert(mount_provider_key(name), FLAG_SET.to_owned());
    }
    let isolation = isolation(constraints.isolation)?;
    if layered & (isolation == Isolation::Host) {
        return Err(Fault::Field("spec.constraints.isolation"));
    }
    let wanted = Requirements {
        resources: resources.shape().clone(),
        disk_bytes: Some(resources.shape().disk_bytes()).filter(|bytes| *bytes > 0),
        os: kept.os.clone(),
        arch: kept.arch.clone(),
        isa_level: kept.isa_level,
        os_version: kept.os_version,
        exec: kept.exec,
        accelerator,
        network: job_network(network),
        host_ports: ports
            .iter()
            .map(|port| port.host)
            .collect::<BTreeSet<u16>>()
            .into_iter()
            .collect(),
        layered_rootfs: layered,
        mount_providers,
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
            isolation,
        },
        wall_limit_secs,
        network,
        ports,
        files,
        labels,
        layers,
        mounts,
        host_ports,
        history_key,
        callback,
    };
    Ok(Valid {
        id,
        spec: job,
        priority: spec.priority,
        wanted,
    })
}

/// The command of a job without an image: an absolute path, since no image configuration
/// names a program; and such a job has layers, which are then its whole root.
fn imageless(image: Option<&ImageDigestRef>, command: &str, layers: &[Layer]) -> Result<(), Fault> {
    if image.is_some() {
        return Ok(());
    }
    if layers.is_empty() {
        return Err(Fault::Field("spec.image"));
    }
    js_types::path::absolute(command).map_err(|_| Fault::Field("spec.command"))
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

/// How the job is confined: unspecified reads as contained, an unknown value is refused.
fn isolation(value: i32) -> Result<Isolation, Fault> {
    match v1::Isolation::try_from(value) {
        Ok(v1::Isolation::Unspecified | v1::Isolation::Contained) => Ok(Isolation::Contained),
        Ok(v1::Isolation::Host) => Ok(Isolation::Host),
        Err(_) => Err(Fault::Field("spec.constraints.isolation")),
    }
}

/// The constraints admission reads, each `None` when the job leaves it empty.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    os: Option<String>,
    arch: Option<String>,
    exec: Option<ExecMode>,
    isa_level: Option<IsaLevel>,
    os_version: Option<u32>,
}

/// The constraints of a job, and the capabilities the tables record beside them: the ISA
/// level, the least operating-system version, the Xcode build and the accelerator's keys, which [`AcceleratorNeed::requested`] checks with the
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
    let os_version = given(&constraints.os_version)
        .map(|text| major_version(&text).ok_or(Fault::Field("spec.constraints.osVersion")))
        .transpose()?;
    let xcode = given(&constraints.xcode)
        .map(|build| {
            is_xcode_build(&build)
                .then_some(build)
                .ok_or(Fault::Field("spec.constraints.xcode"))
        })
        .transpose()?;
    let mut capabilities = BTreeMap::new();
    if let Some(version) = os_version {
        capabilities.insert(OS_VERSION_KEY.to_owned(), version.to_string());
    }
    if let Some(build) = xcode {
        capabilities.insert(XCODE.to_owned(), build);
    }
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
        os_version,
    };
    Ok((kept, capabilities))
}

/// The major version `text` states: one to four ASCII digits with no leading zero, and at
/// least 1.
fn major_version(text: &str) -> Option<u32> {
    let digits = (1..=4).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    let plain = !text.starts_with('0');
    text.parse::<u32>().ok().filter(|_| digits && plain)
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
pub(crate) fn job_network(policy: NetworkPolicy) -> Reach {
    match policy {
        NetworkPolicy::None => Reach::None,
        NetworkPolicy::AllowList => Reach::AllowList,
        NetworkPolicy::Open => Reach::Open,
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
fn files(files: &[v1::JobFile]) -> Result<JobFiles, Fault> {
    let run: Vec<RunFile> = files
        .iter()
        .map(|file| RunFile {
            name: file.name.clone(),
            contents: file.contents.clone(),
        })
        .collect();
    validate_files(&run).map_err(|_| Fault::Field("spec.files"))?;
    Ok(JobFiles::Contents(
        run.into_iter()
            .map(|file| (file.name, file.contents.into_bytes()))
            .collect(),
    ))
}

/// The environment, checked as a start's is, so that no job is admitted whose start the wire
/// would refuse and which would then never be sent.
fn env(env: &BTreeMap<String, String>) -> Result<(), Fault> {
    check_env("spec.env", env).map_err(|error| Fault::Wire("spec.env", error.code()))
}

/// The labels, checked as a lease's are.
fn labels(labels: &BTreeMap<String, String>) -> Result<Labels, Fault> {
    Labels::from_caller(labels).map_err(|_| Fault::Field("spec.labels"))
}

/// The history key: none when empty, else kept as sent when it is a [`HistoryKey`] (1 to
/// 512 bytes of printable ASCII), refused with the word of its fault otherwise.
fn history_key(text: &str) -> Result<Option<HistoryKey>, Fault> {
    if text.is_empty() {
        return Ok(None);
    }
    HistoryKey::parse(text)
        .map(Some)
        .map_err(|fault| Fault::Wire("spec.historyKey", fault.code()))
}

/// A job that carries a retry policy is refused, whatever it holds (an empty policy
/// included), before its callback is looked at: this release would record the job without
/// it, so it would run once while its client believed otherwise
/// (`docs/design/job-retry-and-completion-callback.md`, section 7).
fn not_enabled(spec: &v1::JobSpec) -> Result<(), Fault> {
    match spec.retry {
        Some(_) => Err(Fault::Wire("spec.retry", "retry-not-enabled")),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
