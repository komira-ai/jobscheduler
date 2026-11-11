//! The runtime's host ports, with the namespace step faked: when `CrunRuntime` offers a
//! job's host ports (after the container's pid is known, at every level, with no egress),
//! what it records of them in the container's labels and its init's arguments, what a failed
//! offer leaves, when the listeners close, and how a restart takes them over. The fake binds
//! each listener on `127.0.0.1` port 0 of the test's own namespace and records what it was
//! asked; the forwarder that copies the bytes is the real one. Whether a listener made in a
//! job's namespace answers there is the `crun_runtime` target's.
//!
//! Every run a test starts is owned by a [`Supervisor`] guard, which ends the run's
//! supervisor and what it runs when the test ends, a failing test included; and every wait
//! is bounded, so a run that does not answer fails its test rather than hanging it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, PidfdFlags, Signal};

use js_agent::crun::{CrunRuntime, HostPorts, Reason};
use js_agent::host_ports::Listen;
use js_agent::lifecycle::{RunHandle, RunSpec, Runtime, Status, label};
use js_agent_wire::{HostPort, PortProtocol, PortRange};
use serde_json::{Value, json};

use crate::node::{NoImages, Node, PID};

/// The fake namespace step: records `listen <pid> <ports>`, binds on loopback, and fails when
/// told to.
#[derive(Debug, Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    addrs: Mutex<Vec<SocketAddr>>,
    fails: AtomicBool,
}

impl Fake {
    /// The calls so far, and empties the record.
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }

    /// The address of the listener made `index`th.
    fn addr(&self, index: usize) -> SocketAddr {
        self.addrs.lock().expect("addrs")[index]
    }
}

impl Listen for Fake {
    fn listen(&self, pid: u32, ports: &[u16]) -> std::io::Result<Vec<TcpListener>> {
        let text: Vec<String> = ports.iter().map(u16::to_string).collect();
        let call = format!("listen {pid} {}", text.join(","));
        self.calls.lock().expect("calls").push(call);
        if self.fails.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("no such namespace"));
        }
        let mut made = Vec::new();
        for _ in ports {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            self.addrs
                .lock()
                .expect("addrs")
                .push(listener.local_addr()?);
            made.push(listener);
        }
        Ok(made)
    }
}

/// A runtime on `node` that takes the host ports `allowed` and listens with `fake`.
fn open(node: &Node, fake: &Arc<Fake>, allowed: Vec<PortRange>) -> CrunRuntime {
    let mut settings = node.settings("/root");
    settings.host_ports = HostPorts {
        allowed,
        listen: Arc::clone(fake) as Arc<dyn Listen>,
    };
    // A removal ends a container that ignores SIGTERM by force at once.
    let hooks = Node::hooks(std::time::Duration::ZERO, crate::node::LONG);
    CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens")
}

/// A host port that is `host` on the host and `job` in the job when it is given.
fn port(host: u16, job: Option<u16>) -> HostPort {
    HostPort {
        host_port: host,
        container_port: job,
        protocol: PortProtocol::Tcp,
    }
}

/// A listener on the host's loopback that stands for what the job reaches.
fn host_side() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a host listener");
    let port = listener.local_addr().expect("its address").port();
    (listener, port)
}

/// The job `run` of `node`, asking for `ports`.
fn job(node: &Node, run: &str, ports: Vec<HostPort>) -> RunSpec {
    RunSpec {
        host_ports: ports,
        ..node.job(run, 600_000)
    }
}

/// How long a run's supervisor is given to end on SIGTERM before it is killed.
const GRACE: Duration = Duration::from_millis(200);

/// How long a killed supervisor, or a run that is to say it is up, is waited for before the
/// test fails: far more than either takes, so only one that does not answer reaches it.
const ANSWER: Duration = Duration::from_secs(30);

/// How many times a closed listener is tried before the test fails: it closes as soon as the
/// forwarder's thread drops it, so only one that stays open reaches this.
const TRIES: u32 = 1_000_000;

