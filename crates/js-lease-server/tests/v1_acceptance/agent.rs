//! The agent: `js_agent::run_with`, the program's own entry after its signal handlers, on a
//! thread of the test, with its real transport (`Dialer`) to the server's lease port and the
//! real `CrunRuntime` over the node. What a test that is not root cannot give it is replaced:
//! the probes' programs and files, the view of the host the post-job checks read, the cgroup
//! files and the scratch quota. Its waits are a gate the test opens, so a heartbeat happens
//! when the test lets one happen and the test counts them.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use js_agent::bundle::SystemHost;
use js_agent::crun_host::CrunHost;
use js_agent::integrity::{FakeHost, HostView};
use js_agent::lifecycle::Runtime;
use js_agent::quota::{FakeQuota, ScratchQuota};
use js_agent::{
    CommandRunner, Dialer, FileReader, Handle, Jitter, Machine, Platform, RuntimeBuild, Services,
    SharedClock, Sleeper, Snapshot, System, Wake, run_with,
};
use js_agent_wire::HeldState;

use crate::node::{JOB_USER, Node};

/// How long a side waits for the other before the test fails: a guard against a hang, never
/// an expectation. A healthy agent answers within milliseconds, and the cases assert on what
/// was said and done, not on how soon.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// The agent's waits, each held until the test opens it.
struct Gate {
    slept: Sender<()>,
    go: Receiver<Wake>,
}

impl Sleeper for Gate {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        let _ = self.slept.send(());
        match self.go.recv_timeout(HANG_GUARD) {
            Ok(wake) => wake,
            Err(RecvTimeoutError::Disconnected) => Wake::Stop,
            Err(RecvTimeoutError::Timeout) => panic!("the test never opened the agent's wait"),
        }
    }
}

/// No jitter: every wait is the lowest the backoff allows.
struct Lowest;

impl Jitter for Lowest {
    fn up_to(&mut self, _max: u64) -> u64 {
        0
    }
}

/// The probes' programs: the CPU count and `df` of the scratch root. Anything else is not
/// found, which the probes take as a host without it (no GPU).
struct Commands {
    answers: BTreeMap<String, String>,
}

impl CommandRunner for Commands {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let line = format!("{program} {}", args.join(" "));
        self.answers
            .get(&line)
            .cloned()
            .ok_or_else(|| format!("{program}: not found"))
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

/// The probes' files: a Linux host with 8 CPUs and 32 GiB whose `/proc` is the host's own.
/// The state root is on a disk of its own and everything else on the node's.
#[derive(Clone)]
struct Files {
    files: BTreeMap<String, String>,
    links: BTreeMap<String, String>,
    state: PathBuf,
}

impl Files {
    fn new(state: &Path) -> Self {
        let files = [
            (
                "/proc/meminfo",
                "MemTotal:       33554432 kB\nMemFree:         1048576 kB\nMemAvailable:   20971520 kB\n",
            ),
            ("/etc/machine-id", "3f0a6c1d9e8b4a7c92d1e5f60718293a\n"),
            ("/proc/2/stat", "2 (kthreadd) S 0 0 0 0"),
            ("/proc/1/cgroup", "0::/init.scope\n"),
        ];
        let links = [
            ("/proc/self/ns/pid", "pid:[4026531836]"),
            ("/proc/1/ns/pid", "pid:[4026531836]"),
            ("/proc/self/ns/mnt", "mnt:[4026531841]"),
            ("/proc/1/ns/mnt", "mnt:[4026531841]"),
        ];
        let owned = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        Self {
            files: owned(&files),
            links: owned(&links),
            state: state.to_owned(),
        }
    }
}

impl FileReader for Files {
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
        Ok(if path.starts_with(&self.state) { 8 } else { 7 })
    }

    fn is_tmpfs(&self, _path: &Path) -> Result<bool, String> {
        Ok(false)
    }
}

/// The real [`System`] for the clock, the runtime and the start check, over the node; the
/// view of the host is a quiet one, the cgroup files are the probes' and the quota lives in
/// memory, as a test that is not root cannot read the namespaces of process 1.
struct NodeMachine {
    system: System,
    files: Files,
    quota: Arc<FakeQuota>,
}

impl Machine for NodeMachine {
    fn clock(&self) -> SharedClock {
        self.system.clock()
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        self.system.runtime(build)
    }

