//! Tests of [`SystemDriver`] over a scripted host: the helper is a script of events, the
//! guest is a thread on the other end of `exec.sock` (the real guest agent, or a scripted
//! one for the paths a real agent does not take).

use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::mpsc;
use std::thread::JoinHandle;

use js_guest::server::{Agent, Config};
use js_guest::wire::{Exit, GuestMsg, HostMsg, Refusal, Usage, VERSION, read_frame, write_frame};
use js_testkit::dir::TestDir;
use serde_json::{Value, json};

use super::*;
use crate::fake::{FakeClock, work_with_base};
use crate::system::{Clock, OfflineFile, Signature};

/// What a scripted helper does when asked to stop.
#[derive(Clone, Copy)]
enum Mode {
    /// Exits when asked to shut down.
    Obeys,
    /// Exits only once killed.
    NeedsKill,
    /// Never exits.
    Immortal,
}

/// One scripted helper: the polls it answers, how it stops, and what its log says.
struct Start {
    events: Vec<Poll>,
    mode: Mode,
    diag: Option<String>,
}

impl Start {
    fn new(events: Vec<Poll>) -> Self {
        Self {
            events,
            mode: Mode::Obeys,
            diag: None,
        }
    }

    fn booted() -> Self {
        Self::new(vec![
            Poll::Quiet,
            Poll::Event(Event::State {
                state: "running".to_owned(),
                ms: 1,
            }),
            Poll::Event(Event::Started { ms: 2 }),
        ])
    }
}

struct ScriptProc {
    events: VecDeque<Poll>,
    mode: Mode,
    diag: Option<String>,
    log: Arc<Mutex<Vec<&'static str>>>,
    shutdown: bool,
    killed: bool,
}

impl VmProcess for ScriptProc {
    fn pid(&self) -> u32 {
        7
    }
    fn poll(&mut self, _wait: Duration) -> Poll {
        self.events.pop_front().unwrap_or(Poll::Quiet)
    }
    fn kill9(&mut self) {
        lock(&self.log).push("kill9");
        self.killed = true;
    }
    fn request_shutdown(&mut self) {
        lock(&self.log).push("shutdown");
        self.shutdown = true;
    }
    fn request_stop(&mut self) {
        lock(&self.log).push("stop");
    }
    fn wait_gone(&mut self, _wait: Duration) -> bool {
        match self.mode {
            Mode::Obeys => self.shutdown || self.killed,
            Mode::NeedsKill => self.killed,
            Mode::Immortal => false,
        }
    }
    fn diagnostics(&self) -> Option<String> {
        self.diag.clone()
    }
}

