//! The real [`VmDriver`]: one VM per run, booted through a [`System`] (on macOS the signed
//! `js-vmm vm-run` helper), commands run in it through the guest agent (`js-guest`).
//!
//! # The store
//!
//! ```text
//! <store>/base/                the prepared base bundle (`js-vmm image base`)
//! <store>/runs/<name>/         one VM, created by clone_base, removed by destroy
//!     bundle/                  the clone (`bundle::clone_bundle`, a fresh machine identifier)
//!     inputs/                  a share, read-only in the guest
//!         kbf-guest.token      the per-boot token: 64 hex digits and a newline, fresh
//!                              random bytes (from /dev/urandom) at every boot
//!     outputs/                 a share, writable in the guest: `stdout` and `stderr`
//!                              of the command, written by the guest agent
//!     exec.sock                the Unix socket the driver listens on before the boot
//!     spec.json, helper.log    the helper's spec and log (written by the `System`)
//! ```
//!
//! The store should be on one APFS volume (the clone is `clonefile(2)`) and its path short:
//! `exec.sock` is a Unix socket and its whole path must fit `sun_path` (104 bytes on macOS),
//! or the boot fails.
//!
//! # What the guest must do (the base image, not part of this module)
//!
//! UNCERTAIN until a Mac runs it. The helper shares `inputs/` under the virtiofs tag
//! [`INPUTS_TAG`] (read-only) and `outputs/` under [`OUTPUTS_TAG`] (writable), and bridges
//! the guest's connections to host vsock port [`EXEC_PORT`] to `exec.sock`. So the image
//! must:
//!
//! - mount [`INPUTS_TAG`] at [`GUEST_INPUTS`] and [`OUTPUTS_TAG`] at [`GUEST_OUTPUTS`];
//! - run `kbf-guest` with `--token-file` `<GUEST_INPUTS>/kbf-guest.token`, `--inputs`
//!   [`GUEST_INPUTS`] and `--outputs` [`GUEST_OUTPUTS`], with its agent **dialling** host
//!   vsock port [`EXEC_PORT`] and serving the run protocol (`js_guest::server`) on that
//!   connection.
//!
//! The command's working directory is the root of the inputs share: [`SystemDriver`] sends
//! an empty `cwd`, which the guest agent reads as [`GUEST_INPUTS`] itself.
//!
//! # Concurrency
//!
//! Every call names its VM. A VM's state is behind locks of its own, so the steps of one VM
//! run on one thread while [`VmDriver::kill`] comes from another: `kill` never waits for
//! `run_command`, it only marks the VM killed and writes `Kill` to the guest's connection.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_guest::client::{Client, Killer};
use js_guest::run::{STDERR, STDOUT};
use js_guest::wire::{End, RunRequest, TOKEN_LEN};

use crate::bundle::{self, Identity};
use crate::spec::{Event, ExecBridge, TaggedShare, VmSpec};
use crate::system::{Poll, System, VmProcess};
use js_vm_lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmName, VmSize};

/// The vsock port the guest's agent dials on the host; the helper bridges it to
/// `exec.sock`. (5000 is the probes' results port, 5001 the guest's shutdown daemon.)
pub const EXEC_PORT: u32 = 5002;

/// The virtiofs tag of the inputs share.
pub const INPUTS_TAG: &str = "kbfinputs";

/// The virtiofs tag of the outputs share.
pub const OUTPUTS_TAG: &str = "kbfoutputs";

/// Where the guest mounts the inputs share.
pub const GUEST_INPUTS: &str = "/private/var/kbf-run/inputs";

/// Where the guest mounts the outputs share.
pub const GUEST_OUTPUTS: &str = "/private/var/kbf-run/outputs";

/// The token file in the inputs share.
pub const TOKEN_FILE: &str = "kbf-guest.token";

const INPUTS: &str = "inputs";
const OUTPUTS: &str = "outputs";
const SOCKET: &str = "exec.sock";

/// How long the driver waits for each thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverTiming {
    /// From the boot to the guest agent's `Ready`.
    pub ready: Duration,
    /// How often the boot is looked at again while nothing has happened.
    pub tick: Duration,
    /// How long the guest has to obey a shutdown request, and then how long the helper has
    /// to die after SIGKILL.
    pub stop_grace: Duration,
}