    fn view(&self) -> Box<dyn HostView> {
        Box::new(FakeHost::quiet())
    }

    fn cgroup_files(&self) -> Box<dyn FileReader> {
        Box::new(self.files.clone())
    }

    fn scratch_quota(&self, _root: &Path) -> Arc<dyn ScratchQuota> {
        self.quota.clone()
    }

    fn crun_host(&self) -> CrunHost {
        self.system.crun_host()
    }
}

/// What the agent printed, and how it ended.
pub struct Ran {
    /// The exit status, as `ExitCode`'s `Debug` text.
    pub status: String,
    /// Its standard error.
    pub err: String,
}

/// An agent running on a thread, held at each of its waits until the test lets it go.
pub struct Agent {
    slept: Receiver<()>,
    go: Sender<Wake>,
    handle: Option<JoinHandle<Ran>>,
    worker: Handle,
}

impl Agent {
    /// Starts the agent of `node` with the join token in `token`, against the lease port
    /// `server`, as the host `host_id`, with `extra` flags after its own; returns once it has
    /// said hello and waits for its first heartbeat.
    pub fn start(node: &Node, server: &str, token: &Path, host_id: &str, extra: &[&str]) -> Self {
        Self::spawn(node, Self::args(node, server, token, host_id, extra, true)).unwrap_or_else(
            |ran| {
                panic!(
                    "the agent ended before its worker started ({}):\n{}",
                    ran.status, ran.err
                )
            },
        )
    }

    /// [`Agent::start`] with no `--runtime`: the default runtime of a Linux agent, `crun`.
    pub fn start_defaulted(node: &Node, server: &str, token: &Path, host_id: &str) -> Self {
        Self::spawn(node, Self::args(node, server, token, host_id, &[], false)).unwrap_or_else(
            |ran| {
                panic!(
                    "the agent ended before its worker started ({}):\n{}",
                    ran.status, ran.err
                )
            },
        )
    }

    /// An agent that is expected to end before its worker starts: how it ended.
    pub fn refused(node: &Node, server: &str, token: &Path, host_id: &str, extra: &[&str]) -> Ran {
        match Self::spawn(node, Self::args(node, server, token, host_id, extra, true)) {
            Ok(agent) => panic!("the agent started: {}", agent.stop().err),
            Err(ran) => ran,
        }
    }

    /// The command line of an agent of `node`, with `--runtime crun` when `crun`.
    fn args(
        node: &Node,
        server: &str,
        token: &Path,
        host_id: &str,
        extra: &[&str],
        crun: bool,
    ) -> Vec<OsString> {
        let mut words = vec![
            "js-agent",
            "--server",
            server,
            "--pool",
            crate::server::POOL_NAME,
            "--host-id",
            host_id,
        ];
        if crun {
            words.extend(["--runtime", "crun"]);
        }
        words.extend(["--job-user", JOB_USER]);
        words
            .iter()
            .map(OsString::from)
            .chain(
                [
                    ("--token-file", token),
                    ("--crun-bin", node.crun.as_path()),
                    ("--state-root", node.state.as_path()),
                    ("--scratch-root", node.scratch.as_path()),
                ]
                .into_iter()
                .flat_map(|(flag, path)| [OsString::from(flag), path.as_os_str().to_owned()]),
            )
            .chain(extra.iter().map(OsString::from))
            .collect()
    }

