//! `js-agent crun-gpu-dropin`: the `--gpu-*` flags of the crun unit, made from the host's NVIDIA
//! device nodes before the unit starts.
//!
//! ```text
//! js-agent crun-gpu-dropin --out FILE [--unit FILE] [--dropin-dir DIR]... [--dev-root DIR] [--driver-root DIR] [--nvidia-smi PATH]
//! ```
//!
//! `js-agent-crun-gpu.service` runs it before each start of `js-agent-crun.service`, then reloads
//! systemd, so the agent starts with the flags that match the GPUs the host has now. It
//!
//! 1. runs `--nvidia-smi -L` when that flag is given (which also makes the driver create the
//!    nodes of the GPUs and `nvidiactl`) and reads the indices of the GPUs it lists; a list that
//!    differs from the `nvidia<N>` nodes is a warning;
//! 2. lists the nodes below `--dev-root` ([`Nodes`]): `nvidia<N>`, `nvidiactl`, `nvidia-modeset`,
//!    `nvidia-uvm`, `nvidia-uvm-tools` and the entries of `nvidia-caps/`;
//! 3. reads the `ExecStart=` that applies to the unit (`--unit`, then the `*.conf` drop-ins of
//!    its `.d` directory, of `--out`'s directory and of each `--dropin-dir`, by name, but not
//!    `--out`'s own name); a
//!    quoted word is refused, since the verb splits on white space; a `--flags-file` there is
//!    expanded as the agent expands it ([`crate::flags_file`]) for the checks below, and kept as
//!    it is in the drop-in, so the agent reads the file again at its own start;
//! 4. adds the flags ([`flags`]): `--gpu-device /dev/nvidia{index}` for the GPUs, one
//!    `--gpu-device` for each other node, the driver's `lib64` and `bin` below `--driver-root`
//!    as `--gpu-mount`s at [`LIB_TARGET`] and [`BIN_TARGET`], and `LD_LIBRARY_PATH` and `PATH`
//!    as `--gpu-env`s;
//! 5. reads each node the flags name below `--dev-root` (as the agent reads them in `/dev` at
//!    start: a character device), for the GPU indices `nvidia-smi -L` listed (the ones the agent
//!    checks at start) or, without that list, the ones the nodes name; then parses the whole
//!    command line as the agent does and checks its GPU flags with
//!    [`GpuPassthrough::from_cli`];
//! 6. writes the drop-in, `[Service]` with an `ExecStart=` reset and the command line, to a
//!    temporary file beside `--out` that is then renamed over it.
//!
//! Nothing is written, and an earlier `--out` is removed, when the host has no GPU node and
//! `nvidia-smi -L` lists no GPU (a host without a GPU starts as the unit says), when the unit
//! already lists a `--gpu-` flag (the operator's own flags win), and when a step fails: the agent
//! then starts without GPUs rather than with flags it would refuse. Exit 0 when the drop-in was written or there was nothing to
//! write, 1 when a step failed (the reason is on standard error), 2 on a usage error.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use crate::Cli;
use js_contain::container::DeviceNode;

use crate::crun::{GpuFlagError, GpuPassthrough, INDEX, NodeError, ReadNode};

/// The verb: `js-agent crun-gpu-dropin`.
pub(crate) const VERB: &str = "crun-gpu-dropin";

/// Where the driver's libraries are mounted in a GPU job.
pub(crate) const LIB_TARGET: &str = "/usr/local/nvidia/lib64";

/// Where the driver's programs are mounted in a GPU job.
pub(crate) const BIN_TARGET: &str = "/usr/local/nvidia/bin";

/// The directories of `PATH` after the driver's: the usual ones of a Linux image.
const PATH_TAIL: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The nodes every GPU job is given, when the host has them.
const COMMON: [&str; 4] = [
    "nvidiactl",
    "nvidia-modeset",
    "nvidia-uvm",
    "nvidia-uvm-tools",
];

/// The directory of the driver's capability nodes, below the dev root.
const CAPS: &str = "nvidia-caps";

/// The flags whose presence in the unit means the operator lists the GPU flags by hand.
const GPU_FLAG_PREFIX: &str = "--gpu-";