impl Default for DriverTiming {
    fn default() -> Self {
        Self {
            ready: Duration::from_secs(300),
            tick: Duration::from_millis(50),
            stop_grace: Duration::from_secs(60),
        }
    }
}

/// What a VM has once it is booted.
struct Launched {
    proc: Box<dyn VmProcess>,
    token: [u8; TOKEN_LEN],
    listener: UnixListener,
}

/// What `kill` and `run_command` share.
#[derive(Default)]
struct Control {
    killed: bool,
    /// Present from ready until the command has ended.
    killer: Option<Killer<UnixStream>>,
}

impl Control {
    /// Writes `Kill` to the guest, if a command can be running. A write that fails means
    /// the guest is gone, which ends the command as well.
    fn strike(&mut self) {
        if let Some(killer) = self.killer.as_mut() {
            let _ = killer.kill();
        }
    }
}

#[derive(Default)]
struct VmState {
    launched: Mutex<Option<Launched>>,
    client: Mutex<Option<Client<UnixStream>>>,
    control: Mutex<Control>,
}

/// A lock that a panicking thread did not make unusable: the data behind these locks is
/// plain state that every use re-checks.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The driver of the VMs of one store, over `S`.
pub struct SystemDriver<S> {
    system: S,
    store: PathBuf,
    timing: DriverTiming,
    token: fn() -> io::Result<[u8; TOKEN_LEN]>,
    vms: Mutex<BTreeMap<VmName, Arc<VmState>>>,
}

impl<S: System + Send + Sync> SystemDriver<S> {
    /// A driver over `system` for the base image at `<store>/base` and the VMs under
    /// `<store>/runs`, with the default timing.
    #[must_use]
    pub fn new(system: S, store: PathBuf) -> Self {
        Self {
            system,
            store,
            timing: DriverTiming::default(),
            token: random_token,
            vms: Mutex::new(BTreeMap::new()),
        }
    }

    /// The same driver with `timing`.
    #[must_use]
    pub fn with_timing(mut self, timing: DriverTiming) -> Self {
        self.timing = timing;
        self
    }

    fn run_dir(&self, vm: &VmName) -> PathBuf {
        self.store.join("runs").join(vm.as_str())
    }

    fn lookup(&self, vm: &VmName) -> Option<Arc<VmState>> {
        lock(&self.vms).get(vm).cloned()
    }

    fn state(&self, vm: &VmName, step: Step) -> Result<Arc<VmState>, VmError> {
        self.lookup(vm)
            .ok_or_else(|| VmError::step(vm, step, "this driver made no clone of it"))
    }
}

/// `TOKEN_LEN` bytes from `path`.
fn read_token(path: &Path) -> io::Result<[u8; TOKEN_LEN]> {
    let mut token = [0; TOKEN_LEN];
    File::open(path)?.read_exact(&mut token)?;
    Ok(token)
}

/// A fresh token from the kernel's random source.
fn random_token() -> io::Result<[u8; TOKEN_LEN]> {
    read_token(Path::new("/dev/urandom"))
}

/// The last `tail` bytes of the file `path` and whether it had more; a missing file is
/// empty.
fn tail_of(path: &Path, tail: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => return Err(e),
    };
    let tail = u64::try_from(tail).unwrap_or(u64::MAX);
    // A file whose length or seek is refused is read from its start: the `take` below
    // still bounds what is kept.
    let len = file.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(tail);
    let _ = file.seek(SeekFrom::Start(start));
    let mut bytes = Vec::new();
    (&mut file).take(tail).read_to_end(&mut bytes)?;
    Ok((bytes, start > 0))
}

impl<S: System + Send + Sync> VmDriver for SystemDriver<S> {
    fn clone_base(&self, vm: &VmName) -> Result<(), VmError> {
        let fail = |why: String| VmError::step(vm, Step::Clone, why);
        let run = self.run_dir(vm);
        let runs = self.store.join("runs");
        std::fs::create_dir_all(&runs)
            .map_err(|e| fail(format!("make {}: {e}", runs.display())))?;
        // Atomic, and refuses a directory that is already there: a name that has a clone
        // keeps it.
        std::fs::create_dir(&run).map_err(|e| fail(format!("make {}: {e}", run.display())))?;
        if let Err(e) = bundle::clone_bundle(
            &self.system,
            &self.store.join("base"),
            &run.join("bundle"),
            Identity::Fresh,
        ) {
            // Nothing of the failed clone is left.
            let _ = std::fs::remove_dir_all(&run);
            return Err(fail(e.to_string()));
        }
        lock(&self.vms).insert(vm.clone(), Arc::new(VmState::default()));
        Ok(())
    }

