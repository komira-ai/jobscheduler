//! What the tests share: recorded program output, fake hosts, a scripted transport, and
//! a sleeper and a jitter that a test decides.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use js_agent::{
    Answer, CommandRunner, Connector, FileReader, Jitter, Platform, Services, Sleeper, Transport,
    Wake, run_with,
};
use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_client::ClientError;
use js_testkit::dir::TestDir;
use serde_json::{Value, json};

/// The pool the tests' hosts ask to join.
pub const POOL: &str = "macs";

/// The session secret of the scripted welcomes. A test that looks for it in output is
/// looking for a leak.
pub const SECRET: &str = "sess-secret-0001-abcdefghijklmnop";

/// A directory under the target directory that holds one test's files and is removed when
/// the test ends.
pub struct Scratch {
    dir: PathBuf,
    _owner: TestDir,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let owner = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-agent"),
            name,
        );
        Self {
            dir: owner.path().to_path_buf(),
            _owner: owner,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes `text` to the file `name` with `mode` and returns its path.
    pub fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

/// Asserts that two scratch directories made under one name are two directories: the
/// second one's set-up does not remove the first one's file and dropping the second does
/// not remove the first. It catches a path named by the test alone, which two tests of
/// one name, or two runs of one test binary at once, would share and delete under each
/// other.
#[test]
fn scratches_of_one_name_are_separate_and_each_removes_only_itself() {
    let first = Scratch::new("same-name");
    first.write("f", "one", 0o600);
    let second = Scratch::new("same-name");
    assert_ne!(first.path("f"), second.path("f"));
    assert!(
        first.path("f").exists(),
        "a second scratch removed the first"
    );
    second.write("f", "two", 0o600);
    let kept = first.path("f");
    drop(second);
    assert_eq!(fs::read(kept).unwrap(), b"one");
}

/// The probes' command lines. Every program is named by its absolute path: the agent never
/// looks a helper up on the `PATH` it inherited.
pub const NVIDIA_CMD: &str = "/usr/bin/nvidia-smi --query-gpu=index,uuid,name,memory.total \
     --format=csv,noheader,nounits";
pub const PROFILER_CMD: &str = "/usr/sbin/system_profiler SPDisplaysDataType -json";
pub const CPU_CMD: &str = "/usr/bin/getconf _NPROCESSORS_ONLN";
pub const MEMSIZE_CMD: &str = "/usr/sbin/sysctl -n hw.memsize";
pub const IOREG_CMD: &str = "/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice";

/// `nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv,noheader,nounits` on
/// a host with two A100s.
pub const NVIDIA_TWO_A100: &str =
    "0, GPU-3f1c9a52-0b7e-4d0a-9a55-6f1d2c8b7e01, NVIDIA A100-SXM4-80GB, 81920
1, GPU-77a0c2d4-5e19-4b3f-8c66-0d9e1f2a3b02, NVIDIA A100-SXM4-80GB, 81920
";

/// The same command on a host with an A100 and an H100.
pub const NVIDIA_MIXED: &str =
    "0, GPU-3f1c9a52-0b7e-4d0a-9a55-6f1d2c8b7e01, NVIDIA A100-SXM4-80GB, 81920
1, GPU-88b1d3e5-6f2a-4c40-9d77-1e0f2a3b4c03, NVIDIA H100 80GB HBM3, 81559
";

/// `system_profiler SPDisplaysDataType -json` on an Apple silicon Mac.
pub const APPLE_M2: &str = r#"{
  "SPDisplaysDataType" : [
    {
      "_name" : "Apple M2 Pro",
      "spdisplays_mtlgpufamilysupport" : "spdisplays_metal3",
      "spdisplays_ndrvs" : [
        {
          "_name" : "Color LCD",
          "_spdisplays_display-product-id" : "ae2c",
          "spdisplays_main" : "spdisplays_yes"
        }
      ],
      "spdisplays_vendor" : "sppci_vendor_Apple",
      "sppci_bus" : "spdisplays_builtin",
      "sppci_cores" : "19",
      "sppci_device_type" : "spdisplays_gpu",
      "sppci_model" : "Apple M2 Pro"
    }
  ]
}
"#;

