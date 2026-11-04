//! The runtime's egress port, faked: when `CrunRuntime` checks the firewall, wires a
//! container and releases it, around which step of a start, and what a failure at each step
//! leaves (nothing). The stand-in's `crun state` names the pid [`PID`]; the port records what
//! it was asked, in order. Whether the wiring works on a real kernel is the `egress`
//! target's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::crun::{CrunRuntime, EgressPort, Hooks, Wired, start_egress};
use js_agent::egress::{Config as EgressConfig, EgressError, Flags};
use js_agent::lifecycle::{
    Capacity, Clock, Config, DirLayout, Event, Lifecycle, MountRoots, NoChecks, Prepared, Runtime,
    ScratchRoot, StateRoot, Status,
};
use js_agent_wire::{Command, HeartbeatReply, Isolation, JobSpec, Limits};

use crate::node::{LONG, NoImages, Node, PID};

/// What the port was asked, and how it answers.
#[derive(Debug, Default)]
struct Seen {
    /// `check <sidecars>` (the leases the ledger held when the check was asked),
    /// `attach <run> <pid>` and `release <run>`, in order.
    calls: Mutex<Vec<String>>,
    /// The directory of the runtime's sidecars.
    ledger: PathBuf,
    check_fails: AtomicBool,
    attach_fails: AtomicBool,
    release_fails: AtomicBool,
}

impl Seen {
    fn new(node: &Node) -> Arc<Self> {
        Arc::new(Self {
            ledger: node.scratch.join("oci-labels"),
            ..Self::default()
        })
    }

    fn note(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
    }

    /// The calls so far, and empties the record.
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }
}

/// A port that records in `seen`.
fn port(seen: &Arc<Seen>) -> Option<Arc<dyn EgressPort>> {
    Some(Arc::new(Port(Arc::clone(seen))))
}

/// The fake port.
#[derive(Debug)]
struct Port(Arc<Seen>);

impl EgressPort for Port {
    fn check(&self) -> Result<(), EgressError> {
        let sidecars = std::fs::read_dir(&self.0.ledger).map_or(0, Iterator::count);
        self.0.note(format!("check {sidecars}"));
        match self.0.check_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Rules),
            false => Ok(()),
        }
    }

    fn attach(&self, run: &str, pid: u32) -> Result<Box<dyn Wired>, EgressError> {
        self.0.note(format!("attach {run} {pid}"));
        match self.0.attach_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Exhausted),
            false => Ok(Box::new(Wire {
                seen: Arc::clone(&self.0),
                run: run.to_owned(),
            })),
        }
    }
}

/// One run's fake wiring.
struct Wire {
    seen: Arc<Seen>,
    run: String,
}

impl Wired for Wire {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        self.seen.note(format!("release {}", self.run));
        match self.seen.release_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Command {
                what: "deleting the veth".to_owned(),
                why: "Operation not permitted".to_owned(),
            }),
            false => Ok(()),
        }
    }
}

/// A runtime on `node` with the port `egress` and `hooks`.
fn open(node: &Node, egress: Option<Arc<dyn EgressPort>>, hooks: Hooks) -> CrunRuntime {
    let mut settings = node.settings("/root");
    settings.egress = egress;
    CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens")
}

/// Hooks whose wait after a kill is spent at once: a removal ends a container that ignores
/// SIGTERM by force.
fn impatient() -> Hooks {
    Node::hooks(Duration::ZERO, LONG)
}

/// Whether nothing of the lease of the run `run_id` is left on `node`: no sidecar, no lease
/// directory and no lease cgroup (each is named for the lease, `js-<tag>-<run id>`).
fn nothing_left(node: &Node, run_id: &str) -> bool {
    let suffix = format!("-{run_id}");
    let held = |dir: &Path| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.trim_end_matches(".json").ends_with(&suffix)
            })
        })
    };
    let dirs = [
        node.scratch.join("oci-labels"),
        node.scratch.clone(),
        node.cgroups.join("actions"),
    ];
    !dirs.iter().any(|dir| held(dir))
}

