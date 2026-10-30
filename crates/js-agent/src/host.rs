//! The hello a host sends: what was detected, lowered by the operator's flags.

use std::collections::BTreeMap;

use js_agent_wire::limits::MAX_DISK_BYTES;
use js_agent_wire::{Hello, InstanceIdentity, Phase, ProtocolRange, WireShape};

use js_caps::{DETECTED_KEYS, DISK_GIB};

use crate::bundle::{BundleHost, Settings, hello_capabilities, report, reserve_bytes};
use crate::cli::Cli;
use crate::facts::{
    CommandRunner, FileReader, GIB, Platform, detect, free_bytes, host_id, machine_id,
};

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

/// The bytes of disk to offer: `free` minus the low-water mark of `low_water_gib` GiB,
/// at least 0 and at most what the wire takes.
pub fn offered_disk_bytes(free: u64, low_water_gib: Option<u64>) -> u64 {
    let low_water = low_water_gib.unwrap_or(0).saturating_mul(GIB);
    free.saturating_sub(low_water).min(MAX_DISK_BYTES)
}

/// What the work volume offers: the volume that holds `--scratch-root`, where every run
/// gets its directory. The bytes are its free space minus the part of the bundle store's
/// budget the store has not used (`kept_back`, 0 without a store) minus
/// `--scratch-low-water-gib`; they
/// are the shape's `disk_bytes`, and the capability `disk-gib` says them in GiB, so there
/// are never two numbers. Without the flag there is no work volume to speak of and nothing
/// is offered or reported. A volume that cannot be read is a warning, and nothing is
/// offered or reported, so a job that needs disk is not placed on a guess. It is read when
/// the hello is made, not again.
fn disk(
    cli: &Cli,
    runner: &dyn CommandRunner,
    kept_back: u64,
) -> (u64, Option<(String, String)>, Option<String>) {
    let Some(root) = &cli.scratch_root else {
        return (0, None, None);
    };
    match free_bytes(runner, root) {
        Ok(free) => {
            let offered =
                offered_disk_bytes(free.saturating_sub(kept_back), cli.scratch_low_water_gib);
            let gib = (DISK_GIB.to_owned(), (offered / GIB).to_string());
            (offered, Some(gib), None)
        }
        Err(why) => (
            0,
            None,
            Some(format!(
                "--scratch-root: cannot read the free disk ({why}); disk-gib is not reported"
            )),
        ),
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

/// The bundle store a hello speaks for: its settings, and the host that reads it.
#[derive(Clone, Copy)]
pub struct StoreView<'a> {
    /// The checked flags.
    pub settings: &'a Settings,
    /// The machine, as the store sees it.
    pub host: &'a dyn BundleHost,
}

/// Detects the host and applies the flags. A host that has a bundle store (`store`) also
/// reports `bundle-root` and `bundle` (see [`crate::bundle`]), and the disk it offers (the
/// shape's `disk_bytes` and `disk-gib`) leaves out the unused part of the store's budget:
/// `floor((free - max(0, budget - used) - low_water) / GiB)`.
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
    store: Option<StoreView<'_>>,
) -> Result<Offer, String> {
    let detected = detect(platform, runner, files)?;
    let gpus = capped("--gpus", cli.gpus, detected.gpus.count)?;
    let cpu_millis = capped("--cpu-millis", cli.cpu_millis, detected.cpu_millis)?;
    let memory_bytes = capped("--memory-bytes", cli.memory_bytes, detected.memory_bytes)?;
    let gpu_class = class(cli.gpu_class.clone(), gpus, detected.gpus.class)?;
    let mut capabilities = capabilities(platform, &cli.capabilities)?;
    if cli.scratch_low_water_gib.is_some() && cli.scratch_root.is_none() {
        return Err("--scratch-low-water-gib: needs --scratch-root".to_owned());
    }
    if cli.scratch_quota && cli.scratch_root.is_none() {
        return Err("--scratch-quota: needs --scratch-root".to_owned());
    }
    let (kept_back, reserve_warning) = store
        .map(|view| reserve_bytes(view.settings, view.host))
        .unzip();
    let (disk_bytes, advisory, disk_warning) = disk(cli, runner, kept_back.unwrap_or_default());
    capabilities.extend(advisory);
    let reported = store.map(|view| report(view.settings, view.host));
    capabilities.extend(
        reported
            .iter()
            .flat_map(|found| hello_capabilities(&found.caps)),
    );
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
                disk_bytes,
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
        warnings: detected
            .warnings
            .into_iter()
            .chain(disk_warning)
            .chain(reserve_warning.flatten())
            .chain(reported.into_iter().flat_map(|found| found.warnings))
            .collect(),
    })
}
