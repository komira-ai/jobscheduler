//! A job that runs across an agent hand-over keeps its egress, against the real kernel and a
//! real systemd unit: the old agent ends leaving the wiring, nothing enforces the rules for a
//! while but the kernel, and the next agent takes the wiring over and never cuts it.
//!
//! The agent here is a probe: this test binary again, run in a transient unit by `systemd-run`
//! as `--exact handover::probe_of_the_agent` with the packaged unit's `KillMode=` and
//! delegation (the same way `tests/crun_supervisor/unit.rs` runs its probe). The container is a
//! [`Holder`] that the test starts outside the units: the probe is given its pid, as the agent
//! is given the pid of a container that outlives it. The probe plays one of four agents, by the
//! variable `JS_HANDOVER_ROLE`:
//!
//! - `first-leave`: starts the egress, wires the holder at the allow-list level with one
//!   declared port, says `up`, and on `SIGTERM` (what `systemctl stop` sends) leaves the wiring
//!   and exits: what the agent does when it hands its runs over;
//! - `first-drop`: the same, except that on `SIGTERM` it releases the wiring: what the agent
//!   did before a hand-over could carry an egress ceiling;
//! - `second`: starts the egress again (which keeps what it finds), adopts the holder's wiring
//!   and says `adopted`, settles when told to and says `settled` once the firewall checks as
//!   loaded, and releases the run when told to and says `released`;
//! - `settle-only`: starts the egress, takes nothing over, settles and says `settled`.
//!
//! Both agents use the same egress store, the directory `state/egress` of the test's directory:
//! the first agent's attach writes the record `job.json` there, the second agent's start keeps
//! the wiring that record names, and the record is removed with the wiring (the run's release,
//! or the settle of a run nobody adopted).
//!
//! No clock is read. The test and the probe talk through named pipes in a directory of the
//! test, each side waiting for the other's word: the probe's `up`, `adopted`, `settled` and
//! `released`, and the test's `settle` and `release`. A probe that fails says `failed: <why>` on
//! the pipe the test waits on, so a failure is a message, never a hang; the stop of the first
//! unit returns when the probe has exited, and the second unit is a unit of its own, started
//! after the first has stopped.
//!
//! What the tests look at, from the holder's namespace and from the far host, in each phase:
//! the allowed endpoint answers; another host, another port and the metadata address do not
//! (their listeners count nothing); the declared port reaches the holder; a connection opened
//! to the allowed endpoint before the stop still carries data (the same veth, conntrack entry
//! and masquerade binding); and the host still has the same interface (by its index) and set
//! member.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use js_agent::egress::store::Store;
use js_agent::egress::{Config, Egress, Flags};
use js_types::{JobNetwork, PortMap};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};
use serde_json::Value;

use crate::sys::{
    Far, Holder, Reach, Server, clear_tables, far, in_netns, ip, ip_in, links, run, serial, tcp,
    text, tool, wired,
};

/// The variable that makes the probe act: the test's directory for it. Set only on a probe's
/// unit.
const PROBE_DIR: &str = "JS_HANDOVER_PROBE_DIR";

/// The variable that says which agent the probe plays.
const PROBE_ROLE: &str = "JS_HANDOVER_ROLE";

/// The host port the job declares, and the container port it maps to.
const HOST_PORT: u16 = 8080;
const JOB_PORT: u16 = 7000;

/// The configuration of the probe's agent and of the tests: the allow-list ceiling with the
/// far host's allowed endpoint and the host port 8080 a job may declare.
fn config() -> Config {
    let allow = ["192.0.2.10:9300".to_owned()];
    let ports = [HOST_PORT.to_string()];
    Config::from_flags(&Flags {
        mode: Some("allow-list"),
        allow: &allow,
        forward_ports: &ports,
        ..Flags::default()
    })
    .expect("a sound configuration")
}

/// The ports the job declares.
fn ports() -> Vec<PortMap> {
    vec![PortMap {
        host: HOST_PORT,
        container: JOB_PORT,
    }]
}

/// The host's address on the link to the far host, at the declared host port.
fn host_port() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(far::HOST, HOST_PORT))
}

/// The name of the record the run `job` has in the store.
const RECORD: &str = "job.json";

/// The state root the probes share, in the test's directory `dir`.
fn state_root(dir: &Path) -> PathBuf {
    dir.join("state")
}