/// How often the stand-in's `crun state` was asked about the run `run_id`.
fn asks(node: &Node, run_id: &str) -> usize {
    let suffix = format!("-{run_id}");
    let entries = std::fs::read_dir(node.dir.path()).expect("the node");
    let file = entries.flatten().map(|entry| entry.path()).find(|path| {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        name.is_some_and(|name| name.starts_with("asks-") && name.ends_with(&suffix))
    });
    file.map_or(0, |file| {
        std::fs::read_to_string(file)
            .expect("the asks")
            .lines()
            .count()
    })
}

/// The record of an attach of the run `run_id` to the stand-in's pid.
fn attached(call: &str, run_id: &str) -> bool {
    call.starts_with("attach js-") && call.ends_with(&format!("-{run_id} {PID}"))
}

/// A clock that reads a minute later at every read, so any wait on it is over at once.
fn ticking() -> u64 {
    static NOW: AtomicU64 = AtomicU64::new(0);
    NOW.fetch_add(60_000, Ordering::SeqCst)
}

/// A lifecycle clock that does not move.
struct Still;

impl Clock for Still {
    fn now_ms(&self) -> u64 {
        5_000
    }
}

/// A lifecycle over `runtime`, with its scratch and state roots on `node`.
fn lifecycle(node: &Node, runtime: CrunRuntime) -> Lifecycle {
    use std::os::unix::fs::PermissionsExt as _;
    let private = |name: &str| {
        let dir = node.dir.join(name);
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
        dir
    };
    let config = Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: Default::default(),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[]).expect("no mount roots"),
        scratch: ScratchRoot::open_with(&private("life-scratch"), DirLayout::Private)
            .expect("a scratch root"),
        files: None,
        state: StateRoot::open(&private("life-state")).expect("a state root"),
        stop_grace: LONG,
        bundle: None,
        quota: None,
    };
    let clock = Box::new(Still);
    let adopted = Lifecycle::adopt(config, Box::new(runtime), Box::new(NoChecks), clock);
    adopted.expect("a lifecycle over an empty runtime").0
}

/// Asks `life` to start the run `run_id` of the node's image; the events as short texts.
fn start(life: &mut Lifecycle, node: &Node, run_id: &str) -> Vec<String> {
    let spec = JobSpec {
        run_id: run_id.to_owned(),
        lease_id: format!("lease-{run_id}"),
        image: node.image(),
        args: vec!["--go".to_owned()],
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            pids: Some(256),
            wall_ms: 600_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
    };
    let reply = HeartbeatReply {
        commands: vec![Command::Start(Box::new(spec))],
        ..HeartbeatReply::default()
    };
    life.apply(&reply)
        .iter()
        .map(|event| match event {
            Event::Started { run_id } => format!("started {run_id}"),
            Event::Refused { run_id, code, .. } => format!("refused {run_id} {code}"),
            Event::ResultReady { run_id } => format!("result {run_id}"),
            other => format!("{other:?}"),
        })
        .collect()
}

/// Proves the firewall is checked through the port before anything of a run is made, and
/// that a failing check makes nothing. The runtime's own check answers what the port says.
/// Under the lifecycle, a start is checked when the ledger holds no sidecar yet (the sidecar
/// is the first thing a create makes) and then wired; a start whose check fails is refused
/// `egress-unchecked`, the port is asked nothing more, and the run has no sidecar, no lease
/// directory and no cgroup. Catches a `CrunRuntime` without the override (the default check
/// passes, and the refused run starts) and a check made after the create.
#[test]
fn crun_checks_egress_before_it_creates_the_container() {
    let node = Node::new("egress-check-first");
    let seen = Seen::new(&node);
    let runtime = open(&node, port(&seen), impatient());
    runtime.check_egress().expect("the firewall is as loaded");
    seen.check_fails.store(true, Ordering::SeqCst);
    let why = runtime.check_egress().expect_err("the firewall changed");
    assert_eq!(why.to_string(), EgressError::Rules.to_string());
    assert_eq!(seen.taken(), ["check 0", "check 0"]);
    seen.check_fails.store(false, Ordering::SeqCst);

    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    let mut life = lifecycle(&node, runtime);
    assert_eq!(start(&mut life, &node, "r1-wired"), ["started r1-wired"]);
    let calls = seen.taken();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[0], "check 0",
        "checked before the run's sidecar was made"
    );
    assert!(attached(&calls[1], "r1-wired"), "{calls:?}");

    seen.check_fails.store(true, Ordering::SeqCst);
    assert_eq!(
        start(&mut life, &node, "r2"),
        ["refused r2 egress-unchecked"]
    );
    assert_eq!(
        seen.taken(),
        ["check 1"],
        "only the first run's sidecar is there"
    );
    let made = |dir: &Path| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().ends_with("-r2"))
        })
    };
    assert!(!made(&node.scratch), "a lease directory of r2");
    assert!(!made(&node.cgroups.join("actions")), "a cgroup of r2");
    let sidecars = std::fs::read_dir(node.scratch.join("oci-labels")).expect("the ledger");
    assert_eq!(sidecars.count(), 1, "a sidecar of r2");
}