/// The supervisor of a run a test started, held by a pidfd taken while it runs: dropped (the
/// test passed or failed), it signals the supervisor's process group (the supervisor, the
/// stand-in `crun` and what that started) SIGTERM, then SIGKILL after [`GRACE`], and waits up
/// to [`ANSWER`] for the supervisor's end (the runtime's thread waits on it as its child, so
/// it is reaped). One that outlives the kill fails the test, unless the test failed already.
/// One that had ended before the guard was made has no pidfd, and nothing to end.
struct Supervisor {
    pid: Pid,
    pidfd: Option<OwnedFd>,
}

impl Supervisor {
    /// The supervisor of the run `id` on `node`, as its sidecar names it.
    fn of(node: &Node, id: &RunHandle) -> Self {
        let text = std::fs::read_to_string(node.sidecar(&id.0)).expect("the sidecar");
        let json: Value = serde_json::from_str(&text).expect("json");
        let raw = json["supervisor"]["pid"]
            .as_i64()
            .expect("a supervisor pid");
        let pid = Pid::from_raw(i32::try_from(raw).expect("a pid")).expect("not 0");
        let pidfd = match rustix::process::pidfd_open(pid, PidfdFlags::empty()) {
            Err(rustix::io::Errno::SRCH) => None,
            opened => Some(opened.expect("a pidfd")),
        };
        Self { pid, pidfd }
    }

    /// Whether the supervisor ends within `bound` (or had ended before the guard was made).
    fn ends_within(&self, bound: Duration) -> bool {
        let Some(pidfd) = &self.pidfd else {
            return true;
        };
        let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
        let timeout = Timespec::try_from(bound).expect("a timeout");
        rustix::event::poll(&mut fds, Some(&timeout)).is_ok_and(|ready| ready > 0)
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        for (signal, bound) in [(Signal::TERM, GRACE), (Signal::KILL, ANSWER)] {
            if self.ends_within(Duration::ZERO) {
                return;
            }
            // The group of a supervisor that has just ended may be gone: nothing to signal.
            let _ = rustix::process::kill_process_group(self.pid, signal);
            if self.ends_within(bound) {
                return;
            }
        }
        if !std::thread::panicking() {
            panic!(
                "the supervisor {} outlived its kill",
                self.pid.as_raw_nonzero()
            );
        }
    }
}

/// Waits up to [`ANSWER`] for the script of the container `id` on `node` to say it is up
/// (the pipe [`Node::pipe`] made), failing the test if it does not.
fn up(node: &Node, id: &RunHandle) {
    use std::os::unix::fs::OpenOptionsExt;
    let path = node.dir.join(format!("ready-{}", id.0));
    let flags = rustix::fs::OFlags::NONBLOCK.bits().cast_signed();
    let pipe = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path);
    let mut pipe = pipe.expect("the ready pipe");
    let mut fds = [PollFd::new(&pipe, PollFlags::IN)];
    let timeout = Timespec::try_from(ANSWER).expect("a timeout");
    let ready = rustix::event::poll(&mut fds, Some(&timeout)).expect("poll the ready pipe");
    assert_eq!(ready, 1, "the run did not say it is up within {ANSWER:?}");
    // The writer is there and has written: the rest is read to its close, blocking.
    rustix::fs::fcntl_setfl(&pipe, rustix::fs::OFlags::empty()).expect("a blocking read");
    let mut said = String::new();
    pipe.read_to_string(&mut said).expect("read the pipe");
    assert_eq!(said.trim(), "up");
}

/// Starts `spec` and has the init in its cgroup: a run a restart can adopt, with the guard of
/// its supervisor.
fn held(node: &Node, runtime: &mut CrunRuntime, spec: &RunSpec) -> (RunHandle, Supervisor) {
    let id = runtime.stage(spec).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    let supervisor = Supervisor::of(node, &id);
    up(node, &id);
    node.place(&id.0, PID);
    (id, supervisor)
}