/// A host whose files are really copied and whose helpers are scripted. It is `Sync`,
/// which the fake of the probes is not.
struct HostFake {
    clock: Arc<FakeClock>,
    starts: Mutex<VecDeque<Result<Start, String>>>,
    specs: Mutex<Vec<VmSpec>>,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl HostFake {
    fn new() -> Self {
        Self {
            clock: Arc::new(FakeClock::default()),
            starts: Mutex::new(VecDeque::new()),
            specs: Mutex::new(Vec::new()),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl System for HostFake {
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    fn clonefile(&self, src: &Path, dst: &Path) -> io::Result<()> {
        std::fs::copy(src, dst).map(|_| ())
    }
    fn clear_immutable(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
    fn new_machine_identifier(&self) -> Result<Vec<u8>, String> {
        Ok(b"fresh-id".to_vec())
    }
    fn start_vm(&self, spec: &VmSpec, run_as: Option<u32>) -> Result<Box<dyn VmProcess>, String> {
        assert_eq!(run_as, None);
        lock(&self.specs).push(spec.clone());
        let start = lock(&self.starts)
            .pop_front()
            .unwrap_or_else(|| Ok(Start::booted()))?;
        Ok(Box::new(ScriptProc {
            events: start.events.into(),
            mode: start.mode,
            diag: start.diag,
            log: self.log.clone(),
            shutdown: false,
            killed: false,
        }))
    }
    fn purge_cache(&self) -> Result<(), String> {
        Err("purge".to_owned())
    }
    fn uid(&self) -> u32 {
        1
    }
    fn ppid(&self) -> u32 {
        2
    }
    fn launchd_context(&self) -> Option<String> {
        None
    }
    fn console_uid(&self) -> Option<u32> {
        None
    }
    fn host_facts(&self) -> Value {
        json!({})
    }
    fn signature(&self) -> Result<Signature, String> {
        Err("signature".to_owned())
    }
    fn offline_write(&self, _disk: &Path, _files: &[OfflineFile]) -> Result<Value, String> {
        Err("write".to_owned())
    }
    fn offline_stage(&self, _disk: &Path, _plan: &crate::stage::Plan) -> Result<Value, String> {
        Err("stage".to_owned())
    }
    fn offline_harvest(&self, _disk: &Path, _out: &Path) -> Result<Value, String> {
        Err("harvest".to_owned())
    }
    fn process_table(&self) -> Result<String, String> {
        Err("table".to_owned())
    }
    fn fetch(&self, _url: &str, _dest: &Path) -> Result<(), String> {
        Err("fetch".to_owned())
    }
    fn install_image(
        &self,
        _ipsw: &Path,
        _out: &Path,
        _sizing: &crate::bundle::Sizing,
    ) -> Result<Value, String> {
        Err("install".to_owned())
    }
    fn run_vm_helper(&self, _spec: &VmSpec) -> Result<(), String> {
        Err("helper".to_owned())
    }
}

/// A driver over a store with a base image, and the VM it is for.
struct Rig {
    dir: TestDir,
    driver: Arc<SystemDriver<HostFake>>,
    vm: VmName,
}

impl Rig {
    fn new(name: &str) -> Self {
        Self::with_token(name, random_token)
    }

    fn with_token(name: &str, token: fn() -> io::Result<[u8; TOKEN_LEN]>) -> Self {
        let dir = work_with_base(name);
        let mut driver =
            SystemDriver::new(HostFake::new(), dir.path().to_owned()).with_timing(DriverTiming {
                ready: Duration::from_secs(1),
                tick: Duration::from_millis(10),
                stop_grace: Duration::from_secs(1),
            });
        driver.token = token;
        Self {
            dir,
            driver: Arc::new(driver),
            vm: VmName::new("run-1").expect("name"),
        }
    }

    fn script(&self, start: Result<Start, String>) {
        lock(&self.driver.system.starts).push_back(start);
    }

    fn run(&self) -> PathBuf {
        self.driver.run_dir(&self.vm)
    }

    fn size() -> VmSize {
        VmSize {
            cpus: 4,
            memory_mib: 8192,
        }
    }

    /// Clones and boots the VM.
    fn booted(&self) {
        self.driver.clone_base(&self.vm).expect("clone");
        self.driver.boot(&self.vm, Self::size()).expect("boot");
    }

    fn error_of<T: Debug>(&self, result: Result<T, VmError>) -> String {
        result.unwrap_err().to_string()
    }
}

/// The guest agent for the VM in `run`, serving the connection it makes to `exec.sock`.
/// The connection is made before this returns (the driver's listener holds it until
/// `wait_ready` accepts), so that no test depends on which thread runs first.
fn spawn_agent(run: PathBuf) -> JoinHandle<()> {
    let stream = UnixStream::connect(run.join(SOCKET)).expect("connect");
    std::thread::spawn(move || {
        let text = std::fs::read_to_string(run.join(INPUTS).join(TOKEN_FILE)).expect("token");
        let token: [u8; TOKEN_LEN] = hex::decode(text.trim())
            .expect("hex")
            .try_into()
            .expect("32 bytes");
        let mut agent = Agent::new(Config {
            token,
            inputs: run.join(INPUTS),
            outputs: run.join(OUTPUTS),
            session: "Test".to_owned(),
            hello_timeout: Duration::from_secs(10),
        });
        agent.serve_connection(stream).expect("served");
    })
}

/// A guest that connects to `exec.sock` and hands the connection to `script`.
fn spawn_script(run: PathBuf, script: impl FnOnce(UnixStream) + Send + 'static) -> JoinHandle<()> {
    let stream = UnixStream::connect(run.join(SOCKET)).expect("connect");
    std::thread::spawn(move || script(stream))
}

fn recv(stream: &mut UnixStream) -> HostMsg {
    HostMsg::decode(&read_frame(stream).expect("frame")).expect("host message")
}

fn send(stream: &mut UnixStream, msg: &GuestMsg) {
    write_frame(stream, &msg.encode()).expect("send");
}

/// Reads `Hello` and answers `Ready`.
fn handshake(stream: &mut UnixStream) {
    assert!(matches!(
        recv(stream),
        HostMsg::Hello { version, .. } if version == VERSION
    ));
    send(
        stream,
        &GuestMsg::Ready {
            version: VERSION,
            session: "Test".to_owned(),
        },
    );
}

fn exited(end: End) -> GuestMsg {
    GuestMsg::Exited(Exit {
        end,
        usage: Usage::default(),
        outputs: Vec::new(),
        stragglers: false,
    })
}

fn shell(script: &str) -> ExecRequest {
    ExecRequest {
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        timeout_ms: 30_000,
    }
}

/// Asserts the whole life of a VM against the real guest agent: the clone, the spec the
/// helper is started with, the token the guest reads, the command's status and output, the
/// stop and the removal. Catches a boot that shares the wrong directories or tags, a token
/// the guest cannot read, an exec that loses the status or the output, and a VM left behind.
#[test]
fn a_vm_is_cloned_booted_run_collected_stopped_and_destroyed() {
    let r = Rig::new("lease-life");
    let (d, vm) = (&r.driver, &r.vm);
    d.clone_base(vm).expect("clone");
    assert_eq!(d.leftovers(), Ok(vec![vm.clone()]));
    assert_eq!(
        std::fs::read(r.run().join("bundle").join(bundle::MACHINE_ID)).expect("id"),
        b"fresh-id"
    );
    assert_eq!(
        std::fs::read_to_string(r.run().join("bundle").join(bundle::DISK)).expect("disk"),
        "contents of disk.img"
    );
    d.boot(vm, Rig::size()).expect("boot");

    let specs = lock(&d.system.specs).clone();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!(
        (
            spec.bundle.clone(),
            spec.cpus,
            spec.memory_mib,
            spec.graphics
        ),
        (r.run().join("bundle"), 4, 8192, false)
    );
    assert_eq!(
        spec.tagged,
        vec![
            TaggedShare {
                tag: "kbfinputs".to_owned(),
                path: r.run().join("inputs"),
                multiple_name: None,
                read_only: true,
            },
            TaggedShare {
                tag: "kbfoutputs".to_owned(),
                path: r.run().join("outputs"),
                multiple_name: None,
                read_only: false,
            },
        ]
    );
    assert_eq!(
        spec.exec_bridge,
        Some(ExecBridge {
            port: 5002,
            socket: r.run().join("exec.sock"),
        })
    );
    assert_eq!(
        (
            spec.vsock_ports.len(),
            spec.results_dir.clone(),
            spec.automount_dir.clone()
        ),
        (0, None, None)
    );
    let token = std::fs::read_to_string(r.run().join("inputs").join(TOKEN_FILE)).expect("token");
    assert_eq!(token.len(), 65);
    assert!(token.trim().bytes().all(|b| b.is_ascii_hexdigit()));

    let guest = spawn_agent(r.run());
    d.wait_ready(vm).expect("ready");
    let end = d
        .run_command(vm, &shell("printf out; printf err >&2; exit 3"))
        .expect("exec");
    assert_eq!(end, ExecEnd::Exited(3));
    guest.join().expect("guest");

    assert_eq!(
        d.collect(vm, 1000),
        Ok(Collected {
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            truncated: false,
        })
    );
    assert_eq!(d.stop(vm), Ok(()));
    assert_eq!(*lock(&d.system.log), ["shutdown"]);
    assert_eq!(d.destroy(vm), Ok(()));
    assert!(!r.run().exists());
    assert_eq!(d.leftovers(), Ok(vec![]));
    // The name is free again, and the new VM is a new one.
    assert_eq!(d.clone_base(vm), Ok(()));
    assert_eq!(
        d.run_command(vm, &shell("exit 0"))
            .map_err(|e| e.to_string())
            .unwrap_err(),
        "VM run-1: exec failed: the guest agent is not connected (not ready, or the command already ran)"
    );
}

/// Asserts each way the guest reports a command ending reaches the caller as the same
/// [`ExecEnd`], and that the guest receives the request as asked: the program, the whole
/// environment, the timeout, the root of the inputs share as the directory and no outputs.
/// Catches an end mapped to another, or a request field dropped.
#[test]
fn each_end_the_guest_reports_is_the_end_exec_returns() {
    let cases = [
        (End::Exited(0), ExecEnd::Exited(0)),
        (End::Signaled(9), ExecEnd::Signaled(9)),
        (End::TimedOut, ExecEnd::TimedOut),
        (End::Killed, ExecEnd::Killed),
    ];
    for (i, (end, want)) in cases.into_iter().enumerate() {
        let r = Rig::new(&format!("lease-ends-{i}"));
        r.booted();
        let guest = spawn_script(r.run(), move |mut s| {
            handshake(&mut s);
            assert_eq!(
                recv(&mut s),
                HostMsg::Run(js_guest::wire::RunRequest {
                    argv: vec!["prog".to_owned(), "arg".to_owned()],
                    env: vec![("A".to_owned(), "b".to_owned())],
                    cwd: String::new(),
                    timeout_ms: 1234,
                    outputs: vec![],
                })
            );
            send(&mut s, &GuestMsg::Started { pid: 5 });
            send(&mut s, &exited(end));
        });
        r.driver.wait_ready(&r.vm).expect("ready");
        let request = ExecRequest {
            argv: vec!["prog".to_owned(), "arg".to_owned()],
            env: vec![("A".to_owned(), "b".to_owned())],
            timeout_ms: 1234,
        };
        assert_eq!(r.driver.run_command(&r.vm, &request), Ok(want));
        guest.join().expect("guest");
    }
}

/// Asserts the guest's refusal of a request, and a connection that breaks after the
/// command started, are `exec` errors that name the step, and that a second `exec` is
/// refused because the VM runs at most one command. Catches a refusal reported as a
/// success and a command run twice.
#[test]
fn a_refused_request_a_broken_connection_and_a_second_exec_are_errors() {
    let r = Rig::new("lease-exec-errors");
    r.booted();
    let guest = spawn_script(r.run(), |mut s| {
        handshake(&mut s);
        recv(&mut s);
        send(
            &mut s,
            &GuestMsg::Refused {
                reason: Refusal::BadRequest,
                detail: "nope".to_owned(),
            },
        );
    });
    r.driver.wait_ready(&r.vm).expect("ready");
    assert_eq!(
        r.error_of(r.driver.run_command(&r.vm, &shell("true"))),
        "VM run-1: exec failed: the guest refused (BadRequest): nope"
    );
    guest.join().expect("guest");
    assert_eq!(
        r.error_of(r.driver.run_command(&r.vm, &shell("true"))),
        "VM run-1: exec failed: the guest agent is not connected (not ready, or the command already ran)"
    );

    let r = Rig::new("lease-exec-broken");
    r.booted();
    let guest = spawn_script(r.run(), |mut s| {
        handshake(&mut s);
        recv(&mut s);
        send(&mut s, &GuestMsg::Started { pid: 5 });
    });
    r.driver.wait_ready(&r.vm).expect("ready");
    let err = r.error_of(r.driver.run_command(&r.vm, &shell("true")));
    assert!(err.starts_with("VM run-1: exec failed: "), "{err}");
    guest.join().expect("guest");
}

/// Asserts that a VM killed before its command starts runs nothing: `exec` returns
/// `Killed` at once, without a guest and without being ready. Catches a killed VM that
/// still runs its command.
#[test]
fn exec_on_a_killed_vm_returns_killed_and_runs_nothing() {
    let r = Rig::new("lease-killed-first");
    r.driver.clone_base(&r.vm).expect("clone");
    r.driver.kill(&r.vm);
    assert_eq!(
        r.driver.run_command(&r.vm, &shell("true")),
        Ok(ExecEnd::Killed)
    );
    assert_eq!(r.driver.stop(&r.vm), Ok(()));
}

/// Asserts that `kill` from another thread while the command runs sends the guest `Kill`
/// and `exec` returns `Killed`. Catches a kill that waits for the command, or never
/// reaches the guest.
#[test]
fn kill_while_the_command_runs_ends_it() {
    let r = Rig::new("lease-kill-running");
    r.booted();
    let (started_tx, started_rx) = mpsc::channel();
    let guest = spawn_script(r.run(), move |mut s| {
        handshake(&mut s);
        recv(&mut s);
        send(&mut s, &GuestMsg::Started { pid: 5 });
        started_tx.send(()).expect("signal");
        assert_eq!(recv(&mut s), HostMsg::Kill);
        send(&mut s, &exited(End::Killed));
    });
    r.driver.wait_ready(&r.vm).expect("ready");
    let (d, vm) = (r.driver.clone(), r.vm.clone());
    let exec = std::thread::spawn(move || d.run_command(&vm, &shell("long-running")));
    started_rx.recv().expect("started");
    r.driver.kill(&r.vm);
    assert_eq!(exec.join().expect("exec"), Ok(ExecEnd::Killed));
    guest.join().expect("guest");
}

/// Asserts that a `kill` that lands while the command is being started is repeated once
/// the guest has started it: the guest sees `Kill` twice, the first from `kill` itself and
/// the second from `exec`. Catches a kill lost because the guest ignores `Kill` before
/// the command runs.
#[test]
fn a_kill_that_races_the_start_is_repeated_after_it() {
    let r = Rig::new("lease-kill-race");
    r.booted();
    let (d, vm) = (r.driver.clone(), r.vm.clone());
    let guest = spawn_script(r.run(), move |mut s| {
        handshake(&mut s);
        recv(&mut s);
        d.kill(&vm);
        send(&mut s, &GuestMsg::Started { pid: 5 });
        assert_eq!(recv(&mut s), HostMsg::Kill);
        assert_eq!(recv(&mut s), HostMsg::Kill);
        send(&mut s, &exited(End::Killed));
    });
    r.driver.wait_ready(&r.vm).expect("ready");
    assert_eq!(
        r.driver.run_command(&r.vm, &shell("long-running")),
        Ok(ExecEnd::Killed)
    );
    guest.join().expect("guest");
}

/// Asserts that killing a VM nobody cloned does nothing, and that every step names a VM
/// that was never cloned. Catches a panic on an unknown name.
#[test]
fn a_vm_that_was_never_cloned_is_unknown_to_every_step() {
    let r = Rig::new("lease-unknown");
    let (d, vm) = (&r.driver, &r.vm);
    d.kill(vm);
    for (text, step) in [
        (r.error_of(d.boot(vm, Rig::size())), "boot"),
        (r.error_of(d.wait_ready(vm)), "wait for the guest"),
        (r.error_of(d.run_command(vm, &shell("true"))), "exec"),
    ] {
        assert_eq!(
            text,
            format!("VM run-1: {step} failed: this driver made no clone of it")
        );
    }
    assert_eq!(d.stop(vm), Ok(()));
    assert_eq!(d.destroy(vm), Ok(()));
}

/// Asserts the failures of `clone_base`: a base that is not there leaves nothing, a name
/// that has a clone keeps it, and a store that cannot hold `runs` is an error. Catches a
/// half-made clone left behind and a second clone overwriting the first.
#[test]
fn clone_base_failures_leave_nothing_and_keep_an_existing_clone() {
    let r = Rig::new("lease-clone-fails");
    std::fs::remove_dir_all(r.dir.join("base")).expect("rm base");
    let err = r.error_of(r.driver.clone_base(&r.vm));
    assert!(err.starts_with("VM run-1: clone failed: "), "{err}");
    assert!(err.contains("is not a prepared base bundle"), "{err}");
    assert!(!r.run().exists());
    assert_eq!(r.driver.leftovers(), Ok(vec![]));

    let r = Rig::new("lease-clone-twice");
    r.driver.clone_base(&r.vm).expect("clone");
    std::fs::write(r.run().join("mine"), "x").expect("mark");
    let err = r.error_of(r.driver.clone_base(&r.vm));
    assert!(err.starts_with("VM run-1: clone failed: make "), "{err}");
    assert_eq!(
        std::fs::read_to_string(r.run().join("mine")).expect("kept"),
        "x"
    );

    let r = Rig::new("lease-clone-blocked");
    std::fs::write(r.dir.join("runs"), "").expect("file");
    let err = r.error_of(r.driver.clone_base(&r.vm));
    assert!(err.starts_with("VM run-1: clone failed: make "), "{err}");
}

fn no_token() -> io::Result<[u8; TOKEN_LEN]> {
    Err(io::Error::other("no entropy"))
}

/// Asserts each way `boot` fails is an error of the boot step, and that a VM boots once.
/// Catches a second helper started for one VM and a failure that leaves the VM half
/// booted.
#[test]
fn boot_failures_are_errors_of_the_boot_step() {
    let r = Rig::new("lease-boot-twice");
    r.booted();
    assert_eq!(
        r.error_of(r.driver.boot(&r.vm, Rig::size())),
        "VM run-1: boot failed: it is already booted"
    );
    assert_eq!(lock(&r.driver.system.specs).len(), 1);

    let r = Rig::new("lease-boot-share");
    r.driver.clone_base(&r.vm).expect("clone");
    std::fs::write(r.run().join("inputs"), "").expect("file");
    let err = r.error_of(r.driver.boot(&r.vm, Rig::size()));
    assert!(
        err.starts_with("VM run-1: boot failed: make the inputs share: "),
        "{err}"
    );

    let r = Rig::with_token("lease-boot-token", no_token);
    r.driver.clone_base(&r.vm).expect("clone");
    assert_eq!(
        r.error_of(r.driver.boot(&r.vm, Rig::size())),
        "VM run-1: boot failed: a token: no entropy"
    );

    let r = Rig::new("lease-boot-token-file");
    r.driver.clone_base(&r.vm).expect("clone");
    std::fs::create_dir_all(r.run().join("inputs").join(TOKEN_FILE)).expect("dir");
    let err = r.error_of(r.driver.boot(&r.vm, Rig::size()));
    assert!(
        err.starts_with("VM run-1: boot failed: write the token: "),
        "{err}"
    );

    // A socket path longer than `sun_path`.
    let r = Rig::new("lease-boot-socket");
    let long = VmName::new(&"a".repeat(MAX_NAME)).expect("name");
    r.driver.clone_base(&long).expect("clone");
    let err = r.error_of(r.driver.boot(&long, Rig::size()));
    assert!(err.contains("boot failed: listen on "), "{err}");

    let r = Rig::new("lease-boot-helper");
    r.script(Err("no helper".to_owned()));
    r.driver.clone_base(&r.vm).expect("clone");
    assert_eq!(
        r.error_of(r.driver.boot(&r.vm, Rig::size())),
        "VM run-1: boot failed: no helper"
    );
    assert_eq!(r.driver.stop(&r.vm), Ok(()));
}

const MAX_NAME: usize = js_vm_lease::MAX_NAME_LEN;

/// Asserts each way the wait for the guest fails names the reason: the helper ending (with
/// its log), the helper reporting an error, the VM never starting, the guest never
/// connecting, and a VM never booted. Catches a wait that hangs, or fails without saying
/// why.
#[test]
fn wait_ready_fails_when_the_vm_does_not_come_up() {
    let r = Rig::new("lease-ready-unbooted");
    r.driver.clone_base(&r.vm).expect("clone");
    assert_eq!(
        r.error_of(r.driver.wait_ready(&r.vm)),
        "VM run-1: wait for the guest failed: it was not booted"
    );

    let cases: Vec<(Start, &str)> = vec![
        (
            Start {
                diag: Some(" boom\n".to_owned()),
                ..Start::new(vec![Poll::Gone {
                    code: Some(1),
                    signal: None,
                }])
            },
            "the VM helper ended (code Some(1), signal None); helper log: boom",
        ),
        (
            Start::new(vec![Poll::Event(Event::Error {
                stage: "start".to_owned(),
                message: "limit".to_owned(),
                code: None,
            })]),
            "the VM helper failed at start: limit",
        ),
        (Start::new(vec![]), "the VM did not start within 1s"),
        (
            Start::new(vec![Poll::Event(Event::Started { ms: 1 })]),
            "the guest agent did not connect within 1s",
        ),
    ];
    for (i, (start, why)) in cases.into_iter().enumerate() {
        let r = Rig::new(&format!("lease-ready-{i}"));
        r.script(Ok(start));
        r.booted();
        assert_eq!(
            r.error_of(r.driver.wait_ready(&r.vm)),
            format!("VM run-1: wait for the guest failed: {why}")
        );
    }
}

/// Asserts a guest that refuses the token, and one that hangs up, fail the wait with the
/// guest's reason. Catches a handshake failure reported as ready.
#[test]
fn a_guest_that_refuses_the_token_or_hangs_up_is_not_ready() {
    let r = Rig::new("lease-token-refused");
    r.booted();
    let guest = spawn_script(r.run(), |mut s| {
        recv(&mut s);
        send(
            &mut s,
            &GuestMsg::Refused {
                reason: Refusal::Token,
                detail: "wrong token".to_owned(),
            },
        );
    });
    assert_eq!(
        r.error_of(r.driver.wait_ready(&r.vm)),
        "VM run-1: wait for the guest failed: the guest refused (Token): wrong token"
    );
    guest.join().expect("guest");

    let r = Rig::new("lease-hangup");
    r.booted();
    let guest = spawn_script(r.run(), |mut s| {
        recv(&mut s);
    });
    let err = r.error_of(r.driver.wait_ready(&r.vm));
    assert!(
        err.starts_with("VM run-1: wait for the guest failed: "),
        "{err}"
    );
    guest.join().expect("guest");
}

/// Asserts `collect` keeps the end of each stream, says when either was cut, reads a
/// missing file as empty, and reports a file it cannot read. Catches a tail taken from the
/// start, a truncation flag that misses stderr, and an unreadable file read as empty.
#[test]
fn collect_keeps_the_tail_of_each_stream() {
    let r = Rig::new("lease-collect");
    let outputs = r.run().join("outputs");
    std::fs::create_dir_all(&outputs).expect("outputs");
    let put = |name: &str, body: &str| std::fs::write(outputs.join(name), body).expect("write");
    let get = |tail| r.driver.collect(&r.vm, tail).expect("collect");

    assert_eq!(get(10), Collected::default());

    put("stdout", "0123456789");
    put("stderr", "ab");
    assert_eq!(
        get(4),
        Collected {
            stdout: b"6789".to_vec(),
            stderr: b"ab".to_vec(),
            truncated: true,
        }
    );
    put("stdout", "abc");
    put("stderr", "0123456789");
    assert_eq!(
        get(4),
        Collected {
            stdout: b"abc".to_vec(),
            stderr: b"6789".to_vec(),
            truncated: true,
        }
    );
    put("stdout", "ab");
    put("stderr", "cd");
    assert_eq!(
        get(2),
        Collected {
            stdout: b"ab".to_vec(),
            stderr: b"cd".to_vec(),
            truncated: false,
        }
    );

    std::fs::remove_file(outputs.join("stdout")).expect("rm");
    std::fs::create_dir(outputs.join("stdout")).expect("dir");
    let err = r.error_of(r.driver.collect(&r.vm, 4));
    assert!(
        err.starts_with("VM run-1: collect failed: read stdout: "),
        "{err}"
    );

    std::fs::remove_dir(outputs.join("stdout")).expect("rmdir");
    put("stdout", "ok");
    std::fs::remove_file(outputs.join("stderr")).expect("rm");
    std::fs::create_dir(outputs.join("stderr")).expect("dir");
    let err = r.error_of(r.driver.collect(&r.vm, 4));
    assert!(
        err.starts_with("VM run-1: collect failed: read stderr: "),
        "{err}"
    );

    std::fs::remove_dir_all(&outputs).expect("rm outputs");
    std::fs::write(&outputs, "").expect("file");
    let err = r.error_of(r.driver.collect(&r.vm, 4));
    assert!(
        err.starts_with("VM run-1: collect failed: read stdout: "),
        "{err}"
    );
}

/// Asserts `stop` leaves a VM that never booted alone, lets a guest that obeys shut down,
/// kills a helper that does not, reports one that survives (and tries again later), and
/// is a no-op once stopped. Catches a stop that kills a helper that was shutting down, a
/// hung helper reported as stopped, and a second stop acting twice.
#[test]
fn stop_asks_first_kills_second_and_reports_a_helper_that_survives() {
    let r = Rig::new("lease-stop-unbooted");
    r.driver.clone_base(&r.vm).expect("clone");
    assert_eq!(r.driver.stop(&r.vm), Ok(()));
    assert!(lock(&r.driver.system.log).is_empty());

    for (mode, log) in [
        (Mode::Obeys, vec!["shutdown"]),
        (Mode::NeedsKill, vec!["shutdown", "kill9"]),
    ] {
        let r = Rig::new("lease-stop-modes");
        r.script(Ok(Start {
            mode,
            ..Start::booted()
        }));
        r.booted();
        assert_eq!(r.driver.stop(&r.vm), Ok(()));
        assert_eq!(*lock(&r.driver.system.log), log);
        assert_eq!(r.driver.stop(&r.vm), Ok(()));
        assert_eq!(*lock(&r.driver.system.log), log);
    }

    let r = Rig::new("lease-stop-immortal");
    r.script(Ok(Start {
        mode: Mode::Immortal,
        ..Start::booted()
    }));
    r.booted();
    let want = "VM run-1: stop failed: the VM helper is still running after SIGKILL";
    assert_eq!(r.error_of(r.driver.stop(&r.vm)), want);
    assert_eq!(r.error_of(r.driver.stop(&r.vm)), want);
    assert_eq!(
        *lock(&r.driver.system.log),
        ["shutdown", "kill9", "shutdown", "kill9"]
    );
    // A clone of a VM that cannot be stopped is kept.
    assert_eq!(r.error_of(r.driver.destroy(&r.vm)), want);
    assert!(r.run().exists());
}

/// Asserts `destroy` removes the clone, the shares and the socket, stops a VM that still
/// runs first, accepts a VM with nothing left, and reports what it cannot remove. Catches
/// a clone left on disk and a running VM whose files are deleted.
#[test]
fn destroy_stops_the_vm_and_removes_everything_of_it() {
    let r = Rig::new("lease-destroy");
    assert_eq!(r.driver.destroy(&r.vm), Ok(()));

    r.booted();
    assert!(r.run().join("exec.sock").exists());
    assert_eq!(r.driver.destroy(&r.vm), Ok(()));
    assert_eq!(*lock(&r.driver.system.log), ["shutdown"]);
    assert!(!r.run().exists());
    assert_eq!(r.driver.leftovers(), Ok(vec![]));
    assert!(r.driver.lookup(&r.vm).is_none());

    // Something that is not a directory where the clone should be.
    std::fs::write(r.run(), "").expect("file");
    let err = r.error_of(r.driver.destroy(&r.vm));
    assert!(err.starts_with("VM run-1: destroy failed: "), "{err}");
}

/// Asserts `leftovers` lists the VM directories in order and nothing else, and says so
/// when the store cannot be listed. Catches a stray file or an odd name taken for a VM,
/// and a missing `runs` taken for a failure.
#[test]
fn leftovers_are_the_vm_directories_of_the_store() {
    let r = Rig::new("lease-leftovers");
    assert_eq!(r.driver.leftovers(), Ok(vec![]));
    let runs = r.dir.join("runs");
    std::fs::create_dir_all(&runs).expect("runs");
    for dir in ["b", "a", "bad name", ".hidden"] {
        std::fs::create_dir(runs.join(dir)).expect("dir");
    }
    std::fs::write(runs.join("c"), "").expect("file");
    let names: Vec<String> = r
        .driver
        .leftovers()
        .expect("list")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, ["a", "b"]);

    std::fs::remove_dir_all(&runs).expect("rm");
    std::fs::write(&runs, "").expect("file");
    let err = r.error_of(r.driver.leftovers());
    assert!(err.starts_with("the VM store cannot be listed: "), "{err}");
}

/// Asserts the token comes from the kernel's random source (32 bytes, not the same twice)
/// and that a source that is missing or short is an error. Catches a constant token and a
/// short read taken for a token.
#[test]
fn the_token_is_32_random_bytes_and_a_short_source_is_an_error() {
    let (a, b) = (random_token().expect("a"), random_token().expect("b"));
    assert_ne!(a, b);
    let dir = TestDir::under(std::env::temp_dir(), "js-vmm-lease-token");
    assert!(read_token(&dir.join("missing")).is_err());
    std::fs::write(dir.join("short"), [1u8; TOKEN_LEN - 1]).expect("short");
    assert!(read_token(&dir.join("short")).is_err());
    std::fs::write(dir.join("long"), [7u8; TOKEN_LEN + 3]).expect("long");
    assert_eq!(
        read_token(&dir.join("long")).expect("long"),
        [7u8; TOKEN_LEN]
    );
}

/// Asserts the default timing is the one the agent runs with, and the scripted host's
/// answers outside what the driver uses are plain. Catches a default changed by accident.
#[test]
fn the_default_timing_and_the_unused_host_calls() {
    let t = DriverTiming::default();
    assert_eq!(
        (t.ready, t.tick, t.stop_grace),
        (
            Duration::from_secs(300),
            Duration::from_millis(50),
            Duration::from_secs(60)
        )
    );
    let host = HostFake::new();
    let here = Path::new("/nowhere");
    host.clear_immutable(here).expect("clear");
    assert_eq!(host.purge_cache(), Err("purge".to_owned()));
    assert_eq!((host.uid(), host.ppid()), (1, 2));
    assert_eq!((host.launchd_context(), host.console_uid()), (None, None));
    assert_eq!(host.host_facts(), json!({}));
    assert_eq!(host.signature().unwrap_err(), "signature");
    assert_eq!(host.offline_write(here, &[]).unwrap_err(), "write");
    assert_eq!(
        host.offline_stage(here, &crate::stage::Plan::default())
            .unwrap_err(),
        "stage"
    );
    assert_eq!(host.offline_harvest(here, here).unwrap_err(), "harvest");
    assert_eq!(host.process_table().unwrap_err(), "table");
    assert_eq!(host.fetch("u", here).unwrap_err(), "fetch");
    let sizing = crate::bundle::Sizing {
        disk_gib: 1,
        cpus: None,
        memory_mib: None,
    };
    assert_eq!(
        host.install_image(here, here, &sizing).unwrap_err(),
        "install"
    );
    assert_eq!(
        host.run_vm_helper(&VmSpec::minimal(here.to_owned())),
        Err("helper".to_owned())
    );
    let mut proc = ScriptProc {
        events: VecDeque::new(),
        mode: Mode::Obeys,
        diag: None,
        log: host.log.clone(),
        shutdown: false,
        killed: false,
    };
    proc.request_stop();
    assert_eq!((proc.pid(), proc.diagnostics()), (7, None));
    assert_eq!(*lock(&host.log), ["stop"]);
}