/// Proves a container is wired after its start, with the pid `crun state` names: a staged
/// container is not wired, and its start wires it once with the stand-in's pid under the
/// run's container name. Catches an attach before the start, with another pid (the
/// `crun run` child's, or none), or not at all.
#[test]
fn crun_attaches_the_container_pid_after_start() {
    let node = Node::new("egress-attach");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    let id = runtime.stage(&node.spec("r1")).expect("staged");
    assert_eq!(
        seen.taken(),
        Vec::<String>::new(),
        "nothing wired before the start"
    );
    assert_eq!(asks(&node, "r1"), 0, "no pid asked for before the start");
    runtime.start_staged(&id).expect("started");
    assert_eq!(seen.taken(), [format!("attach {} {PID}", id.0)]);
    assert_eq!(asks(&node, "r1"), 1);
    runtime.remove(&id).expect("removed");
    assert_eq!(seen.taken(), [format!("release {}", id.0)]);
    assert!(nothing_left(&node, "r1"));
}

/// Proves the first exit of the asks for a pid: a container that `crun state` does not know
/// for its first two asks (as a real `crun` right after its start) is asked a third time,
/// named, and wired with that pid, once. Catches a start that gives up at the first empty
/// answer, and one that wires before a pid is named.
#[test]
fn a_pid_named_after_empty_answers_is_wired() {
    let node = Node::new("egress-pid-late");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    let id = runtime
        .create_and_start(&node.spec("r1-late"))
        .expect("wired at the third ask");
    assert_eq!(asks(&node, "r1-late"), 3);
    assert_eq!(seen.taken(), [format!("attach {} {PID}", id.0)]);
    runtime.remove(&id).expect("removed");
    assert_eq!(seen.taken(), [format!("release {}", id.0)]);
    assert!(nothing_left(&node, "r1-late"));
}

/// Proves a container whose network cannot be wired runs no job and leaves nothing: the
/// port refuses the attach (the pool is full), the start fails naming the wiring, the
/// container (which ignores SIGTERM and would run until killed) is ended and removed with
/// its lease (no sidecar, lease directory or cgroup is left, and the runtime lists
/// nothing), and nothing is released, as nothing was attached. Catches a container left
/// running with its job held, and a lease leaked by a failed wiring.
#[test]
fn a_failed_attach_ends_the_container_and_runs_no_job() {
    let node = Node::new("egress-attach-fails");
    let seen = Seen::new(&node);
    seen.attach_fails.store(true, Ordering::SeqCst);
    let mut runtime = open(&node, port(&seen), impatient());
    let why = runtime
        .create_and_start(&node.spec("r1-wired"))
        .map(|_| ())
        .expect_err("not wired");
    assert_eq!(
        why.to_string(),
        format!("wire the container's network: {}", EgressError::Exhausted)
    );
    let calls = seen.taken();
    assert_eq!(calls.len(), 1, "one attach and no release: {calls:?}");
    assert!(attached(&calls[0], "r1-wired"), "{calls:?}");
    assert!(runtime.list_labelled().expect("listed").is_empty());
    assert!(nothing_left(&node, "r1-wired"));
}