    fn boot(&self, vm: &VmName, size: VmSize) -> Result<(), VmError> {
        let state = self.state(vm, Step::Boot)?;
        let fail = |why: String| VmError::step(vm, Step::Boot, why);
        let mut launched = lock(&state.launched);
        if launched.is_some() {
            return Err(fail("it is already booted".to_owned()));
        }
        let run = self.run_dir(vm);
        for share in [INPUTS, OUTPUTS] {
            std::fs::create_dir_all(run.join(share))
                .map_err(|e| fail(format!("make the {share} share: {e}")))?;
        }
        let token = (self.token)().map_err(|e| fail(format!("a token: {e}")))?;
        std::fs::write(
            run.join(INPUTS).join(TOKEN_FILE),
            format!("{}\n", hex::encode(token)),
        )
        .map_err(|e| fail(format!("write the token: {e}")))?;
        let socket = run.join(SOCKET);
        let listener = UnixListener::bind(&socket)
            .map_err(|e| fail(format!("listen on {}: {e}", socket.display())))?;
        // A listener that stays blocking would hold `wait_ready` past its deadline; setting
        // the flag on a socket just made cannot fail.
        let _ = listener.set_nonblocking(true);
        let mut spec = VmSpec::minimal(run.join("bundle"));
        spec.cpus = size.cpus;
        spec.memory_mib = size.memory_mib;
        spec.tagged = vec![
            TaggedShare {
                tag: INPUTS_TAG.to_owned(),
                path: run.join(INPUTS),
                multiple_name: None,
                read_only: true,
            },
            TaggedShare {
                tag: OUTPUTS_TAG.to_owned(),
                path: run.join(OUTPUTS),
                multiple_name: None,
                read_only: false,
            },
        ];
        spec.exec_bridge = Some(ExecBridge {
            port: EXEC_PORT,
            socket,
        });
        let proc = self.system.start_vm(&spec, None).map_err(fail)?;
        *launched = Some(Launched {
            proc,
            token,
            listener,
        });
        Ok(())
    }

    fn wait_ready(&self, vm: &VmName) -> Result<(), VmError> {
        let state = self.state(vm, Step::Ready)?;
        let fail = |why: String| VmError::step(vm, Step::Ready, why);
        let clock = self.system.clock();
        let deadline = clock.now() + self.timing.ready;
        let mut guard = lock(&state.launched);
        let launched = guard
            .as_mut()
            .ok_or_else(|| fail("it was not booted".to_owned()))?;
        let mut started = false;
        let stream = loop {
            if clock.now() >= deadline {
                let what = if started {
                    "the guest agent did not connect"
                } else {
                    "the VM did not start"
                };
                return Err(fail(format!("{what} within {:?}", self.timing.ready)));
            }
            match launched.proc.poll(Duration::ZERO) {
                Poll::Gone { code, signal } => {
                    let log = launched
                        .proc
                        .diagnostics()
                        .map_or(String::new(), |t| format!("; helper log: {}", t.trim()));
                    return Err(fail(format!(
                        "the VM helper ended (code {code:?}, signal {signal:?}){log}"
                    )));
                }
                Poll::Event(Event::Started { .. }) => started = true,
                Poll::Event(Event::Error { stage, message, .. }) => {
                    return Err(fail(format!("the VM helper failed at {stage}: {message}")));
                }
                Poll::Event(_) => continue,
                Poll::Quiet => {}
            }
            // Any other accept error is retried until the deadline.
            if started && let Ok((stream, _)) = launched.listener.accept() {
                break stream;
            }
            clock.pause(self.timing.tick);
        };
        // A socket accepted from a non-blocking listener is blocking on Linux and not on
        // macOS; a stream left non-blocking fails the handshake below.
        let _ = stream.set_nonblocking(false);
        let left = deadline
            .saturating_sub(clock.now())
            .max(Duration::from_millis(1));
        let _ = stream.set_read_timeout(Some(left));
        // The timeout bounds the handshake only; the same socket option clears from here.
        let tuner = stream.try_clone().ok();
        let client = Client::connect(stream, launched.token).map_err(|e| fail(e.to_string()))?;
        tuner.iter().for_each(|s| {
            let _ = s.set_read_timeout(None);
        });
        // Without a second handle `kill` only marks the VM; `stop` still ends the command.
        let killer = client.killer().ok();
        *lock(&state.client) = Some(client);
        lock(&state.control).killer = killer;
        Ok(())
    }

