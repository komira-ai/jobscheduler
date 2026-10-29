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

/// Where the run's files are mounted, read-only, inside the container.
///
/// The container's root is read-only, and an engine puts a tmpfs on `/run` for such a
/// container (Podman does; an explicit `--tmpfs /run` may be added). The bind sits on top
/// of that tmpfs: engines order mounts parent first, so `/run` is mounted before
/// `/run/js-files`, and the engine creates the mount point itself. Nothing here assumes
/// `/run` exists in the image. The mount is built by the same [`mount_value`] as a job's
/// mounts (read-only, `nosuid,nodev` on Podman) and is in [`binds_of`], so the read-back
/// after `create` checks it like them: one that reads back writable is refused.
pub const FILES_TARGET: &str = "/run/js-files";

/// The most processes a job may hold when its limits name no bound: a fork bomb is
/// stopped well before it takes the host, and a build with many threads still fits.
pub const DEFAULT_PIDS: u64 = 4096;

/// A value that cannot be given to the runtime safely, or a limit it cannot honour.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    /// A mount path that is not absolute UTF-8 without a comma, a colon, a double quote
    /// or a control character: `--mount` is a comma-separated list, so any of those would
    /// let a path add options of its own (the colon is the separator of the short `-v`
    /// form, which is not used, and the mount roots refuse it too).
    #[error(
        "a mount path must be absolute UTF-8 without a comma, a colon, a quote or a control character"
    )]
    MountPath,
    /// A read-only mount that holds another mount point, on an engine without recursive
    /// read-only: the mount below it would stay writable.
    #[error(
        "a read-only mount holds another mount, which this engine would leave writable: use Docker 25 or later, or mount a directory without mount points"
    )]
    NestedMount,
    /// A read-only mount on an engine without recursive read-only, and no way to see
    /// whether it holds another mount.
    #[error(
        "/proc/self/mountinfo cannot be read, so a read-only mount cannot be checked for mounts below it"
    )]
    MountinfoUnreadable,
    /// A label whose key is empty or holds `=`.
    #[error("a label key must not be empty or hold '='")]
    LabelKey,
    /// A CPU, memory or process limit of 0, which the engines read as "no limit".
    #[error("a limit of 0 means no limit to the engine: cpu, memory and pids must be above zero")]
    ZeroLimit,
}

/// A bind mount of a container: the host path, the path inside, and whether it is
/// writable.
pub type Bind = (String, String, bool);

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
/// because `-v` splits on `:`. With `recursive_ro` a read-only bind also gets
/// `bind-recursive=readonly,bind-propagation=rprivate`, so a mount below it is read-only
/// too (Docker 25 and later; the engine refuses the container on a kernel before 5.12
/// rather than leave the submount writable). Podman gets `nosuid,nodev` on every mount; on
/// Docker, whose bind takes neither, the scratch directory is instead a volume of the local
/// driver that binds it with them ([`scratch_volume_options`]).
///
/// # Errors
///
/// A mount path that is not safe to put in the list ([`ArgsError::MountPath`]), or a CPU,
/// memory or process limit of 0 ([`ArgsError::ZeroLimit`]). The wire refuses both
/// already; this is the second check.
pub fn limits_to_args(
    engine: Engine,
    spec: &ContainerSpec,
    recursive_ro: bool,
) -> Result<Vec<String>, ArgsError> {
    if spec.cpu_millis == 0 || spec.memory_bytes == 0 || spec.pids == Some(0) {
        return Err(ArgsError::ZeroLimit);
    }
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
    let scratch = match engine {
        Engine::Docker => scratch_volume(&spec.scratch)?,
        Engine::Podman => mount_value(engine, &spec.scratch, SCRATCH_TARGET, false, recursive_ro)?,
    };
    args.extend(["--mount".to_owned(), scratch]);
    for mount in &spec.mounts {
        let value = mount_value(engine, &mount.host, &mount.target, mount.ro, recursive_ro)?;
        args.extend(["--mount".to_owned(), value]);
    }
    if let Some(files) = &spec.files {
        let value = mount_value(engine, files, FILES_TARGET, true, recursive_ro)?;
        args.extend(["--mount".to_owned(), value]);
    }
    Ok(args)
}