/// The store the probe opens: in the state root of the test's directory.
fn open_store(dir: &Path) -> Result<Store, String> {
    let root = state_root(dir);
    fs::create_dir_all(&root).map_err(|why| failed("state root", &why))?;
    Store::open(&root).map_err(|why| failed("store", &why))
}

/// The packaged unit's value of `key` in its `[Service]` section.
fn packaged(key: &str) -> String {
    let unit = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/js-agent/js-agent-crun.service");
    let text = fs::read_to_string(unit).expect("read the packaged unit");
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("the packaged unit sets no {key}"))
        .trim()
        .to_owned()
}

/// Makes the named pipe `path`.
fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap_or_else(|e| panic!("make the pipe {}: {e}", path.display()));
}

/// The named pipes between a test and its probe: one per word, in `dir`.
struct Wire {
    dir: PathBuf,
}

impl Wire {
    /// The word written to the pipe `name`, waiting until the other side opens and closes it.
    fn heard(&self, name: &str) -> String {
        let path = self.dir.join(name);
        let mut word = String::new();
        File::open(&path)
            .and_then(|mut pipe| pipe.read_to_string(&mut word))
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        word.trim().to_owned()
    }

    /// Writes `word` to the pipe `name`, waiting until the other side opens it.
    fn say(&self, name: &str, word: &str) {
        let path = self.dir.join(name);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut pipe| pipe.write_all(word.as_bytes()))
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
}

/// Which agent the probe plays.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    FirstLeave,
    FirstDrop,
    Second,
    SettleOnly,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::FirstLeave => "first-leave",
            Self::FirstDrop => "first-drop",
            Self::Second => "second",
            Self::SettleOnly => "settle-only",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::FirstLeave,
            Self::FirstDrop,
            Self::Second,
            Self::SettleOnly,
        ]
        .into_iter()
        .find(|role| role.name() == name)
    }
}

/// `step` and why it failed, as the probe says it.
fn failed(step: &str, why: &dyn std::fmt::Display) -> String {
    format!("{step}: {why}")
}

/// What the probe does as `role`; the first agent wires the holder `pid`. `next` names the pipe the test waits on
/// at each step: a failure is said there.
fn play(role: Role, pid: u32, wire: &Wire, next: &Cell<&'static str>) -> Result<(), String> {
    let store = open_store(&wire.dir)?;
    let egress = Egress::start(config(), Some(store)).map_err(|why| failed("start", &why))?;
    match role {
        Role::FirstLeave | Role::FirstDrop => {
            let attachment = egress
                .attach("job", pid, JobNetwork::AllowList, &ports())
                .map_err(|why| failed("attach", &why))?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|why| failed("runtime", &why))?;
            let mut term = runtime
                .block_on(async {
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                })
                .map_err(|why| failed("handler", &why))?;
            // After this the test waits for the stop, not for a word.
            next.set("");
            wire.say("up", "up");
            runtime.block_on(term.recv());
            if role == Role::FirstLeave {
                attachment.leave();
            } else {
                attachment
                    .release()
                    .map_err(|why| failed("release", &why))?;
            }
            drop(egress);
        }
        Role::Second => {
            let attachment = egress
                .adopt("job", JobNetwork::AllowList, &ports())
                .map_err(|why| failed("adopt", &why))?;
            next.set("settled");
            wire.say("adopted", "adopted");
            assert_eq!(wire.heard("settle"), "settle");
            egress.settle().map_err(|why| failed("settle", &why))?;
            egress.check().map_err(|why| failed("check", &why))?;
            next.set("released");
            wire.say("settled", "settled");
            assert_eq!(wire.heard("release"), "release");
            attachment
                .release()
                .map_err(|why| failed("release", &why))?;
            next.set("");
            wire.say("released", "released");
        }
        Role::SettleOnly => {
            egress.settle().map_err(|why| failed("settle", &why))?;
            egress.check().map_err(|why| failed("check", &why))?;
            next.set("");
            wire.say("settled", "settled");
        }
    }
    Ok(())
}