/// Starts `spec`, whose stand-in never says it is up (`*-badpid`, `*-deaf`, `*-wired`), and has its init
/// in its cgroup: a run a restart can adopt, with the guard of its supervisor.
fn held_silent(node: &Node, runtime: &mut CrunRuntime, spec: &RunSpec) -> (RunHandle, Supervisor) {
    let id = runtime.stage(spec).expect("staged");
    runtime.start_staged(&id).expect("started");
    let supervisor = Supervisor::of(node, &id);
    node.place(&id.0, PID);
    (id, supervisor)
}

/// Creates and starts `spec`, with the guard of its supervisor.
fn started(node: &Node, runtime: &mut CrunRuntime, spec: &RunSpec) -> (RunHandle, Supervisor) {
    let id = runtime.create_and_start(spec).expect("created and started");
    let supervisor = Supervisor::of(node, &id);
    (id, supervisor)
}

/// Changes the label `key` of the sidecar of the lease `name`.
fn set_label(node: &Node, name: &str, key: &str, value: &str) {
    let text = std::fs::read_to_string(node.sidecar(name)).expect("the sidecar");
    let mut json: Value = serde_json::from_str(&text).expect("json");
    json["labels"][key] = json!(value);
    std::fs::write(node.sidecar(name), json.to_string()).expect("write the sidecar");
}

/// Whether a connection to `addr` is refused now.
fn refused(addr: SocketAddr) -> bool {
    TcpStream::connect(addr).is_err()
}

/// Waits until `addr` refuses connections, failing the test after [`TRIES`]: a listener the
/// forwarder holds is closed by its runtime's thread, shortly after what closes it returns.
fn wait_refused(addr: SocketAddr) {
    let closed = (0..TRIES).any(|_| refused(addr));
    assert!(closed, "{addr} still accepts after {TRIES} tries");
}

/// Sends `say` from the job's side to `job`, expects the host side `host` to read it and
/// answer `back`, and expects the job's side to read the answer.
fn exchange(job: SocketAddr, host: &TcpListener, say: &[u8], back: &[u8]) {
    let mut from_job = TcpStream::connect(job).expect("the job's listener answers");
    from_job.write_all(say).expect("write to the host");
    let (mut at_host, _) = host.accept().expect("the host's port is reached");
    let mut got = vec![0; say.len()];
    at_host.read_exact(&mut got).expect("read at the host");
    assert_eq!(got, say);
    at_host.write_all(back).expect("write to the job");
    let mut answer = vec![0; back.len()];
    from_job.read_exact(&mut answer).expect("read at the job");
    assert_eq!(answer, back);
}