/// The same command on an Intel Mac whose only GPU is AMD's.
pub const AMD_MAC: &str = r#"{
  "SPDisplaysDataType" : [
    {
      "_name" : "AMD Radeon Pro 5500M",
      "spdisplays_vendor" : "sppci_vendor_amd",
      "sppci_bus" : "spdisplays_pcie_device",
      "sppci_device_type" : "spdisplays_gpu",
      "sppci_model" : "AMD Radeon Pro 5500M"
    }
  ]
}
"#;

/// `/proc/meminfo` of a host with 32 GiB.
pub const MEMINFO: &str = "MemTotal:       33554432 kB
MemFree:         1048576 kB
MemAvailable:   20971520 kB
";

/// The machine id of the Linux fake host.
pub const MACHINE_ID: &str = "3f0a6c1d9e8b4a7c92d1e5f60718293a";

/// The platform UUID of the Mac fake host.
pub const PLATFORM_UUID: &str = "6B1A0C2E-1111-4222-8333-444455556666";

/// `ioreg -rd1 -c IOPlatformExpertDevice` on a Mac.
pub fn ioreg() -> String {
    format!(
        "+-o Root  <class IORegistryEntry, id 0x100000100, retain 20>
  +-o J314sAP  <class IOPlatformExpertDevice, id 0x10000020f, registered, matched, active, busy 0 (97 ms), retain 40>
    {{
      \"IOPlatformSerialNumber\" = \"XXXXXXXXXX\"
      \"IOPlatformUUID\" = \"{PLATFORM_UUID}\"
    }}
"
    )
}

/// Programs and their recorded output. A program not listed is not found.
#[derive(Clone, Default)]
pub struct FakeCommands {
    answers: BTreeMap<String, Result<String, String>>,
    pub ran: Rc<RefCell<Vec<String>>>,
}

impl FakeCommands {
    pub fn with(mut self, line: &str, answer: Result<&str, &str>) -> Self {
        let answer = answer.map(str::to_owned).map_err(str::to_owned);
        self.answers.insert(line.to_owned(), answer);
        self
    }
}

impl CommandRunner for FakeCommands {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let line = format!("{program} {}", args.join(" "));
        self.ran.borrow_mut().push(line.clone());
        self.answers
            .get(&line)
            .cloned()
            .unwrap_or_else(|| Err(format!("{program}: not found")))
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

/// Files and their text. A file not listed does not exist.
#[derive(Clone, Default)]
pub struct FakeFiles {
    files: BTreeMap<String, String>,
    links: BTreeMap<String, String>,
    /// Paths under a prefix share a device.
    devices: Vec<(PathBuf, u64)>,
    /// Paths under a prefix cannot be looked at.
    unlooked: Vec<PathBuf>,
}

impl FakeFiles {
    pub fn with(mut self, path: &str, text: &str) -> Self {
        self.files.insert(path.to_owned(), text.to_owned());
        self
    }

    /// The file at `path` does not exist.
    pub fn without(mut self, path: &str) -> Self {
        self.files.remove(path);
        self
    }

    /// The link at `path` points at `target`.
    pub fn with_link(mut self, path: &str, target: &str) -> Self {
        self.links.insert(path.to_owned(), target.to_owned());
        self
    }

    /// The link at `path` cannot be read.
    pub fn without_link(mut self, path: &str) -> Self {
        self.links.remove(path);
        self
    }

    /// Everything under `prefix` is on the device `device`; any other path is on a device
    /// of its own.
    pub fn with_device_under(mut self, prefix: &Path, device: u64) -> Self {
        self.devices.push((prefix.to_path_buf(), device));
        self
    }

    /// Nothing under `prefix` can be looked at.
    pub fn unlooked_under(mut self, prefix: &Path) -> Self {
        self.unlooked.push(prefix.to_path_buf());
        self
    }
}

impl FileReader for FakeFiles {
    fn read(&self, path: &str) -> Result<String, String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| format!("{path}: No such file or directory"))
    }

    fn link(&self, path: &str) -> Result<String, String> {
        self.links
            .get(path)
            .cloned()
            .ok_or_else(|| format!("{path}: No such file or directory"))
    }

    fn device(&self, path: &Path) -> Result<u64, String> {
        if self.unlooked.iter().any(|prefix| path.starts_with(prefix)) {
            return Err(format!("{}: Permission denied", path.display()));
        }
        let shared = self
            .devices
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix));
        Ok(shared.map_or_else(
            || {
                let mut hasher = std::hash::DefaultHasher::new();
                std::hash::Hash::hash(path, &mut hasher);
                std::hash::Hasher::finish(&hasher) | (1 << 63)
            },
            |(_, device)| *device,
        ))
    }
}