/// The probe: the agent of the units the tests below start. Not a test of its own: it acts
/// only in such a unit (where `JS_HANDOVER_PROBE_DIR` is set), and elsewhere it says so and
/// passes, since the job that runs the ignored tests runs it too.
#[test]
#[ignore = "the probe of the hand-over tests: started by them in a unit of its own"]
fn probe_of_the_agent() {
    let Some(dir) = std::env::var_os(PROBE_DIR) else {
        eprintln!("{PROBE_DIR} is not set: this runs only as the probe of a unit");
        return;
    };
    let wire = Wire {
        dir: PathBuf::from(dir),
    };
    let name = std::env::var(PROBE_ROLE).unwrap_or_default();
    let role = Role::parse(&name).unwrap_or_else(|| panic!("{PROBE_ROLE} is {name:?}"));
    let pid: u32 = fs::read_to_string(wire.dir.join("holder"))
        .expect("read the holder's pid")
        .trim()
        .parse()
        .expect("a pid");
    // The pipe the test waits on first.
    let next = Cell::new(if matches!(role, Role::Second) {
        "adopted"
    } else if matches!(role, Role::SettleOnly) {
        "settled"
    } else {
        "up"
    });
    if let Err(why) = play(role, pid, &wire, &next) {
        if !next.get().is_empty() {
            wire.say(next.get(), &format!("failed: {why}"));
        }
        panic!("the probe failed: {why}");
    }
}

/// A transient unit that runs the probe; stopped when dropped.
struct Unit {
    name: String,
}

impl Unit {
    /// Starts the probe as `role` in a unit with the packaged unit's kill mode and delegation.
    fn start(label: &str, role: Role, dir: &Path) -> Self {
        let name = format!("js-handover-{label}-{}-{}", role.name(), std::process::id());
        let exe = std::env::current_exe().expect("this test binary");
        let set = |key: &str, value: &str| format!("--property={key}={value}");
        let output = run(
            &tool("systemd-run"),
            &[
                "--quiet",
                &format!("--unit={name}"),
                &set("Delegate", &packaged("Delegate")),
                &set("DelegateSubgroup", &packaged("DelegateSubgroup")),
                &set("KillMode", &packaged("KillMode")),
                // A stop that does not wait the packaged drain's bound before its SIGKILL.
                &set("TimeoutStopSec", "20"),
                &format!("--setenv={PROBE_DIR}={}", dir.display()),
                &format!("--setenv={PROBE_ROLE}={}", role.name()),
                "--",
                exe.to_str().expect("a path in UTF-8"),
                "--exact",
                "handover::probe_of_the_agent",
                "--include-ignored",
                "--test-threads=1",
            ],
        );
        assert!(output.status.success(), "systemd-run: {}", text(&output));
        Self { name }
    }

    /// `systemctl stop`: returns when the probe has exited.
    fn stop(&self) {
        let output = run(&tool("systemctl"), &["stop", &self.name]);
        assert!(output.status.success(), "systemctl stop: {}", text(&output));
    }

    /// Whether the unit has a process running.
    fn running(&self) -> bool {
        let output = run(&tool("systemctl"), &["is-active", &self.name]);
        matches!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "active" | "activating" | "deactivating"
        )
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        let systemctl = tool("systemctl");
        let _ = run(&systemctl, &["stop", &self.name]);
        let _ = run(&systemctl, &["reset-failed", &self.name]);
    }
}

/// Removes what a run of the agent leaves on the host when the test is over, however it ended,
/// and before it starts: a start keeps what it finds, so a test begins with none.
struct Clean;

impl Clean {
    fn new() -> Self {
        let this = Self;
        this.sweep();
        this
    }

    fn sweep(&self) {
        let ip = tool("ip");
        for link in links("jsv") {
            let _ = run(&ip, &["link", "del", &link]);
        }
        for link in links("jsp") {
            if link
                .strip_prefix("jsp")
                .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()))
            {
                let _ = run(&ip, &["link", "del", &link]);
            }
        }
        clear_tables();
    }
}

impl Drop for Clean {
    fn drop(&mut self) {
        self.sweep();
    }
}