fn read_bundle(node: &Node, id: &RunHandle) -> Value {
    let path = node.lease_of(&id.0).join("bundle/config.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("the bundle")).expect("a config")
}

/// Proves a start with host ports, at level `none` on a runtime with no egress, asks the
/// namespace step once with the container's pid and the job ports in order, tells the init to
/// wait for them, records them in the run's labels, and joins a connection to each job port
/// to the host port, bytes both ways. Catches host ports offered only when an egress exists
/// or at a wired level, a job port taken as the host port, an init that does not wait, a
/// missing label, and a connection not carried to the host.
#[test]
fn a_start_with_host_ports_offers_them_at_level_none_and_carries_bytes() {
    let node = Node::new("host-ports-start");
    let fake = Arc::new(Fake::default());
    let mut runtime = open(&node, &fake, Vec::new());
    let (first, first_port) = host_side();
    let (second, second_port) = host_side();
    let ports = vec![port(first_port, None), port(second_port, Some(7001))];
    let (id, _supervisor) = started(&node, &mut runtime, &job(&node, "r1-wired", ports));

    assert_eq!(fake.taken(), [format!("listen {PID} {first_port},7001")]);
    let bundle = read_bundle(&node, &id);
    assert_eq!(
        bundle["process"]["args"],
        json!([
            "/dev/js-init",
            "--wait-listen",
            "30",
            format!("{first_port},7001"),
            "--",
            "/bin/tool",
            "--go"
        ])
    );
    let found = runtime.list_labelled().expect("lists");
    assert_eq!(found.len(), 1);
    let want = format!("{first_port},{second_port}:7001");
    assert_eq!(found[0].labels[label::HOST_PORTS], want);

    exchange(fake.addr(0), &first, b"ping", b"pong");
    exchange(fake.addr(1), &second, b"hello", b"world");
    runtime.remove(&id).expect("removed");
}

/// Proves a start with no host ports never asks the namespace step, writes no label and adds
/// no wait to the init's arguments. Catches an offer made for every run, and a label or a
/// wait written with nothing to record.
#[test]
fn a_start_without_host_ports_asks_nothing_and_records_nothing() {
    let node = Node::new("host-ports-none");
    let fake = Arc::new(Fake::default());
    let mut runtime = open(&node, &fake, Vec::new());
    let (id, _supervisor) = started(&node, &mut runtime, &job(&node, "r1-wired", Vec::new()));
    assert_eq!(fake.taken(), Vec::<String>::new());
    let found = runtime.list_labelled().expect("lists");
    assert!(!found[0].labels.contains_key(label::HOST_PORTS));
    let bundle = read_bundle(&node, &id);
    assert_eq!(
        bundle["process"]["args"],
        json!(["/dev/js-init", "--", "/bin/tool", "--go"])
    );
    runtime.remove(&id).expect("removed");
}

/// Proves a start whose listeners cannot be made fails with the reason, naming the job's
/// ports, and leaves no container. Catches a start that goes on without its host ports, and
/// one that leaves the container behind.
#[test]
fn a_start_whose_listen_fails_fails_and_leaves_no_container() {
    let node = Node::new("host-ports-listen-fails");
    let fake = Arc::new(Fake::default());
    fake.fails.store(true, Ordering::SeqCst);
    let mut runtime = open(&node, &fake, Vec::new());
    let ports = vec![port(8080, None), port(9090, Some(7002))];
    let why = runtime
        .create_and_start(&job(&node, "r1-wired", ports))
        .expect_err("refused");
    let text = why.to_string();
    assert!(
        text.contains(
            "offer the host ports: listen at the job's ports 8080,7002 in its network namespace"
        ),
        "{text}"
    );
    assert!(text.contains("no such namespace"), "{text}");
    assert_eq!(fake.taken(), [format!("listen {PID} 8080,7002")]);
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves the job's listener closes when the run is removed while it runs, and when a run
/// that ends by itself is read to its end. Catches an offer that outlives its run.
#[test]
fn the_listeners_close_when_the_run_is_removed_and_when_it_ends() {
    let node = Node::new("host-ports-close");
    let fake = Arc::new(Fake::default());
    let mut runtime = open(&node, &fake, Vec::new());
    let (_host, host_port) = host_side();
    let ports = vec![port(host_port, None)];

    let (running, _running) = started(&node, &mut runtime, &job(&node, "r1-wired", ports.clone()));
    let addr = fake.addr(0);
    assert!(!refused(addr), "listening while the run runs");
    runtime.remove(&running).expect("removed");
    wait_refused(addr);

    let (ending, supervisor) = started(&node, &mut runtime, &job(&node, "r2", ports));
    let addr = fake.addr(1);
    // The run ends by itself, and its supervisor once it has written the end.
    assert!(
        supervisor.ends_within(ANSWER),
        "the run did not end within {ANSWER:?}"
    );
    assert!(matches!(runtime.poll(&ending), Ok(Status::Exited(_))));
    wait_refused(addr);
    runtime.remove(&ending).expect("removed");
}

/// Proves a restart asks the namespace step again for a run that holds host ports, with the
/// container's pid and the job ports the label records, and the run is adopted with nothing
/// lost, its listener carrying bytes. Catches host ports lost with the agent that offered
/// them, and a job port read back as the host port.
#[test]
fn a_restart_offers_the_host_ports_of_an_adopted_run_again() {
    let node = Node::new("host-ports-adopt");
    let first_fake = Arc::new(Fake::default());
    let mut first = open(&node, &first_fake, Vec::new());
    let (host, host_port) = host_side();
    let ports = vec![port(host_port, Some(7003))];
    let (id, _supervisor) = held(&node, &mut first, &job(&node, "r1-hold", ports));
    assert_eq!(first_fake.taken(), [format!("listen {PID} 7003")]);
    drop(first);

    let fake = Arc::new(Fake::default());
    let second = open(&node, &fake, Vec::new());
    assert_eq!(second.swept().adopted, ["r1-hold"], "{:?}", second.swept());
    assert_eq!(fake.taken(), [format!("listen {PID} 7003")]);
    assert_eq!(second.host_port_lost(&id), None);
    exchange(fake.addr(0), &host, b"again", b"back!");
}

/// Proves a run whose host ports cannot be offered again is still adopted and listed, marked
/// lost with a text naming the job ports. Catches a run dropped, or kept with nothing to say
/// its ports are gone.
#[test]
fn a_run_whose_host_ports_cannot_be_offered_again_is_adopted_and_marked_lost() {
    let node = Node::new("host-ports-adopt-lost");
    let first_fake = Arc::new(Fake::default());
    let mut first = open(&node, &first_fake, Vec::new());
    let ports = vec![port(8081, None), port(8082, Some(7004))];
    let (id, _supervisor) = held(&node, &mut first, &job(&node, "r1-hold", ports));
    drop(first);

    let fake = Arc::new(Fake::default());
    fake.fails.store(true, Ordering::SeqCst);
    let mut second = open(&node, &fake, Vec::new());
    assert_eq!(second.swept().adopted, ["r1-hold"], "{:?}", second.swept());
    assert_eq!(fake.taken(), [format!("listen {PID} 8081,7004")]);
    assert_eq!(second.list_labelled().expect("lists").len(), 1);
    let lost = second.host_port_lost(&id).expect("marked lost");
    assert!(lost.contains("8081,7004"), "{lost}");
    assert!(lost.contains("no such namespace"), "{lost}");
}

/// Proves an adopted run whose host-ports label is not a list of host ports is removed with
/// that reason, and nothing is offered for it. Catches a malformed record read as no host
/// ports, a zero port, and an entry left empty.
#[test]
fn a_run_with_a_malformed_host_ports_label_is_removed_with_its_reason() {
    for (index, bad) in ["x", "0", "80:", "80:0", "1,,2"].into_iter().enumerate() {
        let node = Node::new(&format!("host-ports-adopt-bad-{index}"));
        let first_fake = Arc::new(Fake::default());
        let mut first = open(&node, &first_fake, Vec::new());
        let ports = vec![port(8083, None)];
        let (id, _supervisor) = held(&node, &mut first, &job(&node, "r1-hold", ports));
        set_label(&node, &id.0, label::HOST_PORTS, bad);
        drop(first);

        let fake = Arc::new(Fake::default());
        let second = open(&node, &fake, Vec::new());
        let report = second.swept();
        assert!(report.adopted.is_empty(), "{bad}: {report:?}");
        assert_eq!(report.removed.len(), 1, "{bad}: {report:?}");
        let want = "the label jobscheduler.dev/host-ports is not a list of host ports";
        assert_eq!(
            report.removed[0].reason,
            Reason::Inconsistent(want.to_owned()),
            "{bad}"
        );
        assert_eq!(fake.taken(), Vec::<String>::new(), "{bad}");
    }
}

/// Proves an adopted run with no host ports never asks the namespace step, and is not
/// marked lost. Catches an offer made for every adopted run, and a pid asked for with
/// nothing to offer.
#[test]
fn an_adopted_run_with_no_host_ports_asks_nothing() {
    let node = Node::new("host-ports-adopt-none");
    let first_fake = Arc::new(Fake::default());
    let mut first = open(&node, &first_fake, Vec::new());
    let (id, _supervisor) = held(&node, &mut first, &job(&node, "r1-hold", Vec::new()));
    drop(first);

    let fake = Arc::new(Fake::default());
    let second = open(&node, &fake, Vec::new());
    assert_eq!(second.swept().adopted, ["r1-hold"], "{:?}", second.swept());
    assert_eq!(fake.taken(), Vec::<String>::new());
    assert_eq!(second.host_port_lost(&id), None);
}

/// Proves an adopted run with host ports whose container `crun state` names no usable pid
/// (its state was read: the container is ending) is adopted with nothing offered and nothing
/// lost, and that one whose `crun state` fails is adopted with nothing offered and marked lost,
/// saying why. Catches a run stopped as one that lost its host ports when it was only ending,
/// a run left running with no listener because `crun state` failed once, and an offer made
/// with no namespace to make it in.
#[test]
fn an_adopted_run_with_no_pid_is_offered_nothing() {
    for (n, (run, lost)) in [("r1-badpid", false), ("r2-deaf", true)]
        .into_iter()
        .enumerate()
    {
        let node = Node::new(&format!("host-ports-adopt-nopid-{n}"));
        let first_fake = Arc::new(Fake::default());
        let mut first = open(&node, &first_fake, Vec::new());
        let (id, _supervisor) = held_silent(&node, &mut first, &job(&node, run, Vec::new()));
        drop(first);
        set_label(&node, &id.0, label::HOST_PORTS, "8080:80");

        let fake = Arc::new(Fake::default());
        let second = open(&node, &fake, Vec::new());
        assert_eq!(second.swept().adopted, [run], "{:?}", second.swept());
        assert_eq!(fake.taken(), Vec::<String>::new(), "{run}");
        let said = second.host_port_lost(&id);
        assert_eq!(said.is_some(), lost, "{run}: {said:?}");
        if let Some(why) = said {
            assert!(why.starts_with("find the container's pid: "), "{why}");
        }
    }
}

/// Proves the guard of a run's supervisor ends a run its test left running (a stand-in that
/// ignores SIGTERM, so the kill after the grace is what ends it), and that the supervisor is
/// gone once the guard is dropped. Catches a test that leaves a supervisor and its `crun`
/// running after it, passed or failed.
#[test]
fn the_guard_ends_a_run_its_test_left_running() {
    let node = Node::new("host-ports-guard");
    let fake = Arc::new(Fake::default());
    let mut runtime = open(&node, &fake, Vec::new());
    let (id, guard) = held_silent(&node, &mut runtime, &job(&node, "r1-wired", Vec::new()));
    let watch = Supervisor::of(&node, &id);
    assert!(!watch.ends_within(Duration::ZERO), "the run is running");
    drop(guard);
    assert!(
        watch.ends_within(Duration::ZERO),
        "the guard ended the supervisor"
    );
}

/// Proves the runtime answers the host ports it was opened to allow, in order, and none by
/// default. Catches a list dropped between the settings and the runtime.
#[test]
fn the_runtime_answers_the_host_ports_it_allows() {
    let node = Node::new("host-ports-allowed");
    let fake = Arc::new(Fake::default());
    assert_eq!(open(&node, &fake, Vec::new()).host_ports(), Vec::new());
    let allowed = vec![
        PortRange::new(8080, 8080).expect("a port"),
        PortRange::new(9000, 9100).expect("a range"),
    ];
    assert_eq!(open(&node, &fake, allowed.clone()).host_ports(), allowed);
}