/// Proves a start whose `crun state` answers with no usable pid (a state that names none, a
/// pid out of range) fails at that answer: it is asked once, not again, the port is asked
/// nothing, and the container (which runs until killed) is ended and removed with its lease.
/// Catches an attach with a made-up pid, asks repeated over a definite answer, and a lease
/// leaked by the failure.
#[test]
fn a_failed_container_pid_releases_nothing_and_removes_the_lease() {
    let node = Node::new("egress-no-pid");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    for (run, said) in [
        ("r1-badpid", "crun state: no pid in "),
        ("r2-hugepid", "crun state: pid 99999999999 is not a pid"),
    ] {
        let why = runtime
            .create_and_start(&node.spec(run))
            .map(|_| ())
            .expect_err("no pid");
        let text = why.to_string();
        let wanted = format!("wire the container's network: {said}");
        assert!(text.starts_with(&wanted), "{text}");
        assert_eq!(asks(&node, run), 1, "{run}");
        assert!(nothing_left(&node, run), "{run}");
    }
    assert_eq!(seen.taken(), Vec::<String>::new());
    assert!(runtime.list_labelled().expect("listed").is_empty());
}

/// Proves the second exit of the asks for a pid: a container whose `crun run` has ended with
/// no pid named fails once the run is over, instead of being asked for ever; the port is
/// asked nothing and nothing of the lease is left. Catches a start that waits for a pid from
/// a container that is gone.
#[test]
fn a_run_that_ends_before_its_pid_is_named_is_never_wired() {
    let node = Node::new("egress-pid-ended");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    let why = runtime
        .create_and_start(&node.spec("r1-nopid"))
        .map(|_| ())
        .expect_err("no pid");
    let text = why.to_string();
    assert!(
        text.starts_with(
            "wire the container's network: crun run ended before the container had a pid: "
        ),
        "{text}"
    );
    assert!(
        text.ends_with("; crun: no container made"),
        "crun's words: {text}"
    );
    assert!(asks(&node, "r1-nopid") >= 1);
    assert_eq!(seen.taken(), Vec::<String>::new());
    assert!(nothing_left(&node, "r1-nopid"));
}

/// Proves the third exit of the asks for a pid: a container that runs but that `crun state`
/// never knows fails once the route wait has passed on the runtime's clock (here a clock
/// that jumps a minute at each read, so the wait is over after the first ask, which is the
/// only one); the port is asked nothing, and the container (which runs until killed) is ended
/// and removed with its lease. Catches a start that waits for ever for a pid.
#[test]
fn no_pid_within_the_route_wait_is_never_wired() {
    let node = Node::new("egress-pid-deadline");
    let seen = Seen::new(&node);
    let hooks = Hooks {
        now_ms: ticking,
        ..impatient()
    };
    let mut runtime = open(&node, port(&seen), hooks);
    let why = runtime
        .create_and_start(&node.spec("r1-deaf"))
        .map(|_| ())
        .expect_err("no pid in time");
    let text = why.to_string();
    assert!(
        text.starts_with("wire the container's network: the container had no pid within 30s: "),
        "{text}"
    );
    assert_eq!(asks(&node, "r1-deaf"), 1);
    assert_eq!(seen.taken(), Vec::<String>::new());
    assert!(nothing_left(&node, "r1-deaf"));
}

/// Proves the wiring of a run that ended and was never polled is released by its removal,
/// once: a second removal, and a poll after it, release nothing more. Catches a removal
/// that forgets the wiring.
#[test]
fn release_runs_when_the_run_is_removed() {
    let node = Node::new("egress-release-remove");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), Node::hooks(LONG, LONG));
    let id = runtime.create_and_start(&node.spec("r1")).expect("started");
    runtime.wait_ended(&id);
    assert_eq!(seen.taken().len(), 1, "attached");
    runtime.remove(&id).expect("removed");
    assert_eq!(seen.taken(), [format!("release {}", id.0)]);
    runtime.remove(&id).expect("removed again");
    assert_eq!(runtime.poll(&id).expect("polled"), Status::Gone);
    assert_eq!(seen.taken(), Vec::<String>::new());
    assert!(nothing_left(&node, "r1"));
}

/// Proves the wiring of a run that ends by itself is released when the runtime reads the
/// end, before any removal, and not again by the removal. Catches an end that keeps the
/// interface until the lease is removed (a run's wiring outliving it while the lifecycle
/// holds the result).
#[test]
fn release_runs_when_the_run_ends_by_itself() {
    let node = Node::new("egress-release-end");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), Node::hooks(LONG, LONG));
    let id = runtime.create_and_start(&node.spec("r1")).expect("started");
    runtime.wait_ended(&id);
    assert_eq!(seen.taken().len(), 1, "attached");
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    assert_eq!(seen.taken(), [format!("release {}", id.0)]);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    runtime.remove(&id).expect("removed");
    assert_eq!(seen.taken(), Vec::<String>::new(), "released once");
    assert!(nothing_left(&node, "r1"));
}