/// A listener at the allowed endpoint in the far namespace that says `pong` to every
/// connection and then echoes what the connection sends, until it closes: a long-lived
/// connection through the job's wiring shows whether the wiring was ever cut.
struct Echo {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Echo {
    fn start(netns: &File, addr: SocketAddr) -> Self {
        let netns = netns.try_clone().expect("clone the namespace file");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (ready_tx, ready_rx) = channel();
        let thread = std::thread::spawn(move || {
            move_into_link_name_space(netns.as_fd(), Some(LinkNameSpaceType::Network))
                .expect("setns");
            let listener = TcpListener::bind(addr)
                .unwrap_or_else(|why| panic!("cannot listen on {addr}: {why}"));
            listener.set_nonblocking(true).expect("nonblocking");
            let _ = ready_tx.send(());
            let mut handlers = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let flag = flag.clone();
                        handlers.push(std::thread::spawn(move || echo(stream, &flag)));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            for handler in handlers {
                let _ = handler.join();
            }
        });
        ready_rx.recv().expect("the echo thread bound its listener");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

/// Says `pong` on `stream` and echoes what it gets until it closes or `stop` is set.
fn echo(mut stream: TcpStream, stop: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    if stream.write_all(b"pong\n").is_err() {
        return;
    }
    let mut buffer = [0u8; 256];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => {
                if stream.write_all(&buffer[..n]).is_err() {
                    return;
                }
            }
            Err(why)
                if matches!(
                    why.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A connection from the holder's namespace to the echo listener, opened once and used in
/// every phase.
struct Conn {
    reader: BufReader<TcpStream>,
}

impl Conn {
    fn open(netns: &File) -> Self {
        let stream = in_netns(netns, || {
            TcpStream::connect_timeout(&far::ALLOWED, Duration::from_secs(3))
        })
        .expect("connect to the allowed endpoint");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("a read timeout");
        let mut conn = Self {
            reader: BufReader::new(stream),
        };
        assert_eq!(conn.line(), Ok("pong".to_owned()));
        conn
    }

    /// The next line the far end sent, or why there is none.
    fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("closed".to_owned()),
            Ok(_) => Ok(line.trim_end().to_owned()),
            Err(why) => Err(format!("{:?}: {why}", why.kind())),
        }
    }

    /// Sends `word` and reads what comes back: the echo of it, or why not.
    fn round(&mut self, word: &str) -> Result<String, String> {
        self.reader
            .get_mut()
            .write_all(format!("{word}\n").as_bytes())
            .map_err(|why| format!("{:?}: {why}", why.kind()))?;
        self.line()
    }
}

/// A short connection from `netns` to the allowed endpoint: what the far end says, as soon as
/// it has said a line (the echo listener does not close).
fn pong(netns: &File, to: SocketAddr) -> Reach {
    in_netns(netns, move || {
        match TcpStream::connect_timeout(&to, Duration::from_secs(3)) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut said = String::new();
                let _ = BufReader::new(stream).read_line(&mut said);
                Reach::Connected(said)
            }
            Err(why) => Reach::Failed(format!("{:?}: {why}", why.kind())),
        }
    })
}

/// The interface index of the host's interface `name`.
fn ifindex(name: &str) -> u64 {
    let json: Vec<Value> =
        serde_json::from_str(&ip(&["-j", "link", "show", name])).expect("ip json");
    json[0]["ifindex"].as_u64().expect("an ifindex")
}

/// What a hand-over test stands on: the host, the far host with its listeners, the holder
/// standing for the container, and the directory of the pipes to the probe.
struct Rig {
    // Dropped in this order: the holder and the listeners first, the far host, then what the
    // agent left on the host, and the lock last.
    holder: Holder,
    _job_port: Server,
    forbidden: [Server; 3],
    _echo: Echo,
    far: Far,
    _clean: Clean,
    _serial: MutexGuard<'static, ()>,
    wire: Wire,
    label: &'static str,
}

impl Rig {
    fn new(label: &'static str) -> Self {
        let serial = serial();
        let clean = Clean::new();
        let far = Far::new();
        let echo = Echo::start(&far.ns, far::ALLOWED);
        let forbidden = [far::OTHER_HOST, far::OTHER_PORT, far::METADATA]
            .map(|addr| Server::tcp(&far.ns, addr));
        let holder = Holder::new();
        let job_port = Server::tcp(
            &holder.ns,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, JOB_PORT)),
        );
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("handover-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make the probe's directory");
        for name in ["up", "adopted", "settle", "settled", "release", "released"] {
            fifo(&dir.join(name));
        }
        fs::write(dir.join("holder"), holder.pid().to_string()).expect("write the holder's pid");
        Self {
            holder,
            _job_port: job_port,
            forbidden,
            _echo: echo,
            far,
            _clean: clean,
            _serial: serial,
            wire: Wire { dir },
            label,
        }
    }

    /// The probe as `role`, in a unit of its own.
    fn unit(&self, role: Role) -> Unit {
        Unit::start(self.label, role, &self.wire.dir)
    }

    /// Asserts that the job is wired as the first agent left it, in `phase`: the host has the
    /// same interface (`ifindex`) and set member, the allowed endpoint answers from the
    /// holder, nothing else does, the declared port reaches the holder from the far host, and
    /// the connection opened before the stop still carries data.
    fn assert_wired(&self, conn: &mut Conn, ifindex_before: u64, phase: &str) {
        assert_eq!(links("jsv"), ["jsv0"], "{phase}: the host's interface");
        assert_eq!(ifindex("jsv0"), ifindex_before, "{phase}: the same veth");
        assert_eq!(wired(), ["jsv0"], "{phase}: the set member");
        let allowed = pong(&self.holder.ns, far::ALLOWED);
        assert!(
            allowed.connected(),
            "{phase}: the allowed endpoint: {allowed:?}"
        );
        for refused in [far::OTHER_HOST, far::OTHER_PORT, far::METADATA] {
            let reached = tcp(&self.holder.ns, refused);
            assert!(
                matches!(reached, Reach::Failed(_)),
                "{phase}: {refused} answered: {reached:?}"
            );
        }
        let forwarded = tcp(&self.far.ns, host_port());
        assert!(
            forwarded.connected(),
            "{phase}: the declared port: {forwarded:?}"
        );
        assert_eq!(
            conn.round(phase),
            Ok(phase.to_owned()),
            "{phase}: the connection opened before the stop"
        );
        self.assert_nothing_forbidden(phase);
    }

    /// Asserts that no forbidden listener has seen a connection.
    fn assert_nothing_forbidden(&self, phase: &str) {
        let counts: Vec<u64> = self.forbidden.iter().map(Server::accepted).collect();
        assert_eq!(
            counts,
            [0, 0, 0],
            "{phase}: a forbidden listener was reached"
        );
    }

    /// The names of the records in the store.
    fn records(&self) -> Vec<String> {
        let dir = state_root(&self.wire.dir).join("egress");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("list {}: {e}", dir.display()))
            .map(|entry| {
                entry
                    .expect("a directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// Asserts that the store holds the run's record and nothing else, in `phase`.
    fn assert_recorded(&self, phase: &str) {
        assert_eq!(self.records(), [RECORD], "{phase}: the store's records");
    }

    /// Asserts that the store holds no record, in `phase`.
    fn assert_unrecorded(&self, phase: &str) {
        assert_eq!(
            self.records(),
            Vec::<String>::new(),
            "{phase}: the store's records"
        );
    }

    /// Asserts that the job has lost its network, in `phase`: no interface or member on the
    /// host, none in the holder, no connection to the allowed endpoint and no declared port; the
    /// holder's process is still there. The old connection is not read: that would wait for a
    /// timeout to pass, and the holder having no `eth0` is what cuts it.
    fn assert_cut(&self, phase: &str) {
        assert!(links("jsv").is_empty(), "{phase}: {:?}", links("jsv"));
        assert!(wired().is_empty(), "{phase}: {:?}", wired());
        let inside = ip_in(&self.holder.ns, &["link", "show", "dev", "eth0"]);
        assert!(!inside.status.success(), "{phase}: the holder has eth0");
        let allowed = pong(&self.holder.ns, far::ALLOWED);
        assert!(
            !allowed.connected(),
            "{phase}: still reaches the endpoint: {allowed:?}"
        );
        let forwarded = tcp(&self.far.ns, host_port());
        assert!(
            !forwarded.connected(),
            "{phase}: the declared port: {forwarded:?}"
        );
        assert!(
            Path::new(&format!("/proc/{}", self.holder.pid())).exists(),
            "{phase}: the holder's process went"
        );
        self.assert_nothing_forbidden(phase);
    }

    /// Starts the first agent as `role`, which wires the holder, and opens the long-lived
    /// connection; returns the unit, the connection and the veth's interface index.
    fn first_agent(&self, role: Role) -> (Unit, Conn, u64) {
        let first = self.unit(role);
        assert_eq!(self.wire.heard("up"), "up");
        let index = ifindex("jsv0");
        let conn = Conn::open(&self.holder.ns);
        (first, conn, index)
    }
}

/// Asserts that a job's egress survives a hand-over, phase by phase, in a real unit. The first
/// agent wires the holder with a declared port and leaves it on `SIGTERM`; the unit's stop
/// returns, no agent runs; the second agent, in a unit of its own, adopts the wiring
/// from its record in the store (no pid) and settles. In each phase (before the stop, with no agent, after the adopt, after the settle)
/// the allowed endpoint answers from the holder and another host, another port and the
/// metadata address do not, the declared port reaches the holder, a connection opened before
/// the stop still echoes (the same veth, conntrack entry and masquerade binding), the host has
/// the same interface index and set member, the store holds the run's record, and no forbidden
/// listener counted a connection.
/// The second agent's firewall checks as loaded after its settle, and releasing the run takes
/// the interface and member down, removes the record, and the holder loses its network. Catches a stop that takes
/// the wiring down, a start that removes or reloads it unfiltered, an adoption that rewires the
/// job (another interface index), and a settle that removes a run's wiring.
#[test]
#[ignore = "needs root, ip, nft, unshare, systemd: see the module docs"]
fn a_job_keeps_its_egress_across_a_hand_over() {
    let rig = Rig::new("keep");
    let (first, mut conn, index) = rig.first_agent(Role::FirstLeave);
    rig.assert_wired(&mut conn, index, "before-the-stop");
    rig.assert_recorded("before-the-stop");

    first.stop();
    assert!(!first.running(), "an agent still runs");
    rig.assert_wired(&mut conn, index, "no-agent");
    rig.assert_recorded("no-agent");

    let second = rig.unit(Role::Second);
    assert_eq!(rig.wire.heard("adopted"), "adopted");
    rig.assert_wired(&mut conn, index, "after-the-adopt");
    rig.assert_recorded("after-the-adopt");

    rig.wire.say("settle", "settle");
    assert_eq!(rig.wire.heard("settled"), "settled");
    rig.assert_wired(&mut conn, index, "after-the-settle");
    rig.assert_recorded("after-the-settle");

    rig.wire.say("release", "release");
    assert_eq!(rig.wire.heard("released"), "released");
    rig.assert_cut("after-the-release");
    rig.assert_unrecorded("after-the-release");
    drop(second);
}

/// Asserts that the test can see a cut: the first agent releases the wiring on `SIGTERM`
/// instead of leaving it (what the agent did before a hand-over could carry an egress
/// ceiling). After the stop the host has no interface and no set member, the store has no
/// record, the holder has no `eth0`, the allowed endpoint and the declared port do not answer.
/// Catches a test whose phases would pass on a cut wiring, and a release that leaves its record.
#[test]
#[ignore = "needs root, ip, nft, unshare, systemd: see the module docs"]
fn a_job_loses_its_egress_when_the_first_agent_releases_it_at_the_stop() {
    let rig = Rig::new("cut");
    let (first, mut conn, index) = rig.first_agent(Role::FirstDrop);
    rig.assert_wired(&mut conn, index, "before-the-stop");
    rig.assert_recorded("before-the-stop");
    first.stop();
    assert!(!first.running(), "an agent still runs");
    rig.assert_cut("after-the-release-at-the-stop");
    rig.assert_unrecorded("after-the-release-at-the-stop");
}

/// Asserts that wiring no run takes over is removed by the settle: the first agent leaves the
/// wiring and its record, and the second agent starts (which keeps what the record names),
/// adopts nothing and settles. Before the second start the wiring answers as it did and the
/// record is there; after the settle the host has no interface, no set member and no
/// declared-port rule, the record file is gone, the holder has lost `eth0` and its network, its
/// process is untouched, and the second agent's firewall checks as loaded. Catches a settle
/// that leaves a kept interface, member, forward or record behind, and a start that removes
/// what it keeps before a run can take it over.
#[test]
#[ignore = "needs root, ip, nft, unshare, systemd: see the module docs"]
fn a_leftover_no_run_takes_over_is_removed_by_the_settle() {
    let rig = Rig::new("settle");
    let (first, mut conn, index) = rig.first_agent(Role::FirstLeave);
    first.stop();
    assert!(!first.running(), "an agent still runs");
    rig.assert_wired(&mut conn, index, "no-agent");
    rig.assert_recorded("no-agent");

    let second = rig.unit(Role::SettleOnly);
    assert_eq!(rig.wire.heard("settled"), "settled");
    rig.assert_cut("after-the-settle");
    rig.assert_unrecorded("after-the-settle");
    drop(second);
}
