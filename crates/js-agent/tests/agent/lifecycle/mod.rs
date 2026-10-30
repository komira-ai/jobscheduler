//! The lifecycle of a job: every rule of the agent's side of a start, a stop and a
//! cleanup, driven in memory. The runtime is a fake that exits, fails and records when a
//! test says so, the clock moves only when a test moves it, and the scratch directories and
//! mount roots are real directories under the target directory. Nothing sleeps, nothing
//! waits, no process starts, so there is no hang to guard.

mod adopt;
mod cleanup;
mod exclusive;
pub mod fake;
mod files;
mod flags;
mod logs;
mod mounts;
mod owed;
mod reach;
mod results;
mod rules;
mod running;
mod scratch;
mod slots;
mod taint;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use js_agent::lifecycle::{
    Capacity, CheckFinding, CheckHook, Config, Event, FilesRoot, Lifecycle, MARKER_FILE,
    MountRoots, RuntimeError, ScratchRoot, StateRoot,
};
use js_agent_wire::{Command, HeartbeatReply, JobSpec, Limits};
use serde_json::{Value, json};

use crate::support::Scratch;
use fake::{FakeRuntime, Log, ManualClock, ScriptedChecks};

/// How long a stop gives a container in these tests.
pub const GRACE: Duration = Duration::from_secs(10);

/// A lifecycle over a fake runtime, with the pieces a test reaches into.
pub struct Harness {
    pub life: Lifecycle,
    pub fake: FakeRuntime,
    pub clock: ManualClock,
    pub log: Log,
    pub config: Config,
    /// Holds the scratch root and the mount root; removed when the test ends.
    pub dir: Scratch,
}

/// A harness's options.
#[derive(Default)]
pub struct Options {
    pub allow_host_jobs: bool,
    /// Runs whose post-job check finds something.
    pub findings: Vec<(&'static str, CheckFinding)>,
    /// The fake runtime and the checks note in the log whether the marker exists when they
    /// are called.
    pub watch_marker: bool,
    /// Checks to use instead of the scripted ones.
    pub hook: Option<Box<dyn CheckHook>>,
}

impl Harness {
    /// Four CPUs, 8 GiB and GPUs 0 and 1, no host jobs and checks that pass.
    pub fn new(name: &str) -> Self {
        Self::with(name, Options::default())
    }

    pub fn with(name: &str, options: Options) -> Self {
        let dir = Scratch::new(&format!("life-{name}"));
        let scratch = dir.path("scratch");
        let config = config_in(&dir, options.allow_host_jobs);
        let (clock, log) = (ManualClock::default(), Log::default());
        let fake = FakeRuntime::new(&log, &clock);
        if options.watch_marker {
            fake.watch_marker(dir.path("state").join(MARKER_FILE));
        }
        let checks = options
            .hook
            .unwrap_or_else(|| hook(&log, &fake, &scratch, &options.findings));
        let (life, _) = Lifecycle::adopt(
            config.clone(),
            Box::new(fake.clone()),
            checks,
            Box::new(clock.clone()),
        )
        .unwrap();
        Self {
            life,
            fake,
            clock,
            log,
            config,
            dir,
        }
    }

    /// Replaces the lifecycle with a fresh one over the same runtime, clock and scratch
    /// root, as the process of an agent that has just started: it adopts what the runtime
    /// lists. Its post-job checks are the scripted ones, which pass.
    pub fn restart(&mut self) -> Result<Vec<Event>, RuntimeError> {
        let scratch = self.dir.path("scratch");
        let (life, events) = Lifecycle::adopt(
            self.config.clone(),
            Box::new(self.fake.clone()),
            hook(&self.log, &self.fake, &scratch, &[]),
            Box::new(self.clock.clone()),
        )?;
        self.life = life;
        Ok(events)
    }

    /// The taint marker file, which may not exist.
    pub fn marker(&self) -> PathBuf {
        self.dir.path("state").join(MARKER_FILE)
    }

    /// The scratch directory of a run, by the canonical path the scratch root keeps.
    pub fn scratch_of(&self, run_id: &str) -> PathBuf {
        let root = fs::canonicalize(self.dir.path("scratch")).unwrap();
        root.join(run_id)
    }

    /// Applies a reply of the commands.
    pub fn apply(&mut self, commands: Vec<Command>) -> Vec<Event> {
        self.life.apply(&reply(commands))
    }

    /// Applies a reply that starts `spec`.
    pub fn start(&mut self, spec: JobSpec) -> Vec<Event> {
        self.apply(vec![Command::Start(Box::new(spec))])
    }