/// Proves the wiring of a container removed while it runs is released by the removal,
/// once, and the container and its lease go. Catches a removal of a running container that
/// forgets the wiring.
#[test]
fn release_runs_when_a_running_container_is_removed() {
    let node = Node::new("egress-release-running");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    let id = runtime
        .create_and_start(&node.spec("r1-wired"))
        .expect("started");
    assert_eq!(seen.taken().len(), 1, "attached");
    assert_eq!(runtime.poll(&id).expect("polled"), Status::Running);
    runtime.remove(&id).expect("removed");
    assert_eq!(seen.taken(), [format!("release {}", id.0)]);
    assert!(nothing_left(&node, "r1-wired"));
}

/// Log lines written by the test's subscriber on this thread.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lines").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Proves a release that fails is logged as a warning naming the lease and the reason and
/// does not change the run's own result: the end is read as the run gave it, the removal
/// takes the lease apart, and the wiring is not released a second time. Catches a release
/// error returned over the run's result, one that keeps the lease, and one that passes
/// without a trace.
#[test]
fn release_twice_is_harmless_and_a_release_error_does_not_leave_the_lease() {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let node = Node::new("egress-release-fails");
    let seen = Seen::new(&node);
    seen.release_fails.store(true, Ordering::SeqCst);
    let mut runtime = open(&node, port(&seen), Node::hooks(LONG, LONG));
    let id = runtime.create_and_start(&node.spec("r1")).expect("started");
    runtime.wait_ended(&id);
    let ended = runtime.poll(&id).expect("polled");
    assert!(
        matches!(ended, Status::Exited(exit) if exit.code == 0),
        "{ended:?}"
    );
    runtime
        .remove(&id)
        .expect("removed despite the failed release");
    let calls = seen.taken();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1], format!("release {}", id.0));
    assert!(nothing_left(&node, "r1"));
    let logged = String::from_utf8(lines.0.lock().expect("lines").clone()).expect("utf-8");
    assert!(logged.contains("WARN"), "{logged}");
    assert!(logged.contains(&id.0), "{logged}");
    assert!(
        logged.contains(&format!(
            "release the network of the run: {}",
            EgressError::Command {
                what: "deleting the veth".to_owned(),
                why: "Operation not permitted".to_owned(),
            }
        )),
        "{logged}"
    );
}

/// An allow-list of the test host: one endpoint, and `/bin/sh` for the three programs.
fn allow_list() -> EgressConfig {
    let allow = vec!["192.0.2.10:443".to_owned()];
    EgressConfig::from_flags(&Flags {
        mode: Some("allow-list"),
        allow: &allow,
        ip: Some("/bin/sh"),
        nft: Some("/bin/sh"),
        nsenter: Some("/bin/sh"),
        ..Flags::default()
    })
    .expect("a sound allow-list")
}

/// Proves mode none, the default, starts no egress and so calls no port: `start_egress`
/// does not call its start, gives no port, and a whole run (check, create, start, end,
/// removal) asks the port nothing. An allow-list calls the start once with the
/// configuration, and its port is the one the runtime then checks and wires through; a
/// start that fails is the error. Catches a mode-none agent that runs the egress setup, and
/// one whose runtime calls a port it should not have.
#[test]
fn mode_none_calls_no_egress_port() {
    let node = Node::new("egress-mode-none");
    let seen = Seen::new(&node);
    let mut starts = 0;
    let none = start_egress(&EgressConfig::none(), |_| {
        starts += 1;
        Ok(port(&seen))
    });
    assert!(matches!(none, Ok(None)), "{none:?}");
    assert_eq!(starts, 0);
    let mut runtime = open(&node, None, Node::hooks(LONG, LONG));
    runtime.check_egress().expect("nothing to check");
    let id = runtime.create_and_start(&node.spec("r1")).expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    runtime.remove(&id).expect("removed");
    assert_eq!(asks(&node, "r1"), 0, "no pid asked for without a port");
    drop(runtime);

    let wanted = allow_list();
    let started = start_egress(&wanted, |config| {
        assert_eq!(config, wanted);
        starts += 1;
        Ok(port(&seen))
    });
    let port = started.expect("started").expect("an allow-list has a port");
    assert_eq!(starts, 1);
    assert_eq!(
        seen.taken(),
        Vec::<String>::new(),
        "the whole run asked nothing"
    );
    let mut runtime = open(&node, port, Node::hooks(LONG, LONG));
    runtime.check_egress().expect("checked");
    let id = runtime.create_and_start(&node.spec("r2")).expect("started");
    runtime.remove(&id).expect("removed");
    let calls = seen.taken();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], "check 0");
    assert!(attached(&calls[1], "r2"), "{calls:?}");
    assert_eq!(calls[2], format!("release {}", id.0));

    let failed = start_egress(&wanted, |_| Err::<(), _>(EgressError::Forwarding));
    assert_eq!(failed, Err(EgressError::Forwarding));
}

