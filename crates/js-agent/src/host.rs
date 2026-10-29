//! The hello a host sends: what was detected, lowered by the operator's flags.

use std::collections::BTreeMap;

use js_agent_wire::{Hello, InstanceIdentity, Phase, ProtocolRange, WireShape};

use js_caps::DETECTED_KEYS;

use crate::cli::Cli;
use crate::facts::{CommandRunner, FileReader, Platform, detect, host_id, machine_id};

/// The hello, and what the operator should be told about how it was made.
#[derive(Debug)]
pub struct Offer {
    /// The hello to send.
    pub hello: Hello,
    /// Warnings, such as a failed GPU probe.
    pub warnings: Vec<String>,
}

/// The value of a flag that only lowers: the detected amount when the flag is absent, the
/// flag's when it is not above it, otherwise an error naming the flag.
fn capped(flag: &str, wanted: Option<u64>, detected: u64) -> Result<u64, String> {
    match wanted {
        None => Ok(detected),
        Some(n) if n <= detected => Ok(n),
        Some(n) => Err(format!(
            "{flag}: {n} is above the {detected} detected; a flag only lowers what the host has"
        )),
    }
}

/// The GPU class to report: the operator's name for it, or the detected one.
fn class(
    given: Option<String>,
    count: u64,
    detected: Option<String>,
) -> Result<Option<String>, String> {
    match (count > 0, given) {
        (false, Some(_)) => Err("--gpu-class: the host offers no GPU to name".to_owned()),
        (false, None) => Ok(None),
        (true, given) => Ok(given.or(detected)),
    }
}

/// The capabilities to report: the operator's, and `os` and `arch` as detected. The keys
/// of the host contract are detected too and cannot be given.
fn capabilities(
    platform: &Platform,
    given: &[(String, String)],
) -> Result<BTreeMap<String, String>, String> {
    let clash = given
        .iter()
        .map(|(key, _)| key.as_str())
        .find(|key| matches!(*key, "os" | "arch") || DETECTED_KEYS.contains(key));
    match clash {
        Some(key) => Err(format!("--capability: {key} is detected and cannot be set")),
        None => Ok(given
            .iter()
            .cloned()
            .chain([
                ("os".to_owned(), platform.os.clone()),
                ("arch".to_owned(), platform.arch.clone()),
            ])
            .collect()),
    }
}

/// The id to register under.
fn id(
    cli: &Cli,
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<String, String> {
    match &cli.host_id {
        Some(id) => Ok(id.clone()),
        None => machine_id(platform, runner, files)
            .map(|machine| host_id(&machine))
            .map_err(|why| format!("cannot read the machine id ({why}); pass --host-id")),
    }
}

/// The instance the host runs on, from `--instance-file`; none without the flag.
fn instance(cli: &Cli, files: &dyn FileReader) -> Result<Option<InstanceIdentity>, String> {
    let Some(path) = &cli.instance_file else {
        return Ok(None);
    };
    let text = files
        .read(&path.to_string_lossy())
        .map_err(|why| format!("--instance-file: {why}"))?;
    let (provider, instance_id) = text.trim().split_once(':').ok_or_else(|| {
        format!(
            "--instance-file: {} must hold PROVIDER:INSTANCE_ID",
            path.display()
        )
    })?;
    Ok(Some(InstanceIdentity {
        provider: provider.to_owned(),
        instance_id: instance_id.to_owned(),
    }))
}

/// Detects the host and applies the flags.
///
/// # Errors
///
/// A fact cannot be detected, a flag raises what was detected, or the host id cannot be
/// made. The text names the flag when a flag is at fault.
pub fn offer(
    cli: &Cli,
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
    agent_version: &str,
) -> Result<Offer, String> {
    let detected = detect(platform, runner, files)?;
    let gpus = capped("--gpus", cli.gpus, detected.gpus.count)?;
    let cpu_millis = capped("--cpu-millis", cli.cpu_millis, detected.cpu_millis)?;
    let memory_bytes = capped("--memory-bytes", cli.memory_bytes, detected.memory_bytes)?;
    let gpu_class = class(cli.gpu_class.clone(), gpus, detected.gpus.class)?;
    let capabilities = capabilities(platform, &cli.capabilities)?;
    let host_id = id(cli, platform, runner, files)?;
    let instance = instance(cli, files)?;
    let devices = detected
        .gpus
        .devices
        .into_iter()
        .take(usize::try_from(gpus).unwrap_or(usize::MAX))
        .collect();
    Ok(Offer {
        hello: Hello {
            protocol: ProtocolRange::THIS,
            host_id,
            pool: cli.pool.clone(),
            agent_version: agent_version.to_owned(),
            instance,
            shape: WireShape {
                cpu_millis,
                memory_bytes,
                gpus,
                gpu_class,
                max_leases: None,
            },
            capabilities,
            gpu_devices: devices,
            labels: cli.labels.iter().cloned().collect(),
            phase: Phase::Serving,
            held: Vec::new(),
            // The credential has a home only under a state root.
            accepts_host_credential: cli.state_root.is_some(),
        },
        warnings: detected.warnings,
    })
}
