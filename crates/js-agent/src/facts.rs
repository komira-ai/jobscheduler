//! What the host is: its CPUs, memory, GPUs and machine identity, read through two seams.
//!
//! [`CommandRunner`] is the only place a program is started and [`FileReader`] the only
//! place a file is read, so a test feeds recorded output instead of touching the machine.
//! Nothing here is invented: a probe that fails or prints something unreadable is an
//! error, and for the GPU probe (which is allowed to fail) the caller reports no GPU and
//! says why.

use std::collections::BTreeSet;
use std::process::{Command, Stdio};

use js_agent_wire::GpuDevice;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The GPU class an Apple GPU is reported under.
pub const APPLE_GPU_CLASS: &str = "apple-m";

/// Runs a program and returns what it printed.
pub trait CommandRunner {
    /// Runs `program` with `args` and returns its standard output when it exits with
    /// status 0. The error says which program failed and how, never what it printed.
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;
}

/// Reads a text file.
pub trait FileReader {
    /// The text of the file at `path`, or the system's reason it cannot be read.
    fn read(&self, path: &str) -> Result<String, String>;
}

/// The machine's own commands: the one place `std::process` runs.
#[derive(Debug, Clone, Copy)]
pub struct SystemCommands;

impl CommandRunner for SystemCommands {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|err| format!("{program}: {err}"))
            .and_then(|output| match output.status.success() {
                true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
                false => Err(format!("{program}: {}", output.status)),
            })
    }
}

/// The machine's own files.
#[derive(Debug, Clone, Copy)]
pub struct SystemFiles;

impl FileReader for SystemFiles {
    fn read(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))
    }
}

/// The operating system and CPU architecture the agent runs on, as the capability values
/// `os` and `arch` report them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `macos`, `linux`, ...
    pub os: String,
    /// `aarch64`, `x86_64`, ...
    pub arch: String,
}

impl Platform {
    /// The platform this binary was built for.
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    fn is_macos(&self) -> bool {
        self.os == "macos"
    }
}

/// The GPUs a probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpus {
    /// How many whole GPUs the host has.
    pub count: u64,
    /// Their class, present exactly when `count` is above zero.
    pub class: Option<String>,
    /// One entry per GPU when the probe can tell them apart (an NVIDIA host); empty for
    /// an Apple GPU, which has no identity the probe reads.
    pub devices: Vec<GpuDevice>,
}

impl Gpus {
    /// No GPU.
    pub fn none() -> Self {
        Self {
            count: 0,
            class: None,
            devices: Vec::new(),
        }
    }
}

/// What a host has, before the operator's flags lower any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// The GPUs, none when the probe failed.
    pub gpus: Gpus,
    /// Things worth the operator's attention, such as a failed GPU probe.
    pub warnings: Vec<String>,
}

/// Detects the CPUs, memory and GPUs.
///
/// # Errors
///
/// The CPU count or the memory size cannot be read: the host would have to guess. A GPU
/// probe that fails is not an error; it reports no GPU and a warning.
pub fn detect(
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<Detected, String> {
    let cpu = cpu_millis(runner).map_err(|why| format!("cannot count the CPUs: {why}"));
    let memory =
        memory_bytes(platform, runner, files).map_err(|why| format!("cannot size memory: {why}"));
    let probed = probe_gpus(platform, runner);
    let warnings = probed
        .as_ref()
        .err()
        .map(|why| format!("GPU probe failed ({why}); offering no GPU"))
        .into_iter()
        .collect();
    cpu.and_then(|cpu_millis| {
        memory.map(|memory_bytes| Detected {
            cpu_millis,
            memory_bytes,
            gpus: probed.unwrap_or_else(|_| Gpus::none()),
            warnings,
        })
    })
}

/// A number printed alone on a line.
fn number(text: &str, what: &str) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .map_err(|_| format!("{what} did not print a number"))
}

/// Online CPUs, as `getconf` counts them on Linux and macOS alike, in thousandths.
fn cpu_millis(runner: &dyn CommandRunner) -> Result<u64, String> {
    runner
        .run("getconf", &["_NPROCESSORS_ONLN"])
        .and_then(|text| number(&text, "getconf"))
        .map(|cpus| cpus.saturating_mul(1000))
}