/// A machine as the probes see it.
#[derive(Clone)]
pub struct Host {
    pub platform: Platform,
    pub commands: FakeCommands,
    pub files: FakeFiles,
}

pub fn platform(os: &str, arch: &str) -> Platform {
    Platform {
        os: os.to_owned(),
        arch: arch.to_owned(),
    }
}

/// A Linux host with 8 CPUs, 32 GiB and two A100s.
pub fn linux_host() -> Host {
    Host {
        platform: platform("linux", "x86_64"),
        commands: FakeCommands::default()
            .with(CPU_CMD, Ok("8\n"))
            .with(NVIDIA_CMD, Ok(NVIDIA_TWO_A100)),
        files: FakeFiles::default()
            .with("/proc/meminfo", MEMINFO)
            .with("/etc/machine-id", &format!("{MACHINE_ID}\n"))
            .with("/proc/2/stat", "2 (kthreadd) S 0 0 0 0")
            .with("/proc/1/cgroup", "0::/init.scope\n")
            .with_link("/proc/self/ns/pid", "pid:[4026531836]")
            .with_link("/proc/1/ns/pid", "pid:[4026531836]")
            .with_link("/proc/self/ns/mnt", "mnt:[4026531841]")
            .with_link("/proc/1/ns/mnt", "mnt:[4026531841]"),
    }
}

/// An Apple silicon Mac with 10 CPUs, 16 GiB and one Apple GPU.
pub fn mac_host() -> Host {
    Host {
        platform: platform("macos", "aarch64"),
        commands: FakeCommands::default()
            .with(CPU_CMD, Ok("10\n"))
            .with(MEMSIZE_CMD, Ok("17179869184\n"))
            .with(PROFILER_CMD, Ok(APPLE_M2))
            .with(IOREG_CMD, Ok(&ioreg())),
        files: FakeFiles::default(),
    }
}

/// The answer a transport gives.
pub type Reply = Result<Answer, String>;

/// An answer with `status` and a JSON `body`.
pub fn reply(status: u16, body: Value) -> Reply {
    Ok(Answer {
        status,
        body,
        retry_after: None,
    })
}

/// An answer that asks for a wait.
pub fn limited(status: u16, seconds: u64) -> Reply {
    Ok(Answer {
        status,
        body: json!({ "error": "rate-limited" }),
        retry_after: Some(Duration::from_secs(seconds)),
    })
}

/// A refusal: `{"error": code}`.
pub fn refused(status: u16, code: &str) -> Reply {
    reply(status, json!({ "error": code }))
}

/// A welcome for `epoch` with the test secret.
pub fn welcome(epoch: u64) -> Reply {
    welcome_with(epoch, 5_000, 15_000)
}

pub fn welcome_with(epoch: u64, interval_ms: u64, grace_ms: u64) -> Reply {
    welcome_speaking(epoch, interval_ms, grace_ms, 1)
}

/// A welcome that names the protocol major `protocol`.
pub fn welcome_speaking(epoch: u64, interval_ms: u64, grace_ms: u64, protocol: u64) -> Reply {
    reply(
        200,
        json!({
            "protocol": protocol,
            "epoch": epoch,
            "heartbeat_interval_ms": interval_ms,
            "grace_ms": grace_ms,
            "pool": POOL,
            "session": SECRET,
        }),
    )
}

/// An accepted heartbeat with nothing to say.
pub fn beat_ok() -> Reply {
    reply(
        200,
        json!({ "commands": [], "result_acks": [], "taint_ack": false }),
    )
}

/// What a transport was sent: path and body, in order.
pub type Sent = Rc<RefCell<Vec<(String, Value)>>>;

/// A transport that answers from a list and records what it was sent. When the list is
/// used up every post fails as if the server were down.
pub struct Script {
    replies: VecDeque<Reply>,
    sent: Sent,
}

impl Script {
    pub fn new(replies: Vec<Reply>) -> (Self, Sent) {
        let sent = Sent::default();
        let script = Self {
            replies: replies.into(),
            sent: sent.clone(),
        };
        (script, sent)
    }
}

