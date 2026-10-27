//! What the host is: its CPUs, memory, GPUs and machine identity, read through two seams.
//!
//! [`CommandRunner`] is the only place a program is started and [`FileReader`] the only
//! place a file is read, so a test feeds recorded output instead of touching the machine.
//! Nothing here is invented: a probe that fails or prints something unreadable is an
//! error, and for the GPU probe (which is allowed to fail) the caller reports no GPU and
//! says why.
//!
//! # Helper programs
//!
//! Every helper (`getconf`, `nvidia-smi`, `sysctl`, `system_profiler`, `ioreg`) is started
//! by its absolute path in a system directory (`/usr/bin`, `/usr/sbin`), never by a bare
//! name looked up on the `PATH` the agent inherited. The agent runs on a machine whose
//! environment it does not control, and what a helper prints becomes the host's offer. The
//! trust assumption is that those directories are writable by root only, as on a stock
//! Linux or macOS system; a host where an unprivileged user can replace `/usr/bin/nvidia-smi`
//! has bigger problems than the agent can solve. The helpers get the agent's environment
//! ([`CommandRunner::run`]); the container engine does not: it is started with
//! [`CommandRunner::run_in`], whose environment is exactly the one given.
//!
//! # Bounds
//!
//! A helper can hang (a wedged GPU driver blocks `nvidia-smi` in the kernel) or print
//! without end. [`SystemCommands`] therefore waits for each helper at most
//! [`PROBE_LIMIT`] for its output and as long again for its exit, reads at most
//! [`PROBE_OUTPUT_CAP`] bytes of output, and kills a helper that breaks either bound by
//! its pid and collects it. A helper that was killed, printed too much or failed is an
//! error like any other: no GPU for the GPU probe, no start for the CPU count. A stop
//! request that arrives while a helper runs is kept and ends the agent at its first wait.
//!
//! # Containers
//!
//! The totals of `getconf` and `/proc/meminfo` are the whole machine's. On Linux the
//! offer is the lowest of that total and what the process may use: the CPU quota
//! (`cpu.max`) and memory limit (`memory.max`) of its own cgroup and of each cgroup above
//! it (cgroup v2, found through `/proc/self/cgroup`), and the CPUs it may run on
//! (`Cpus_allowed_list` of `/proc/self/status`: `taskset`, a cpuset, a container's
//! `--cpuset-cpus`). A file that is absent or unreadable is no limit, as is cgroup v1 (a
//! `/proc/self/cgroup` with version 1 lines and no `0::` line), which is reported as a
//! warning because the limits it may hold are not read. When `getconf` is absent the
//! standard library's CPU count stands in, with a warning. On macOS the totals are the
//! offer. A flag can lower the result further and never
//! raise it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, PipeReader, Read};
use std::num::NonZeroUsize;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use js_agent_wire::{GpuDevice, Hello, Phase, ProtocolRange, WireShape};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::container;

/// The GPU class an Apple GPU is reported under.
pub const APPLE_GPU_CLASS: &str = "apple-m";

// The helper programs, each by the absolute path it has on a stock system, never by a bare
// name looked up on the `PATH` the agent inherited (see the module documentation).
const GETCONF: &str = "/usr/bin/getconf";
const NVIDIA_SMI: &str = "/usr/bin/nvidia-smi";
const SYSCTL: &str = "/usr/sbin/sysctl";
const SYSTEM_PROFILER: &str = "/usr/sbin/system_profiler";
const IOREG: &str = "/usr/sbin/ioreg";

/// Runs a program and returns what it printed.
pub trait CommandRunner {
    /// Runs `program` with `args` and returns its standard output when it exits with
    /// status 0. The error says which program failed and how, never what it printed.
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;

    /// Like [`CommandRunner::run`], but the program's environment is exactly `env`: the
    /// environment of the agent is not passed on.
    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String>;
}

/// Reads a text file.
pub trait FileReader {
    /// The text of the file at `path`, or the system's reason it cannot be read.
    fn read(&self, path: &str) -> Result<String, String>;
}