/// The verb's flags.
#[derive(Debug, Parser)]
#[command(name = "js-agent crun-gpu-dropin")]
struct Args {
    /// The drop-in to write.
    #[arg(long)]
    out: PathBuf,
    /// The unit whose `ExecStart=` the drop-in extends.
    #[arg(long, default_value = "/etc/systemd/system/js-agent-crun.service")]
    unit: PathBuf,
    /// A directory of `*.conf` drop-ins of the unit besides the unit's own `.d` directory and
    /// `--out`'s: the one systemd reads from `/etc/systemd/system` when the unit is a
    /// package's, in `/usr/lib/systemd/system`. It wins over those for a name in both.
    #[arg(long)]
    dropin_dir: Vec<PathBuf>,
    /// The directory the nodes are listed and read in (`/dev` on a host). The flags name them
    /// below `/dev`.
    #[arg(long, default_value = "/dev")]
    dev_root: PathBuf,
    /// The directory holding the driver's `lib64` and `bin`.
    #[arg(long, default_value = "/opt/nvidia-driver")]
    driver_root: PathBuf,
    /// The `nvidia-smi` whose `-L` list of GPUs the flags are checked against.
    #[arg(long)]
    nvidia_smi: Option<String>,
}

/// What the verb reads the host through.
pub(crate) struct Host<'a> {
    /// Runs `nvidia-smi -L` at the path given and returns what it printed.
    pub(crate) smi: &'a dyn Fn(&str) -> Result<String, String>,
    /// Reads a node, for the check of the flags: [`crate::crun::read_node`] on a machine.
    pub(crate) read: ReadNode,
    /// Expands a `--flags-file` in a command line: [`crate::flags_file::expand`] on a machine.
    pub(crate) expand: Expand<'a>,
}

/// How the verb expands a `--flags-file` in the unit's command line, as the agent does.
pub(crate) type Expand<'a> =
    &'a dyn Fn(Vec<OsString>) -> Result<Vec<OsString>, crate::flags_file::FlagsFileError>;

/// The NVIDIA nodes of a host.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Nodes {
    /// The indices of the `nvidia<N>` nodes.
    pub(crate) gpus: BTreeSet<u32>,
    /// The other nodes, by their path below the dev root.
    pub(crate) common: BTreeSet<String>,
}

impl Nodes {
    /// The NVIDIA nodes among `names` (the entries of the dev root) and `caps` (the entries of
    /// its `nvidia-caps` directory). `nvidia<N>` is a GPU when `N` is its index in decimal, with
    /// no leading zero; other names are not the driver's nodes a job needs.
    #[must_use]
    pub(crate) fn from_names(names: &[String], caps: &[String]) -> Self {
        let mut nodes = Self::default();
        for name in names {
            if COMMON.contains(&name.as_str()) {
                nodes.common.insert(name.clone());
            } else if let Some(index) = gpu_index(name) {
                nodes.gpus.insert(index);
            }
        }
        for cap in caps {
            nodes.common.insert(format!("{CAPS}/{cap}"));
        }
        nodes
    }
}

/// The index of a GPU node's name: `nvidia3` is 3; `nvidia03`, `nvidia` and `nvidiactl` are no
/// GPU's.
fn gpu_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("nvidia")?;
    let index: u32 = digits.parse().ok()?;
    (index.to_string() == digits).then_some(index)
}

/// The index of a line of `nvidia-smi -L` that lists a GPU: `GPU 3: NVIDIA A100 (UUID: ...)` is
/// 3; a MIG device's line, which is indented, is none.
fn listed_index(line: &str) -> Option<u32> {
    let (index, _) = line.strip_prefix("GPU ")?.split_once(':')?;
    index.parse().ok()
}

/// The GPU flags for `nodes` and the driver in `driver_root`: the per-GPU node, every other
/// node, the driver's two directories and the two variables, in that order, each node once and
/// the nodes in name order.
#[must_use]
pub(crate) fn flags(nodes: &Nodes, driver_root: &Path) -> Vec<String> {
    let mut flags = Vec::new();
    let mut add = |flag: &str, value: String| {
        flags.push(flag.to_owned());
        flags.push(value);
    };
    add("--gpu-device", format!("/dev/nvidia{INDEX}"));
    for node in &nodes.common {
        add("--gpu-device", format!("/dev/{node}"));
    }
    for (dir, target) in [("lib64", LIB_TARGET), ("bin", BIN_TARGET)] {
        add(
            "--gpu-mount",
            format!("{}:{target}", driver_root.join(dir).display()),
        );
    }
    add("--gpu-env", format!("LD_LIBRARY_PATH={LIB_TARGET}"));
    add("--gpu-env", format!("PATH={BIN_TARGET}:{PATH_TAIL}"));
    flags
}