    fn run_command(&self, vm: &VmName, request: &ExecRequest) -> Result<ExecEnd, VmError> {
        let state = self.state(vm, Step::Exec)?;
        let fail = |why: String| VmError::step(vm, Step::Exec, why);
        if lock(&state.control).killed {
            return Ok(ExecEnd::Killed);
        }
        let mut client = lock(&state.client).take().ok_or_else(|| {
            fail(
                "the guest agent is not connected (not ready, or the command already ran)"
                    .to_owned(),
            )
        })?;
        let run = RunRequest {
            argv: request.argv.clone(),
            env: request.env.clone(),
            cwd: String::new(),
            timeout_ms: request.timeout_ms,
            outputs: Vec::new(),
        };
        let result = client
            .run(&run)
            .map_err(|e| fail(e.to_string()))
            .and_then(|_pid| {
                // A `kill` between the check above and the guest starting the command was
                // ignored by the guest (nothing ran yet): repeat it now.
                let mut control = lock(&state.control);
                if control.killed {
                    control.strike();
                }
                drop(control);
                client.wait().map_err(|e| fail(e.to_string()))
            });
        // Nothing runs any more; closing the connection lets the guest agent finish.
        lock(&state.control).killer = None;
        let exit = result?;
        Ok(match exit.end {
            End::Exited(status) => ExecEnd::Exited(status),
            End::Signaled(signal) => ExecEnd::Signaled(signal),
            End::TimedOut => ExecEnd::TimedOut,
            End::Killed => ExecEnd::Killed,
        })
    }

    fn kill(&self, vm: &VmName) {
        if let Some(state) = self.lookup(vm) {
            let mut control = lock(&state.control);
            control.killed = true;
            control.strike();
        }
    }

    fn collect(&self, vm: &VmName, tail_bytes: usize) -> Result<Collected, VmError> {
        let outputs = self.run_dir(vm).join(OUTPUTS);
        let read = |name: &str| {
            tail_of(&outputs.join(name), tail_bytes)
                .map_err(|e| VmError::step(vm, Step::Collect, format!("read {name}: {e}")))
        };
        let (stdout, stdout_cut) = read(STDOUT)?;
        let (stderr, stderr_cut) = read(STDERR)?;
        Ok(Collected {
            stdout,
            stderr,
            truncated: stdout_cut || stderr_cut,
        })
    }

    fn stop(&self, vm: &VmName) -> Result<(), VmError> {
        let Some(state) = self.lookup(vm) else {
            return Ok(());
        };
        let mut guard = lock(&state.launched);
        let Some(mut launched) = guard.take() else {
            return Ok(());
        };
        let grace = self.timing.stop_grace;
        launched.proc.request_shutdown();
        let mut gone = launched.proc.wait_gone(grace);
        if !gone {
            launched.proc.kill9();
            gone = launched.proc.wait_gone(grace);
        }
        if gone {
            return Ok(());
        }
        // Still there: keep it, so that a later stop tries again.
        *guard = Some(launched);
        Err(VmError::step(
            vm,
            Step::Stop,
            "the VM helper is still running after SIGKILL",
        ))
    }

    fn destroy(&self, vm: &VmName) -> Result<(), VmError> {
        self.stop(vm)?;
        lock(&self.vms).remove(vm);
        match std::fs::remove_dir_all(self.run_dir(vm)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(VmError::step(vm, Step::Destroy, e.to_string())),
        }
    }

    /// The directories of `<store>/runs` whose names are VM names, in order. Anything else
    /// there (a file, a directory no [`VmName`] can be, a name that is not UTF-8) was not
    /// made by `clone_base` and is left alone. A VM whose helper outlived its agent is not
    /// adopted: the helper force-stops its VM when its stdin closes, so the destroy that
    /// follows finds nothing running.
    fn leftovers(&self) -> Result<Vec<VmName>, VmError> {
        let entries = match std::fs::read_dir(self.store.join("runs")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(VmError::List(e.to_string())),
        };
        let mut names: Vec<VmName> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().and_then(|n| VmName::new(n).ok()))
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
#[path = "lease_system_tests.rs"]
mod tests;