    /// Runs the agent on `args` on a thread; returns it once it has said hello and waits for
    /// its first heartbeat, or how it ended when it ended before its worker started.
    fn spawn(node: &Node, args: Vec<OsString>) -> Result<Self, Ran> {
        let df = format!(
            "/bin/df -P -k {}",
            std::fs::canonicalize(&node.scratch)
                .expect("the scratch root")
                .display()
        );
        let commands = Commands {
            answers: [
                (
                    "/usr/bin/getconf _NPROCESSORS_ONLN".to_owned(),
                    "8\n".to_owned(),
                ),
                (
                    format!("{} --version", node.crun.display()),
                    "crun version 1.14.1\ncommit: abc\n".to_owned(),
                ),
                (
                    df,
                    "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                     /dev/sda1 99999999 100 10485760 11% /\n"
                        .to_owned(),
                ),
            ]
            .into(),
        };
        let files = Files::new(&node.state);
        let machine = Arc::new(NodeMachine {
            system: System::new().with_crun(node.seams()),
            files: files.clone(),
            quota: Arc::new(FakeQuota::new()),
        });
        let floor = node.path().to_owned();
        let (tell, worker) = channel();
        let (slept, waits) = channel();
        let (go, gate) = channel();
        let tell = Mutex::new(tell);
        let handle = std::thread::spawn(move || {
            let mut sleeper = Gate { slept, go: gate };
            let hook = move |worker: Handle| {
                let _ = tell.lock().map(|tell| tell.send(worker));
            };
            let euid = rustix::process::geteuid().as_raw();
            let mut services = Services {
                platform: Platform {
                    os: "linux".to_owned(),
                    arch: std::env::consts::ARCH.to_owned(),
                },
                commands: &commands,
                files: &files,
                connector: &Dialer,
                jitter: &mut Lowest,
                sleeper: &mut sleeper,
                machine,
                bundle: Arc::new(SystemHost::system()),
                long_bundle_values: false,
                worker_hook: Some(&hook),
                rescue_bound: None,
                euid,
                ancestors_floor: &floor,
                secret_owner: euid,
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let status = run_with(args, &mut out, &mut err, &mut services);
            Ran {
                status: format!("{status:?}"),
                err: String::from_utf8_lossy(&err).into_owned(),
            }
        });
        let Ok(worker) = worker.recv_timeout(HANG_GUARD) else {
            return Err(handle.join().expect("the agent's thread"));
        };
        let agent = Self {
            slept: waits,
            go,
            handle: Some(handle),
            worker,
        };
        agent.waited();
        Ok(agent)
    }

    /// Waits until the agent has done its next exchange and waits again.
    fn waited(&self) {
        if self.slept.recv_timeout(HANG_GUARD).is_err() {
            panic!("the agent ended, or said nothing for a long time, before it waited");
        }
    }

    /// One heartbeat of the agent, and the worker done with the reply to it.
    pub fn beat(&self) {
        self.go.send(Wake::Elapsed).expect("the agent waits");
        self.waited();
        assert!(self.worker.settle(), "the agent's worker ended");
    }

    /// Whether the agent holds the job `id`: a start of it reached the agent.
    pub fn holds(&self, id: &str) -> bool {
        self.worker
            .snapshot()
            .jobs
            .iter()
            .any(|job| job.job_id == id)
    }

    /// Waits until the agent no longer holds the job `id` running: its container ended and
    /// the worker saw it end, which the worker publishes (it looks at its containers at each
    /// order and once a second by itself). Returns at once when the job is not running there.
    /// A job that never ends fails the case at the hang guard.
    pub fn until_ended(&self, id: &str) {
        let running = |snapshot: &Snapshot| {
            snapshot
                .jobs
                .iter()
                .any(|job| job.job_id == id && job.state == HeldState::Running)
        };
        let ended = self
            .worker
            .wait_until(&|snapshot| !running(snapshot), HANG_GUARD);
        assert!(ended, "the agent never saw {id} end");
    }

    /// Whether the agent holds no job: every job it began was reported, acknowledged and
    /// cleaned up, so its next heartbeat lists none.
    pub fn holds_no_job(&self) -> bool {
        self.worker.snapshot().jobs.is_empty()
    }

    /// Delivers a stop request, as SIGTERM would, at the agent's current wait, and waits
    /// until it waits again (it drains: it goes on beating until its jobs are reported).
    pub fn signal(&self) {
        self.go.send(Wake::Stop).expect("the agent waits");
        self.waited();
    }

    /// Lets every wait of an agent that is ending pass, until it ends; returns how it ended.
    pub fn finish(mut self) -> Ran {
        self.end_with(Wake::Elapsed)
    }

    /// Asks the agent to stop, as SIGTERM would, and returns how it ended.
    pub fn stop(mut self) -> Ran {
        self.end_with(Wake::Stop)
    }

    /// Answers every wait with `wake` until the agent's thread ends.
    fn end_with(&mut self, wake: Wake) -> Ran {
        let _ = self.go.send(wake);
        // Every later wait is answered the same way, until the agent's thread ends and drops
        // its side of the gate.
        let handle = self.handle.take().expect("the agent's thread");
        loop {
            match self.slept.recv_timeout(HANG_GUARD) {
                Ok(()) => {
                    let _ = self.go.send(wake);
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("the agent never ended after a stop"),
            }
        }
        handle.join().expect("the agent's thread")
    }
}