/// The drop-in that runs `command` (the unit's `ExecStart=` value with the GPU flags).
#[must_use]
pub(crate) fn dropin(command: &str) -> String {
    format!(
        "# Written by `js-agent {VERB}` from the host's NVIDIA nodes; rewritten before each\n\
         # start. To list the GPU flags by hand, put them in the unit's ExecStart instead.\n\
         [Service]\nExecStart=\nExecStart={command}\n"
    )
}

/// The `ExecStart=` value that applies after `texts` (the unit, then its drop-ins in the order
/// systemd reads them), from their `[Service]` sections: the last one, where an empty value
/// clears what came before. A line that ends with a backslash goes on in the next.
#[must_use]
pub(crate) fn exec_start<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut found = None;
    for text in texts {
        let mut section = "";
        for line in logical_lines(text) {
            let line = line.trim();
            if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                section = if name == "Service" { "Service" } else { "" };
            } else if let Some((key, value)) = line.split_once('=')
                && key.trim_end() == "ExecStart"
                && section == "Service"
            {
                let value = value.trim();
                found = (!value.is_empty()).then(|| value.to_owned());
            }
        }
    }
    found
}

/// The lines of a unit file with each line that ends with a backslash joined to the next by a
/// space, as systemd joins them; a backslash on the last line joins it to nothing.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        match line.strip_suffix('\\') {
            Some(head) => {
                pending.push_str(head);
                pending.push(' ');
            }
            None => {
                pending.push_str(line);
                lines.push(std::mem::take(&mut pending));
            }
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }
    lines
}

/// Whether a generated word can stand in an `ExecStart=` line as it is: no white space, quote,
/// backslash, `%` specifier or `$` variable.
fn plain(word: &str) -> bool {
    word.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '/' | '_' | '.' | ':' | '=' | ',' | '{' | '}' | '+' | '-')
    })
}

/// Runs the verb on `args` (the program name, then the verb).
pub(crate) fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    let args = match Args::try_parse_from(args.iter().skip(1)) {
        Ok(args) => args,
        Err(why) => {
            let _ = write!(err, "{why}");
            return ExitCode::from(2);
        }
    };
    match generate(&args, out, err, host) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "{VERB}: {why}; no drop-in written");
            remove_stale(&args.out, err);
            ExitCode::from(1)
        }
    }
}

