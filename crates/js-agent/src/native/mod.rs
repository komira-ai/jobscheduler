//! The runtime of native jobs: a program run on a Mac as a user of its own, under a
//! Seatbelt profile, through the root helper `js-mac-session`.

use std::time::Duration;

use js_mac_pool::seatbelt::{AbsPath, Net};
use js_mac_session::client::Client;
use std::path::PathBuf;

use crate::lifecycle::{Found, RunHandle, RunSpec, Runtime, RuntimeError, Status};

/// What a native runtime is told, from the agent's flags.
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Where the helper makes the home of a run's user.
    pub homes: PathBuf,
    /// The most network any run gets.
    pub net: Net,
    /// The directories a start's program must lie under.
    pub tool_roots: Vec<PathBuf>,
    /// Paths no run may read.
    pub read_denies: Vec<AbsPath>,
}

/// Runs native jobs as users of the root helper.
pub struct NativeRuntime {
    _helper: Client,
    _config: NativeConfig,
}

impl NativeRuntime {
    /// A runtime over the helper `helper`.
    ///
    /// # Errors
    ///
    /// The helper cannot be reached.
    pub fn open(helper: Client, config: NativeConfig, _term: u64) -> Result<Self, RuntimeError> {
        Ok(Self {
            _helper: helper,
            _config: config,
        })
    }
}

impl Runtime for NativeRuntime {
    fn create_and_start(&mut self, _: &RunSpec) -> Result<RunHandle, RuntimeError> {
        Err(RuntimeError::new("native start is not built"))
    }

    fn poll(&mut self, _: &RunHandle) -> Result<Status, RuntimeError> {
        Ok(Status::Gone)
    }

    fn stop(&mut self, _: &RunHandle, _: Duration) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn remove(&mut self, _: &RunHandle) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        Ok(Vec::new())
    }

    fn supports_gpus(&self) -> bool {
        false
    }
}