impl Transport for Script {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        self.sent.borrow_mut().push((path.to_owned(), body.clone()));
        self.replies
            .pop_front()
            .unwrap_or_else(|| Err("the script is used up".to_owned()))
    }
}

/// Hands out one transport.
pub struct ScriptConnector(pub RefCell<Option<Script>>);

impl ScriptConnector {
    pub fn of(script: Script) -> Self {
        Self(RefCell::new(Some(script)))
    }
}

impl Connector for ScriptConnector {
    fn connect(
        &self,
        _server: &str,
        _token_file: &Path,
        _ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let script = self.0.borrow_mut().take().expect("connected once");
        Ok(Box::new(script))
    }
}

/// Counts the waits and stops the loop at the `stop_at`th (1 is the first).
pub struct CountingSleeper {
    pub waits: Rc<RefCell<Vec<Duration>>>,
    stop_at: usize,
}

impl CountingSleeper {
    pub fn stopping_at(stop_at: usize) -> Self {
        Self {
            waits: Rc::default(),
            stop_at,
        }
    }
}

impl Sleeper for CountingSleeper {
    fn sleep(&mut self, duration: Duration) -> Wake {
        let mut waits = self.waits.borrow_mut();
        waits.push(duration);
        match waits.len() >= self.stop_at {
            true => Wake::Stop,
            false => Wake::Elapsed,
        }
    }
}

/// The longest jitter every time.
pub struct Highest;

impl Jitter for Highest {
    fn up_to(&mut self, max: u64) -> u64 {
        max
    }
}

/// No jitter.
pub struct Lowest;

impl Jitter for Lowest {
    fn up_to(&mut self, _max: u64) -> u64 {
        0
    }
}

/// A jitter that lives as long as the test needs it (it holds nothing, so leaking it
/// costs nothing).
pub fn lowest() -> &'static mut Lowest {
    Box::leak(Box::new(Lowest))
}

/// A valid hello for `host` in the test pool.
pub fn hello(host: &str) -> Hello {
    Hello {
        protocol: ProtocolRange::THIS,
        host_id: host.to_owned(),
        pool: POOL.to_owned(),
        agent_version: "0.1.0".to_owned(),
        instance: None,
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 1 << 34,
            gpus: 0,
            gpu_class: None,
            max_leases: None,
        },
        capabilities: BTreeMap::new(),
        gpu_devices: Vec::new(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
    }
}

/// What a run of the program printed and how it ended.
pub struct Ran {
    pub status: ExitCode,
    pub out: String,
    pub err: String,
}

/// The `Debug` text of an exit status, which `ExitCode` offers as the only way to read it.
pub fn code(status: u8) -> String {
    format!("{:?}", ExitCode::from(status))
}

impl Ran {
    pub fn status(&self) -> String {
        format!("{:?}", self.status)
    }
}

/// Runs the program on `args` against `host`.
pub fn run(
    args: &[&str],
    host: &Host,
    connector: &dyn Connector,
    sleeper: &mut dyn Sleeper,
    jitter: &mut dyn Jitter,
) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut services = Services {
        platform: host.platform.clone(),
        commands: &host.commands,
        files: &host.files,
        connector,
        jitter,
        sleeper,
    };
    let argv = std::iter::once("js-agent").chain(args.iter().copied());
    let argv = argv.map(Into::into).collect();
    let status = run_with(argv, &mut out, &mut err, &mut services);
    Ran {
        status,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

/// The flags every run needs: a loopback server and a pool. The token file is a path the
/// scripted connector never reads.
pub fn flags<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec![
        "--server",
        "http://127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
        "--pool",
        POOL,
    ];
    all.extend_from_slice(extra);
    all
}

/// Runs the program with the scripted transport until its `stop_at`th wait; returns what
/// it printed and what the transport was sent.
pub fn run_scripted(
    extra: &[&str],
    host: &Host,
    replies: Vec<Reply>,
    stop_at: usize,
) -> (Ran, Sent, Vec<Duration>) {
    let (script, sent) = Script::new(replies);
    let mut sleeper = CountingSleeper::stopping_at(stop_at);
    let waits = sleeper.waits.clone();
    let ran = run(
        &flags(extra),
        host,
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    let waited = waits.borrow().clone();
    (ran, sent, waited)
}
