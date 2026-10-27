//! The arguments of `create`, built without a shell and without I/O.
//!
//! [`limits_to_args`] turns a job's limits and mounts into flags, [`hardening_args`] adds
//! what every contained job gets, and [`label_args`] the labels. Every value is one argv
//! entry, so nothing a job names is split, expanded or read as an option by anything but
//! the runtime's own flag parser; the image and the command line come after `--`.

use std::collections::BTreeMap;
use std::path::Path;

use super::{Engine, Settings};
use crate::lifecycle::ContainerSpec;

/// Where the run's scratch directory is mounted, read-write, inside the container.
pub const SCRATCH_TARGET: &str = "/scratch";

/// The most processes a job may hold when its limits name no bound: a fork bomb is
/// stopped well before it takes the host, and a build with many threads still fits.
pub const DEFAULT_PIDS: u64 = 4096;

/// A value that cannot be given to the runtime safely, or a limit it cannot honour.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    /// A mount path that is not absolute UTF-8 without a comma, a double quote or a
    /// control character: `--mount` is a comma-separated list, so any of those would
    /// let a path add options of its own.
    #[error("a mount path must be absolute UTF-8 without a comma, a quote or a control character")]
    MountPath,
    /// A label whose key is empty or holds `=`.
    #[error("a label key must not be empty or hold '='")]
    LabelKey,
}

/// `--cpus`, `--memory`, `--memory-swap`, `--pids-limit`, the GPU flags and `--mount`
/// for the scratch directory and every mount of `spec`.
///
/// `--memory-swap` is pinned equal to `--memory`, so the job cannot spill past its limit
/// into swap. `--pids-limit` is always set ([`DEFAULT_PIDS`] when the job names none).
/// GPUs are named by exact index, never `all`: Docker gets `--gpus "device=0,1"` (the
/// quotes are part of the value, because the value is parsed as CSV and holds a comma),
/// Podman one `--device nvidia.com/gpu=<index>` for each (CDI names, which need the
/// operator's generated CDI specification). A mount is `--mount type=bind,src=..,dst=..`
/// with `,readonly` where the job sees it read-only; `--mount` is used and not `-v`
/// because `-v` splits on `:`.
///
/// # Errors
///
/// A mount path that is not safe to put in the list ([`ArgsError::MountPath`]). The wire
/// refuses such paths already; this is the second check.
pub fn limits_to_args(engine: Engine, spec: &ContainerSpec) -> Result<Vec<String>, ArgsError> {
    let memory = spec.memory_bytes.to_string();
    let mut args = vec![
        "--cpus".to_owned(),
        format!("{}.{:03}", spec.cpu_millis / 1000, spec.cpu_millis % 1000),
        "--memory".to_owned(),
        memory.clone(),
        "--memory-swap".to_owned(),
        memory,
        "--pids-limit".to_owned(),
        spec.pids.unwrap_or(DEFAULT_PIDS).to_string(),
    ];
    args.extend(gpu_args(engine, &spec.gpu_indices));
    let scratch = (spec.scratch.as_path(), SCRATCH_TARGET, false);
    let jobs = spec
        .mounts
        .iter()
        .map(|mount| (mount.host.as_path(), mount.target.as_str(), mount.ro));
    for (host, target, ro) in std::iter::once(scratch).chain(jobs) {
        args.extend(["--mount".to_owned(), mount_value(host, target, ro)?]);
    }
    Ok(args)
}

fn gpu_args(engine: Engine, indices: &[u32]) -> Vec<String> {
    let each = |index: &u32| index.to_string();
    match (engine, indices.is_empty()) {
        (_, true) => Vec::new(),
        (Engine::Docker, false) => {
            let list: Vec<String> = indices.iter().map(each).collect();
            vec![
                "--gpus".to_owned(),
                format!("\"device={}\"", list.join(",")),
            ]
        }
        (Engine::Podman, false) => indices
            .iter()
            .flat_map(|index| ["--device".to_owned(), format!("nvidia.com/gpu={index}")])
            .collect(),
    }
}

/// A bind mount, as the value of `--mount`.
fn mount_value(host: &Path, target: &str, ro: bool) -> Result<String, ArgsError> {
    let plain = |text: &str| text.starts_with('/') && !text.contains([',', '"']);
    let clean = |text: &str| plain(text) && !text.chars().any(char::is_control);
    let host = host.to_str().filter(|text| clean(text));
    let target = Some(target).filter(|text| clean(text));
    host.zip(target)
        .map(|(host, target)| {
            let readonly = if ro { ",readonly" } else { "" };
            format!("type=bind,src={host},dst={target}{readonly}")
        })
        .ok_or(ArgsError::MountPath)
}

/// What every contained job gets, whatever it asked for: no capability, no privilege
/// gain, the network the agent was configured with (none by default), a read-only root
/// with a tmpfs `/tmp`, and a user that is not root. There is no `--rm`: the lifecycle
/// removes the container itself, after it has read the result.
pub fn hardening_args(settings: &Settings) -> Vec<String> {
    [
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--network",
        settings.network(),
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--user",
        settings.user(),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// One `--label key=value` for each label, in key order.
///
/// # Errors
///
/// A key that is empty or holds `=` ([`ArgsError::LabelKey`]).
pub fn label_args(labels: &BTreeMap<String, String>) -> Result<Vec<String>, ArgsError> {
    labels
        .iter()
        .map(|(key, value)| {
            let sound = !key.is_empty() && !key.contains('=');
            sound
                .then(|| ["--label".to_owned(), format!("{key}={value}")])
                .ok_or(ArgsError::LabelKey)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|pairs| pairs.into_iter().flatten().collect())
}