fn memory_bytes(
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<u64, String> {
    match platform.is_macos() {
        true => runner
            .run("sysctl", &["-n", "hw.memsize"])
            .and_then(|text| number(&text, "sysctl")),
        false => files
            .read("/proc/meminfo")
            .and_then(|text| meminfo_total(&text)),
    }
}

/// `MemTotal` of `/proc/meminfo`, which is in kibibytes.
fn meminfo_total(text: &str) -> Result<u64, String> {
    text.lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .ok_or_else(|| "/proc/meminfo has no MemTotal".to_owned())
        .and_then(|rest| number(rest.trim().trim_end_matches("kB"), "MemTotal"))
        .map(|kibibytes| kibibytes.saturating_mul(1024))
}

/// The GPUs of the host: Apple's through `system_profiler` on a Mac, NVIDIA's through
/// `nvidia-smi` anywhere else.
fn probe_gpus(platform: &Platform, runner: &dyn CommandRunner) -> Result<Gpus, String> {
    match platform.is_macos() {
        true => runner
            .run("system_profiler", &["SPDisplaysDataType", "-json"])
            .and_then(|text| apple(&text)),
        false => runner
            .run(
                "nvidia-smi",
                &[
                    "--query-gpu=index,uuid,name,memory.total",
                    "--format=csv,noheader,nounits",
                ],
            )
            .and_then(|text| nvidia(&text)),
    }
}

/// The Apple GPUs in `system_profiler SPDisplaysDataType -json`: one for each display
/// adapter whose vendor is Apple. A Mac with another vendor's GPU offers none.
fn apple(text: &str) -> Result<Gpus, String> {
    serde_json::from_str::<Value>(text)
        .map_err(|_| "system_profiler printed no JSON".to_owned())
        .and_then(|json| {
            json.get("SPDisplaysDataType")
                .and_then(Value::as_array)
                .map(|adapters| adapters.iter().filter(|a| is_apple(a)).count() as u64)
                .ok_or_else(|| "system_profiler printed no SPDisplaysDataType list".to_owned())
        })
        .map(|count| Gpus {
            count,
            class: (count > 0).then(|| APPLE_GPU_CLASS.to_owned()),
            devices: Vec::new(),
        })
}

fn is_apple(adapter: &Value) -> bool {
    adapter
        .get("spdisplays_vendor")
        .and_then(Value::as_str)
        .is_some_and(|vendor| vendor.contains("Apple"))
}

/// The GPUs in `nvidia-smi --query-gpu=index,uuid,name,memory.total
/// --format=csv,noheader,nounits` (memory in MiB). Every GPU must be of one model: the
/// wire states one class for the host.
fn nvidia(text: &str) -> Result<Gpus, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(device)
        .collect::<Result<Vec<_>, _>>()
        .and_then(one_model)
}

fn device(line: &str) -> Result<GpuDevice, String> {
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    match fields.as_slice() {
        [index, uuid, model, mib] => index
            .parse::<u32>()
            .ok()
            .zip(mib.parse::<u64>().ok())
            .map(|(index, mib)| GpuDevice {
                index,
                uuid: (*uuid).to_owned(),
                model: (*model).to_owned(),
                memory_bytes: mib.saturating_mul(1024 * 1024),
            })
            .ok_or_else(|| "nvidia-smi printed an index or memory size that is no number".into()),
        other => Err(format!(
            "nvidia-smi printed a line of {} fields, not 4",
            other.len()
        )),
    }
}

fn one_model(devices: Vec<GpuDevice>) -> Result<Gpus, String> {
    let classes: BTreeSet<Option<String>> = devices.iter().map(|d| class_of(&d.model)).collect();
    match (classes.len(), classes.into_iter().next().flatten()) {
        (0, _) => Ok(Gpus::none()),
        (1, Some(class)) => Ok(Gpus {
            count: devices.len() as u64,
            class: Some(class),
            devices,
        }),
        (1, None) => Err("a GPU model name holds no letter or digit".to_owned()),
        _ => Err("the GPUs are of more than one model".to_owned()),
    }
}

/// A GPU class from a model name: lower case words joined by `-`, so `NVIDIA
/// A100-SXM4-80GB` is `nvidia-a100-sxm4-80gb`.
fn class_of(model: &str) -> Option<String> {
    let words: Vec<String> = model
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect();
    (!words.is_empty()).then(|| words.join("-"))
}

/// The id the operating system gave this installation: `/etc/machine-id` on Linux, the
/// platform UUID `ioreg` prints on a Mac.
pub fn machine_id(
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<String, String> {
    match platform.is_macos() {
        true => runner
            .run("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
            .and_then(|text| platform_uuid(&text)),
        false => files
            .read("/etc/machine-id")
            .and_then(|text| match text.trim() {
                "" => Err("/etc/machine-id is empty".to_owned()),
                id => Ok(id.to_owned()),
            }),
    }
}

/// The value of the `IOPlatformUUID` line of `ioreg`.
fn platform_uuid(text: &str) -> Result<String, String> {
    text.lines()
        .find(|line| line.contains("IOPlatformUUID"))
        .and_then(|line| line.rsplit('"').nth(1))
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "ioreg printed no IOPlatformUUID".to_owned())
}

/// The host id derived from a machine id: a salted SHA-256, so the raw id (which names
/// the installation to anything that reads the listing) never leaves the host. The
/// result is `h-` and 32 lower case hex digits, a valid host id.
pub fn host_id(machine_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"js-agent host id v1\0");
    hash.update(machine_id.as_bytes());
    let digest = hash.finalize();
    format!("h-{}", hex::encode(&digest[..16]))
}