    /// Applies a reply that stops `run_id`.
    pub fn stop(&mut self, run_id: &str) -> Vec<Event> {
        self.apply(vec![Command::Stop {
            run_id: run_id.to_owned(),
        }])
    }

    /// Applies a reply that cancels `run_id`.
    pub fn cancel(&mut self, run_id: &str) -> Vec<Event> {
        self.apply(vec![Command::Cancel {
            run_id: run_id.to_owned(),
        }])
    }

    /// Applies a reply that acknowledges `run_id`'s result with `ack`.
    pub fn ack(&mut self, run_id: &str, ack: &str) -> Vec<Event> {
        let reply = reply_json(&json!({
            "result_acks": [{ "run_id": run_id, "ack": ack }]
        }));
        self.life.apply(&reply)
    }

    /// The run ids held, in order.
    pub fn held(&self) -> Vec<String> {
        self.life.held().into_iter().map(|run| run.run_id).collect()
    }
}

/// The lifecycle's configuration under `dir`: four CPUs, 8 GiB and GPUs 0 and 1, a
/// private scratch root and one mount root `data`.
pub fn config_in(dir: &Scratch, allow_host_jobs: bool) -> Config {
    let scratch = dir.path("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(dir.path("data")).unwrap();
    let state = dir.path("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            gpus: BTreeSet::from([0, 1]),
        },
        allow_host_jobs,
        mounts: MountRoots::new(&[("data".to_owned(), dir.path("data"))]).unwrap(),
        scratch: ScratchRoot::open(&scratch).unwrap(),
        files: Some(FilesRoot::open(&dir.path("files")).unwrap()),
        state: StateRoot::open(&state).unwrap(),
        stop_grace: GRACE,
    }
}

/// Checks that pass, except for the runs of `findings`, and note when they ran.
fn hook(
    log: &Log,
    fake: &FakeRuntime,
    scratch_root: &std::path::Path,
    findings: &[(&'static str, CheckFinding)],
) -> Box<dyn CheckHook> {
    let checks = ScriptedChecks::new(log, fake, scratch_root);
    Box::new(
        findings
            .iter()
            .fold(checks, |c, (run, found)| c.find(run, found.clone())),
    )
}

/// A valid start for `run_id`: one CPU, 1 GiB, a minute, no GPU, no mount.
pub fn job(run_id: &str) -> JobSpec {
    JobSpec {
        run_id: run_id.to_owned(),
        lease_id: format!("lease-{run_id}"),
        image: "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        args: vec!["--go".to_owned()],
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            pids: Some(100),
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: js_agent_wire::Isolation::Contained,
        labels: std::collections::BTreeMap::new(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
    }
}

/// A reply of these commands.
pub fn reply(commands: Vec<Command>) -> HeartbeatReply {
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

/// A reply as the server's JSON says it.
pub fn reply_json(body: &Value) -> HeartbeatReply {
    serde_json::from_value(body.clone()).unwrap()
}

/// Events as short texts, so a test reads an order at a glance.
pub fn names(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Event::Adopted { run_id } => format!("adopted {run_id}"),
            Event::Started { run_id } => format!("started {run_id}"),
            Event::Refused { run_id, code, .. } => format!("refused {run_id} {code}"),
            Event::Skipped { run_id, why } => format!("skipped {run_id} {why:?}"),
            Event::Invalid { index, code } => format!("invalid #{index} {code}"),
            Event::ResultReady { run_id } => format!("result {run_id}"),
            Event::ContainerRemoved { run_id } => format!("container removed {run_id}"),
            Event::ScratchRemoved { run_id } => format!("scratch removed {run_id}"),
            Event::ChecksPassed { run_id } => format!("checks passed {run_id}"),
            Event::Tainted(taint) => format!(
                "tainted {} {}",
                taint.run_id.as_deref().unwrap_or("-"),
                taint.reason
            ),
            Event::SlotFreed { run_id } => format!("freed {run_id}"),
            Event::Finding(finding) => format!(
                "finding {} {}",
                finding.run_id.as_deref().unwrap_or("-"),
                finding.code
            ),
        })
        .collect()
}

/// The events of a run's cleanup, in order, as `names` gives them.
pub fn cleanup(run_id: &str) -> Vec<String> {
    [
        "container removed",
        "scratch removed",
        "checks passed",
        "freed",
    ]
    .map(|step| format!("{step} {run_id}"))
    .to_vec()
}