/// Captures the warnings logged on this thread while the guard lives.
fn warnings() -> (Lines, tracing::subscriber::DefaultGuard) {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    (lines, guard)
}

impl Lines {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lines").clone()).expect("utf-8")
    }
}

/// Proves a container's pid that `crun state` names only after the route wait has passed on
/// the runtime's clock (a clock that jumps a minute at each read) is refused: the container's
/// init has given up its wait, so the port is not asked to wire it, and the container is
/// ended and removed with its lease. Catches a late answer that slips past the wait.
#[test]
fn a_pid_named_after_the_route_wait_is_refused() {
    let node = Node::new("egress-pid-late-answer");
    let seen = Seen::new(&node);
    let hooks = Hooks {
        now_ms: ticking,
        ..impatient()
    };
    let mut runtime = open(&node, port(&seen), hooks);
    let why = runtime
        .create_and_start(&node.spec("r1-wired"))
        .map(|_| ())
        .expect_err("named too late");
    assert_eq!(
        why.to_string(),
        "wire the container's network: crun state named the pid after the route wait of 30s: \
         the init has given up"
    );
    assert_eq!(asks(&node, "r1-wired"), 1);
    assert_eq!(seen.taken(), Vec::<String>::new());
    assert!(nothing_left(&node, "r1-wired"));
}

/// Proves a release that fails on the removal path itself (the run's end never polled, and a
/// run removed while it runs) is logged as a warning naming the lease and the reason, the
/// removal still takes the lease apart, and nothing is released again afterwards. Catches a
/// removal that returns the release error, keeps the lease, or releases twice.
#[test]
fn a_release_error_on_removal_is_logged_and_the_lease_still_goes() {
    let (lines, _guard) = warnings();
    let node = Node::new("egress-remove-release-fails");
    let seen = Seen::new(&node);
    seen.release_fails.store(true, Ordering::SeqCst);
    let mut runtime = open(&node, port(&seen), impatient());
    let ended = runtime.create_and_start(&node.spec("r1")).expect("started");
    runtime.wait_ended(&ended);
    let running = runtime
        .create_and_start(&node.spec("r2-wired"))
        .expect("started");
    seen.taken();
    for (id, run) in [(&ended, "r1"), (&running, "r2-wired")] {
        runtime
            .remove(id)
            .expect("removed despite the failed release");
        assert_eq!(seen.taken(), [format!("release {}", id.0)], "{run}");
        assert!(nothing_left(&node, run), "{run}");
        let said = lines.text();
        let warned = said.lines().any(|line| {
            line.contains("WARN")
                && line.contains(&id.0)
                && line.contains(
                    "release the network of the run: egress: deleting the veth: \
                                  Operation not permitted",
                )
        });
        assert!(warned, "{said}");
    }
    for id in [&ended, &running] {
        runtime.remove(id).expect("gone already");
        assert_eq!(runtime.poll(id).expect("polled"), Status::Gone);
    }
    assert_eq!(seen.taken(), Vec::<String>::new(), "released once each");
}

