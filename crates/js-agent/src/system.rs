//! What the job half of the agent takes from the machine: a clock, a container runtime
//! and a view of the host. [`Machine`] is the seam; [`System`] is the machine itself
//! (Docker or Podman by their command line, the host through `/proc`), and a test gives
//! the worker a machine of its own.
//!
//! The runtime and the view are made on the worker thread (see [`crate::worker`]), by the
//! machine the worker was given, so neither needs to be `Send`. The clock is the one
//! thing both threads read: the session thread stamps a reply with it when the reply
//! arrives, and the lifecycle counts the reply's `valid_for_ms` from that stamp.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use js_mac_session::client::Client;

use crate::cli_runtime::{CliRuntime, Settings, system_runner, unix_ms};
use crate::facts::{FileReader, Platform, SystemFiles};
use crate::integrity::{HostView, ProcHost};
use crate::lifecycle::{Clock, Runtime};
use crate::native::{self, NativeRuntime, NativeSettings};
use crate::quota::{self, ScratchQuota};
use crate::usage::UsageClock;

/// The bits of a native runtime's term that are random. A lease user is named
/// `kbf-lease-<term>-<sequence>` and a user name is at most 31 bytes, so the term has at
/// most 13 digits (40 bits is below 10^13) and leaves room for a sequence of seven.
const TERM_BITS: u32 = 40;

/// The native runtime over the helper `native` names. Its term is random, so a second
/// process of the agent never asks the helper for a lease user the first one had.
fn native_runtime(native: &NativeSettings) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
    let mut bytes = [0u8; 8];
    // A source of randomness that fails leaves a term of zero, which the helper's ledger
    // refuses the second time: the start fails, loudly, and never runs as a used user.
    let _ = getrandom::getrandom(&mut bytes);
    let term = u64::from_le_bytes(bytes) >> (u64::BITS - TERM_BITS);
    native::self_test(native.config(), native.sandbox_exec())
        .map_err(|why| format!("--native-helper-socket: the sandbox self-test failed: {why}"))?;
    let helper = Client::new(native.socket());
    let opened = NativeRuntime::open(helper, native.config().clone(), term);
    let runtime = opened.map_err(|why| format!("--native-helper-socket: {why}"))?;
    let runtime = match native.bundles() {
        Some(store) => runtime.with_bundles(Arc::clone(store)),
        None => runtime,
    };
    Ok((Box::new(runtime), Vec::new()))
}

/// A monotonic clock both threads of the agent may read.
pub type SharedClock = Arc<dyn Clock + Send + Sync>;

impl Clock for SharedClock {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

/// What the agent runs jobs with.
#[derive(Debug, Clone)]
pub enum Backend {
    /// Containers, through Docker or Podman (Linux).
    Container(Settings),
    /// Programs run as users of the root helper, under a Seatbelt profile (macOS).
    Native(NativeSettings),
}

/// What a runtime is made from.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeBuild<'a> {
    /// What the jobs run with, checked.
    pub backend: &'a Backend,
    /// The id the agent registered under.
    pub host_id: &'a str,
    /// The platform the agent runs on.
    pub platform: &'a Platform,
    /// How long a container gets to stop before it is killed.
    pub stop_grace: Duration,
}

/// What the agent needs of the machine to run jobs.
pub trait Machine: Send + Sync {
    /// The monotonic clock of the agent's jobs. Called once, when the worker starts.
    fn clock(&self) -> SharedClock;

    /// The container runtime, and the warnings the operator should read when it is made.
    /// Called on the worker thread.
    ///
    /// # Errors
    ///
    /// The runtime cannot be made; the text says why.
    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String>;

    /// The host the post-job checks read. Called on the worker thread.
    fn view(&self) -> Box<dyn HostView>;

    /// The clock the usage is timed by, microseconds on the same monotonic clock as
    /// [`Machine::clock`] (the same zero), which a lease's start is stamped on. Called once,
    /// on the worker thread. By default the clock of the jobs in whole milliseconds.
    fn usage_clock(&self) -> Box<dyn UsageClock> {
        Box::new(Milliseconds(self.clock()))
    }

    /// The project quota of the scratch root `root` (its canonical path): what the agent
    /// probes at start and, under `--scratch-quota`, bounds each run with. Called on the
    /// thread that starts the agent. By default the machine's own ([`quota::system`]).
    fn scratch_quota(&self, root: &Path) -> Arc<dyn ScratchQuota> {
        quota::system(root)
    }

    /// The files the cgroup tree is read from. Called once, on the worker thread. By
    /// default the machine's own.
    fn cgroup_files(&self) -> Box<dyn FileReader> {
        Box::new(SystemFiles)
    }
}

/// The clock of the jobs read as microseconds, a millisecond at a time.
struct Milliseconds(SharedClock);

impl UsageClock for Milliseconds {
    fn now_usec(&self) -> u64 {
        self.0.now_ms().saturating_mul(1000)
    }
}

/// The machine the agent runs on.
#[derive(Debug, Clone, Copy)]
pub struct System {
    origin: Instant,
}

impl System {
    /// The machine, with a clock that starts now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

/// Time since an instant, on the monotonic clock.
struct Monotonic(Instant);

impl Clock for Monotonic {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl UsageClock for Monotonic {
    fn now_usec(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

impl Machine for System {
    fn clock(&self) -> SharedClock {
        Arc::new(Monotonic(self.origin))
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        let settings = match build.backend {
            Backend::Container(settings) => settings,
            Backend::Native(native) => return native_runtime(native),
        };
        let runner = Box::new(system_runner(build.stop_grace));
        let opened = CliRuntime::open(
            settings.clone(),
            build.host_id,
            build.platform,
            unix_ms,
            runner,
        );
        let runtime = opened.map_err(|why| format!("{}: {why}", settings.bin()))?;
        let warnings = runtime.warnings().to_vec();
        Ok((Box::new(runtime), warnings))
    }

    fn view(&self) -> Box<dyn HostView> {
        Box::new(ProcHost::system())
    }

    fn usage_clock(&self) -> Box<dyn UsageClock> {
        Box::new(Monotonic(self.origin))
    }
}