/// The bind mounts `spec` asks for, the scratch directory first and the run's files
/// (read-only) last: what the engine must
/// report back for the container after `create`.
pub fn binds_of(spec: &ContainerSpec) -> Vec<Bind> {
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let scratch = (text(&spec.scratch), SCRATCH_TARGET.to_owned(), true);
    let jobs = spec
        .mounts
        .iter()
        .map(|mount| (text(&mount.host), mount.target.clone(), !mount.ro));
    let files = spec
        .files
        .iter()
        .map(|dir| (text(dir), FILES_TARGET.to_owned(), false));
    std::iter::once(scratch).chain(jobs).chain(files).collect()
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

/// `field` of a `/proc/self/mountinfo` line with every `\NNN` octal escape (a space, tab,
/// newline, `#` or backslash in a path) turned back into its byte; an escape that is not
/// one byte, and a backslash that starts none, stay as written.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let digits = (bytes[at] == b'\\')
            .then(|| bytes.get(at + 1..at + 4))
            .flatten()
            .filter(|digits| digits.iter().all(|digit| (b'0'..=b'7').contains(digit)));
        let byte = digits.and_then(|digits| {
            let value = digits
                .iter()
                .fold(0_u32, |sum, digit| sum * 8 + u32::from(digit - b'0'));
            u8::try_from(value).ok()
        });
        match byte {
            Some(byte) => {
                out.push(byte);
                at += 4;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a mount point other than `host` itself is below it, in the text of a
/// `/proc/self/mountinfo` (the mount point is the fifth field).
pub fn has_submount(mountinfo: &str, host: &Path) -> bool {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape)
        .any(|point| Path::new(&point) != host && Path::new(&point).starts_with(host))
}

/// Refuses a read-only mount of `spec` that holds a mount point, for an engine that
/// cannot make the mounts below it read-only too. `mountinfo` is the agent's
/// `/proc/self/mountinfo`, `None` when it could not be read: then a read-only mount is
/// refused, because the check could not look (a job without one does not need the file).
///
/// It sees the mounts of the agent's own mount namespace, as they are when it looks: an
/// engine in another mount namespace is not covered (the agent supports only a local
/// rootful engine), and a mount added below the directory afterwards (only root can add
/// one) is not seen.
///
/// # Errors
///
/// [`ArgsError::NestedMount`], or [`ArgsError::MountinfoUnreadable`].
pub fn check_submounts(spec: &ContainerSpec, mountinfo: Option<&str>) -> Result<(), ArgsError> {
    let mut read_only = spec.mounts.iter().filter(|mount| mount.ro).peekable();
    match (read_only.peek().is_some(), mountinfo) {
        (false, _) => Ok(()),
        (true, None) => Err(ArgsError::MountinfoUnreadable),
        (true, Some(text)) => match read_only.any(|mount| has_submount(text, &mount.host)) {
            true => Err(ArgsError::NestedMount),
            false => Ok(()),
        },
    }
}

/// The options of the local driver that make the scratch volume a `nosuid,nodev` bind of
/// the directory: what `volume inspect` must print as `.Options` for it.
pub fn scratch_volume_options(scratch: &Path) -> serde_json::Value {
    serde_json::json!({
        "type": "none",
        "device": scratch.to_string_lossy(),
        "o": "bind,nosuid,nodev",
    })
}

/// The scratch directory as Docker's `--mount` value: a volume of the local driver that
/// binds the directory with `nosuid,nodev` (a `type=bind` mount of Docker takes neither),
/// anonymous so that `rm --volumes` removes it with the container, and `volume-nocopy` so
/// that the image's own `/scratch` is not copied into the host directory. The option list
/// holds commas, so that field is in double quotes: the value is read as CSV.
fn scratch_volume(scratch: &Path) -> Result<String, ArgsError> {
    let host = scratch.to_str().filter(|text| clean(text));
    host.map(|host| {
        format!(
            "type=volume,dst={SCRATCH_TARGET},volume-driver=local,volume-nocopy,\
             volume-opt=type=none,volume-opt=device={host},\"volume-opt=o=bind,nosuid,nodev\""
        )
    })
    .ok_or(ArgsError::MountPath)
}

/// Whether `text` can be given in a `--mount` list as it is: absolute, with no comma,
/// double quote, colon or control character.
fn clean(text: &str) -> bool {
    text.starts_with('/') && !text.contains([',', '"', ':']) && !text.chars().any(char::is_control)
}

/// A bind mount, as the value of `--mount`. Podman's `--mount` takes `nosuid,nodev` for a
/// bind, so there a set-uid file or a device node on the mount has no effect; Docker's
/// takes neither.
fn mount_value(
    engine: Engine,
    host: &Path,
    target: &str,
    ro: bool,
    recursive: bool,
) -> Result<String, ArgsError> {
    let host = host.to_str().filter(|text| clean(text));
    let target = Some(target).filter(|text| clean(text));
    let access = match (ro, recursive) {
        (false, _) => "",
        (true, false) => ",readonly",
        (true, true) => ",readonly,bind-recursive=readonly,bind-propagation=rprivate",
    };
    let unprivileged = match engine {
        Engine::Podman => ",nosuid,nodev",
        Engine::Docker => "",
    };
    host.zip(target)
        .map(|(host, target)| format!("type=bind,src={host},dst={target}{access}{unprivileged}"))
        .ok_or(ArgsError::MountPath)
}

/// What every contained job gets, whatever it asked for: no capability, no privilege
/// gain, the network the agent was configured with (none by default), a read-only root
/// with a `/tmp` that is a tmpfs (read-write, but no programs, no devices, no set-uid
/// files, and limited in size: Podman does not default to `noexec`; on Podman `/var/tmp`
/// and `/run` are such tmpfs mounts too), no core dumps, a private IPC namespace against a
/// daemon whose default is to share it, and the pull policy the agent was configured with
/// (never by default). The user is not here: it is the run's slot's (`user_args`). For
/// Podman the PID namespace is named private too; Docker takes only `host` or another
/// container there, and its default is private. There is no `--rm`: the lifecycle
/// removes the container itself, after it has read the result.
pub fn hardening_args(settings: &Settings, engine: Engine, seccomp: &str) -> Vec<String> {
    let tmpfs = |dir: &str| format!("{dir}:rw,noexec,nosuid,nodev,size={}", settings.tmp_size());
    let profile = format!("seccomp={seccomp}");
    let mut args: Vec<String> = [
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--security-opt",
        &profile,
        "--network",
        settings.network(),
        "--read-only",
        "--tmpfs",
        &tmpfs("/tmp"),
        "--ulimit",
        "core=0",
        "--ipc",
        "private",
    ]
    .map(str::to_owned)
    .to_vec();
    if engine == Engine::Podman {
        // Podman's `--read-only` also mounts writable tmpfs at `/var/tmp` and at the
        // image's run path, with neither `noexec` nor a size (`--read-only-tmpfs` is on
        // by default, and turning it off would make `/dev` and `/dev/shm` read-only).
        // A mount named here replaces the default one at its destination. Docker adds no
        // such mounts, so it gets no writable `/var/tmp` or `/run` it did not have.
        let at = args
            .iter()
            .position(|arg| arg == "--ulimit")
            .unwrap_or(args.len());
        let more = ["/var/tmp", "/run"]
            .into_iter()
            .flat_map(|dir| ["--tmpfs".to_owned(), tmpfs(dir)]);
        args.splice(at..at, more.collect::<Vec<_>>());
        args.extend(["--pid", "private"].map(str::to_owned));
    }
    args.extend(["--pull", settings.pull().as_str()].map(str::to_owned));
    args
}

/// The user of a run: `--user <uid>:<gid>` of its slot, one for each slot (see
/// [`Settings::slot_user`]), so two runs held at once never share a user.
pub(super) fn user_args(settings: &Settings, slot: u32) -> Vec<String> {
    vec!["--user".to_owned(), settings.slot_user(slot)]
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