/// How long a helper may take to print its output, and again to exit once it has.
pub const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The most output of a helper that is accepted, in bytes. The longest real output (the
/// `system_profiler` JSON of a Mac with several displays) is a few KiB.
pub const PROBE_OUTPUT_CAP: usize = 16 * 1024;

/// How long a killed helper is given to be collected before the runner reports its failure
/// anyway: a process stuck inside the kernel does not die until the call returns.
const COLLECT_WAIT: Duration = Duration::from_secs(5);

/// The machine's own commands: the one place `std::process` runs. A helper is waited for
/// at most `limit` for its output and `limit` again for its exit, and at most `cap` bytes
/// of its output are accepted; a helper that breaks either bound is killed by its pid and
/// collected, and the call fails (see the module documentation).
#[derive(Debug, Clone, Copy)]
pub struct SystemCommands {
    limit: Duration,
    cap: usize,
}

impl SystemCommands {
    /// A runner with its own bounds, for a test that must not wait [`PROBE_LIMIT`].
    pub const fn bounded(limit: Duration, cap: usize) -> Self {
        Self { limit, cap }
    }

    /// Waits for what `child` prints and for its exit, killing it when it overruns.
    fn supervise(&self, program: &str, child: Child, reader: PipeReader) -> Result<String, String> {
        let pid = child.id();
        let (printed, output) = mpsc::channel();
        let (exited, exit) = mpsc::channel();
        let cap = self.cap;
        // This thread owns the child from here on and always collects it, so the caller
        // never blocks on a process that does not die.
        thread::spawn(move || watch(child, reader, cap, &printed, &exited));
        let give_up = |why: String| {
            kill(pid);
            let _ = exit.recv_timeout(COLLECT_WAIT);
            format!("{program}: {why}")
        };
        let timed_out = || give_up(format!("timed out after {:?}", self.limit));
        output
            .recv_timeout(self.limit)
            .map_err(|_| timed_out())
            .and_then(|bytes| match bytes.len() > cap {
                true => Err(give_up(format!("printed more than {cap} bytes"))),
                false => Ok(bytes),
            })
            .and_then(|bytes| {
                exit.recv_timeout(self.limit)
                    .map_err(|_| timed_out())
                    .and_then(|status| status.map_err(|why| format!("{program}: {why}")))
                    .map(|()| String::from_utf8_lossy(&bytes).into_owned())
            })
    }
}

impl Default for SystemCommands {
    /// The bounds the program runs with: [`PROBE_LIMIT`] and [`PROBE_OUTPUT_CAP`].
    fn default() -> Self {
        Self::bounded(PROBE_LIMIT, PROBE_OUTPUT_CAP)
    }
}

impl SystemCommands {
    /// Starts `program` and supervises it. `env` is the whole environment of the program
    /// when given, and the agent's own when not.
    fn start(
        &self,
        program: &str,
        args: &[&str],
        env: Option<&[(&str, &str)]>,
    ) -> Result<String, String> {
        let named = |err: io::Error| format!("{program}: {err}");
        io::pipe().map_err(named).and_then(|(reader, writer)| {
            let mut command = Command::new(program);
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(writer)
                .stderr(Stdio::null());
            if let Some(env) = env {
                command.env_clear().envs(env.iter().copied());
            }
            let spawned = command.spawn();
            // The command (and with it the parent's copy of the pipe's write end) is gone
            // here, so the reader sees the end of the output.
            drop(command);
            spawned
                .map_err(named)
                .and_then(|child| self.supervise(program, child, reader))
        })
    }
}

impl CommandRunner for SystemCommands {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.start(program, args, None)
    }

    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        self.start(program, args, Some(env))
    }
}

/// Reads at most `cap` and one more byte of the child's output, reports it, then waits for
/// the child to exit and reports whether it succeeded (the text of the failure when not).
/// It does both however the reads and the wait end, so the child is always collected.
fn watch(
    mut child: Child,
    reader: PipeReader,
    cap: usize,
    printed: &Sender<Vec<u8>>,
    exited: &Sender<Result<(), String>>,
) {
    let mut bytes = Vec::new();
    let most = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    let _ = reader.take(most).read_to_end(&mut bytes);
    let _ = printed.send(bytes);
    let lost = (false, "could not be collected".to_owned());
    let (success, text) = child
        .wait()
        .map_or(lost, |status| (status.success(), status.to_string()));
    let _ = exited.send(success.then_some(()).ok_or(text));
}