/// Proves a removal that fails after it released the wiring (here `crun delete` fails) keeps
/// the lease for the next removal, which takes it apart without releasing the wiring again.
/// Catches a retried removal that releases twice, and one that cannot finish because the
/// wiring is gone.
#[test]
fn a_removal_that_fails_after_its_release_is_retried_without_a_second_release() {
    let node = Node::new("egress-remove-retry");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    let id = runtime
        .create_and_start(&node.spec("r1-wired"))
        .expect("started");
    seen.taken();
    let fails = node.dir.path().join("delete-fails");
    std::fs::write(&fails, b"").expect("make crun delete fail");
    let why = runtime.remove(&id).expect_err("crun delete fails");
    assert!(
        why.to_string().starts_with("delete the container: "),
        "{why}"
    );
    assert_eq!(seen.taken(), [format!("release {}", id.0)]);
    assert_eq!(runtime.list_labelled().expect("listed").len(), 1, "kept");
    std::fs::remove_file(&fails).expect("let crun delete work");
    runtime.remove(&id).expect("removed at the retry");
    assert_eq!(seen.taken(), Vec::<String>::new(), "no second release");
    assert!(nothing_left(&node, "r1-wired"));
}

/// Proves a start whose wiring fails and whose removal then fails too returns the wiring's
/// error (the cause), logs the removal's failure naming the lease, and keeps the container
/// listed so that a later removal takes it apart: nothing is left once it does. Catches a
/// cleanup failure that hides the cause, passes without a trace, or forgets the container.
#[test]
fn a_failed_wiring_whose_cleanup_fails_keeps_the_lease_for_a_later_removal() {
    let (lines, _guard) = warnings();
    let node = Node::new("egress-wire-and-cleanup-fail");
    let seen = Seen::new(&node);
    seen.attach_fails.store(true, Ordering::SeqCst);
    let mut runtime = open(&node, port(&seen), impatient());
    let fails = node.dir.path().join("delete-fails");
    std::fs::write(&fails, b"").expect("make crun delete fail");
    let why = runtime
        .create_and_start(&node.spec("r1-wired"))
        .map(|_| ())
        .expect_err("not wired");
    assert_eq!(
        why.to_string(),
        format!("wire the container's network: {}", EgressError::Exhausted)
    );
    let listed = runtime.list_labelled().expect("listed");
    assert_eq!(listed.len(), 1, "the container is kept: {listed:?}");
    let id = listed[0].id.clone();
    let said = lines.text();
    let warned = said.lines().any(|line| {
        line.contains("WARN")
            && line.contains(&id.0)
            && line.contains("remove the failed start: delete the container: ")
    });
    assert!(warned, "{said}");
    std::fs::remove_file(&fails).expect("let crun delete work");
    runtime.remove(&id).expect("removed later");
    assert!(nothing_left(&node, "r1-wired"));
    assert_eq!(seen.taken().len(), 1, "one attach, nothing to release");
}

/// Proves a wiring that fails after the container started is, to the lifecycle, a start that
/// failed: no `started` event, a `start-failed` finding that says the wiring failed, and a
/// `failed_to_start` result with the runtime's detail, for a refused attach and for a
/// container that never named a pid alike; nothing of either lease is left. Catches a job
/// reported running, or reported with no reason, when its network was never wired.
#[test]
fn a_failed_wiring_is_a_failed_start_of_the_lifecycle() {
    let node = Node::new("egress-life-wiring-fails");
    let seen = Seen::new(&node);
    seen.attach_fails.store(true, Ordering::SeqCst);
    let runtime = open(&node, port(&seen), impatient());
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    let mut life = lifecycle(&node, runtime);
    for run in ["r1-wired", "r2-nopid"] {
        let events = start(&mut life, &node, run);
        assert!(
            !events.iter().any(|e| e.starts_with("started")),
            "{events:?}"
        );
        assert!(events.contains(&format!("result {run}")), "{events:?}");
        let finding = events.iter().any(|e| {
            e.starts_with("Finding")
                && e.contains("start-failed")
                && e.contains("wire the container's network")
        });
        assert!(finding, "{events:?}");
        let results = life.results();
        let result = results.iter().find(|r| r.run_id == run).expect("a result");
        assert_eq!(result.outcome, js_agent_wire::Outcome::FailedToStart);
        assert_eq!(result.detail.as_deref(), Some("runtime-failed"));
        assert!(nothing_left(&node, run), "{run}");
    }
    let calls = seen.taken();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("check")).count(),
        2,
        "{calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("attach")).count(),
        1,
        "{calls:?}"
    );
}