/// The steps of the verb; an error is why nothing is written.
fn generate(
    args: &Args,
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> Result<(), String> {
    let listed = args.nvidia_smi.as_deref().map(|smi| (smi, (host.smi)(smi)));
    let nodes = Nodes::from_names(&names(&args.dev_root)?, &caps(&args.dev_root.join(CAPS))?);
    let mut indices = nodes.gpus.clone();
    match listed {
        Some((smi, Ok(text))) => {
            // The agent checks the nodes of the indices nvidia-smi reports: so does the check.
            indices = text.lines().filter_map(listed_index).collect();
            if indices != nodes.gpus {
                let _ = writeln!(
                    err,
                    "{VERB}: warning: {smi} -L lists the GPUs {:?} and {} has the GPU nodes {:?}",
                    indices,
                    args.dev_root.display(),
                    nodes.gpus
                );
            }
        }
        Some((smi, Err(why))) => {
            let _ = writeln!(
                err,
                "{VERB}: warning: {smi} -L: {why}; the flags are checked against the nodes"
            );
        }
        None => {}
    }
    if indices.is_empty() && nodes.gpus.is_empty() {
        let _ = writeln!(
            out,
            "{VERB}: no GPU node in {}: no drop-in written",
            args.dev_root.display()
        );
        remove_stale(&args.out, err);
        return Ok(());
    }
    let base = unit_exec_start(&args.unit, &args.dropin_dir, &args.out)?;
    if base.contains(['"', '\'']) {
        return Err(format!(
            "the ExecStart of {} quotes a word, which this verb does not split as systemd does",
            args.unit.display()
        ));
    }
    let words: Vec<&str> = base.split_whitespace().collect();
    // The agent reads its flags file before it parses its command line; so do the checks.
    let expanded: Vec<String> = (host.expand)(words.iter().map(OsString::from).collect())
        .map_err(|why| format!("the ExecStart of {}: {why}", args.unit.display()))?
        .into_iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect();
    if expanded.iter().any(|w| w.starts_with(GPU_FLAG_PREFIX)) {
        let _ = writeln!(
            out,
            "{VERB}: the ExecStart of {} lists GPU flags already: no drop-in written",
            args.unit.display()
        );
        remove_stale(&args.out, err);
        return Ok(());
    }
    let gpu_flags = flags(&nodes, &args.driver_root);
    if let Some(word) = gpu_flags.iter().find(|w| !plain(w)) {
        return Err(format!(
            "{word:?} cannot stand in an ExecStart= line (white space, a quote, a backslash, % or $)"
        ));
    }
    let line: Vec<&str> = words
        .iter()
        .copied()
        .chain(gpu_flags.iter().map(String::as_str))
        .collect();
    let checked = expanded
        .iter()
        .map(String::as_str)
        .chain(gpu_flags.iter().map(String::as_str));
    let cli = Cli::try_parse_from(checked).map_err(|why| {
        format!(
            "the ExecStart of {} with the GPU flags is not an agent command line: {}",
            args.unit.display(),
            why.render().to_string().trim_end()
        )
    })?;
    let indices: Vec<u32> = indices.into_iter().collect();
    // The nodes are read where they were listed: in the dev root, which is `/dev` on a host.
    let gpu_nodes = indices.iter().map(|i| format!("nvidia{i}"));
    for node in gpu_nodes.chain(nodes.common.iter().cloned()) {
        (host.read)(&args.dev_root.join(node))
            .map_err(|why| GpuFlagError::Node(why).to_string())?;
    }
    GpuPassthrough::from_cli(&cli, &indices, read_already).map_err(|why| why.to_string())?;
    let command = line.join(" ");
    write_atomic(&args.out, &dropin(&command))
        .map_err(|why| format!("{}: {why}", args.out.display()))?;
    let _ = writeln!(
        out,
        "{VERB}: wrote {} for {} GPUs: {}",
        args.out.display(),
        indices.len(),
        gpu_flags.join(" ")
    );
    Ok(())
}

/// The names in the dev root, in no order.
fn names(dir: &Path) -> Result<Vec<String>, String> {
    std::fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .map_err(|why| format!("{}: {why}", dir.display()))
}

/// The names in the capability directory: none when there is no such directory.
fn caps(dir: &Path) -> Result<Vec<String>, String> {
    if dir.is_dir() {
        names(dir)
    } else {
        Ok(Vec::new())
    }
}

/// The `ExecStart=` that applies to `unit`: the unit's, then that of each `*.conf` drop-in of the
/// unit's `.d` directory and of `out`'s directory (where systemd's runtime drop-ins are), by name,
/// the unit's own directory winning for a name in both, then each of `extra` in order, and leaving
/// out `out`'s own name.
fn unit_exec_start(unit: &Path, extra: &[PathBuf], out: &Path) -> Result<String, String> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|why| format!("{}: {why}", path.display()))
    };
    let mut texts = vec![read(unit)?];
    let mut own_dir = unit.as_os_str().to_owned();
    own_dir.push(".d");
    let own_name = out
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let mut confs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let dirs = [out.parent(), Some(Path::new(&own_dir))]
        .into_iter()
        .flatten()
        .chain(extra.iter().map(PathBuf::as_path));
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        for name in names(dir)? {
            if name.ends_with(".conf") && Some(&name) != own_name.as_ref() {
                confs.insert(name.clone(), dir.join(name));
            }
        }
    }
    for path in confs.values() {
        texts.push(read(path)?);
    }
    exec_start(texts.iter().map(String::as_str))
        .ok_or_else(|| format!("{} has no ExecStart= in [Service]", unit.display()))
}

/// Writes `text` to a temporary file beside `path` and renames it over `path`, making the
/// directory first.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".new");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

/// The reader [`GpuPassthrough::from_cli`] is given: each node was read in the dev root before,
/// so this one accepts the `/dev` path the flag names.
fn read_already(path: &Path) -> Result<DeviceNode, NodeError> {
    Ok(DeviceNode {
        path: path.to_string_lossy().into_owned(),
        major: 0,
        minor: 0,
        uid: 0,
        gid: 0,
        mode: 0,
    })
}

/// Removes a drop-in an earlier run wrote, so the unit is as it is without one.
fn remove_stale(path: &Path, err: &mut dyn Write) {
    match std::fs::remove_file(path) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => {
            let _ = writeln!(
                err,
                "{VERB}: warning: the earlier drop-in {} cannot be removed: {why}",
                path.display()
            );
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "gpu_dropin_tests.rs"]
mod tests;