/// Sends SIGKILL to the process `pid`. A process that is already gone is not an error.
fn kill(pid: u32) {
    let pid = i32::try_from(pid).ok().and_then(Pid::from_raw);
    pid.into_iter().for_each(|pid| {
        let _ = kill_process(pid, Signal::KILL);
    });
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

    /// Whether this is a Mac.
    pub fn is_macos(&self) -> bool {
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
    let cpu = cpu_millis(runner, thread::available_parallelism)
        .map(|(host, note)| {
            (
                within(platform, host, || container::cpu_millis(files)),
                note,
            )
        })
        .map_err(|why| format!("cannot count the CPUs: {why}"));
    let memory = memory_bytes(platform, runner, files)
        .map(|host| within(platform, host, || container::memory_bytes(files)))
        .map_err(|why| format!("cannot size memory: {why}"));
    let probed = probe_gpus(platform, runner);
    let gpu_warning = probed
        .as_ref()
        .err()
        .map(|why| format!("GPU probe failed ({why}); offering no GPU"));
    let cgroup_warning = match platform.is_macos() {
        true => None,
        false => container::warning(files),
    };
    cpu.and_then(|(cpu_millis, cpu_note)| {
        memory.map(|memory_bytes| Detected {
            cpu_millis,
            memory_bytes,
            gpus: probed.unwrap_or_else(|_| Gpus::none()),
            warnings: [cpu_note, cgroup_warning, gpu_warning]
                .into_iter()
                .flatten()
                .collect(),
        })
    })
}

/// The host's `total` lowered to what the process may use, when `limit` finds that it is
/// less. A Mac has no such limit the agent reads, and a limit above the total changes
/// nothing.
fn within(platform: &Platform, total: u64, limit: impl FnOnce() -> Option<u64>) -> u64 {
    match platform.is_macos() {
        true => total,
        false => limit().map_or(total, |limit| total.min(limit)),
    }
}

/// A number printed alone on a line.
fn number(text: &str, what: &str) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .map_err(|_| format!("{what} did not print a number"))
}

/// Online CPUs, as `getconf` counts them on Linux and macOS alike, in thousandths. When
/// `getconf` cannot be run or fails, `count` (the standard library's count) stands in and
/// the second value says so, for a warning; `getconf` printing no number is still an error.
fn cpu_millis(
    runner: &dyn CommandRunner,
    count: fn() -> io::Result<NonZeroUsize>,
) -> Result<(u64, Option<String>), String> {
    match runner.run(GETCONF, &["_NPROCESSORS_ONLN"]) {
        Ok(text) => number(&text, "getconf").map(|cpus| (cpus.saturating_mul(1000), None)),
        Err(why) => count()
            .map_err(|err| format!("{why}; the standard library cannot count them either: {err}"))
            .map(|cpus| (cpus.get() as u64 * 1000, Some(fallback_note(&why)))),
    }
}

fn fallback_note(why: &str) -> String {
    format!("{why}; counted the CPUs with the standard library instead")
}

fn memory_bytes(
    platform: &Platform,
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
) -> Result<u64, String> {
    match platform.is_macos() {
        true => runner
            .run(SYSCTL, &["-n", "hw.memsize"])
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
    let probed = match platform.is_macos() {
        true => runner
            .run(SYSTEM_PROFILER, &["SPDisplaysDataType", "-json"])
            .and_then(|text| apple(&text)),
        false => runner
            .run(
                NVIDIA_SMI,
                &[
                    "--query-gpu=index,uuid,name,memory.total",
                    "--format=csv,noheader,nounits",
                ],
            )
            .and_then(|text| nvidia(&text)),
    };
    probed.and_then(accepted)
}

/// The GPUs, when the wire accepts them: the texts a driver printed (a UUID, a model) are
/// held to the wire's rules here, so a long or odd name costs the host its GPUs and a
/// warning and not its registration. The check is a hello that is valid but for the GPU
/// facts, so no rule is copied here and nothing the operator passed as a flag is judged.
fn accepted(gpus: Gpus) -> Result<Gpus, String> {
    let probe = Hello {
        protocol: ProtocolRange::THIS,
        host_id: "gpu-check".to_owned(),
        pool: "gpu-check".to_owned(),
        agent_version: "0".to_owned(),
        instance: None,
        shape: WireShape {
            cpu_millis: 0,
            memory_bytes: 0,
            gpus: gpus.count,
            gpu_class: gpus.class.clone(),
            max_leases: None,
        },
        capabilities: BTreeMap::new(),
        gpu_devices: gpus.devices.clone(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
    };
    probe
        .validate()
        .map(|()| gpus)
        .map_err(|why| format!("the wire refuses the GPU facts: {why}"))
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
            .run(IOREG, &["-rd1", "-c", "IOPlatformExpertDevice"])
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Absent;

    impl CommandRunner for Absent {
        fn run(&self, program: &str, _args: &[&str]) -> Result<String, String> {
            Err(format!("{program}: not found"))
        }

        fn run_in(
            &self,
            program: &str,
            args: &[&str],
            _env: &[(&str, &str)],
        ) -> Result<String, String> {
            self.run(program, args)
        }
    }

    /// Asserts that a host where `getconf` is absent and the standard library cannot count
    /// the CPUs either fails with both reasons, which the public seams cannot arrange.
    #[test]
    fn no_getconf_and_no_standard_count_names_both() {
        assert_eq!(Absent.run_in("x", &[], &[]), Err("x: not found".to_owned()));
        let said = cpu_millis(&Absent, || Err(io::Error::other("no count"))).unwrap_err();
        assert_eq!(
            said,
            "/usr/bin/getconf: not found; the standard library cannot count them either: no count"
        );
    }
}
