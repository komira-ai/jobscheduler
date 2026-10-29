//! What the job half of the agent takes from the machine: a clock, a container runtime
//! and a view of the host. [`Machine`] is the seam; [`System`] is the machine itself
//! (Docker or Podman by their command line, the host through `/proc`), and a test gives
//! the worker a machine of its own.
//!
//! The runtime and the view are made on the worker thread (see [`crate::worker`]), by the
//! machine the worker was given, so neither needs to be `Send`. The clock is the one
//! thing both threads read: the session thread stamps a reply with it when the reply
//! arrives, and the lifecycle counts the reply's `valid_for_ms` from that stamp.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli_runtime::{CliRuntime, Settings, system_runner, unix_ms};
use crate::facts::Platform;
use crate::integrity::{HostView, ProcHost};
use crate::lifecycle::{Clock, ContainerRuntime};

/// A monotonic clock both threads of the agent may read.
pub type SharedClock = Arc<dyn Clock + Send + Sync>;

impl Clock for SharedClock {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

/// What a runtime is made from.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeBuild<'a> {
    /// The checked container flags.
    pub settings: &'a Settings,
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
    fn runtime(
        &self,
        build: &RuntimeBuild<'_>,
    ) -> Result<(Box<dyn ContainerRuntime>, Vec<String>), String>;

    /// The host the post-job checks read. Called on the worker thread.
    fn view(&self) -> Box<dyn HostView>;
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

/// Milliseconds since an instant, on the monotonic clock.
struct Monotonic(Instant);

impl Clock for Monotonic {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl Machine for System {
    fn clock(&self) -> SharedClock {
        Arc::new(Monotonic(self.origin))
    }

    fn runtime(
        &self,
        build: &RuntimeBuild<'_>,
    ) -> Result<(Box<dyn ContainerRuntime>, Vec<String>), String> {
        let runner = Box::new(system_runner(build.stop_grace));
        let (settings, host_id) = (build.settings.clone(), build.host_id);
        let opened = CliRuntime::open(settings, host_id, build.platform, unix_ms, runner);
        let runtime = opened.map_err(|why| format!("{}: {why}", build.settings.bin()))?;
        let warnings = runtime.warnings().to_vec();
        Ok((Box::new(runtime), warnings))
    }

    fn view(&self) -> Box<dyn HostView> {
        Box::new(ProcHost::system())
    }
}
