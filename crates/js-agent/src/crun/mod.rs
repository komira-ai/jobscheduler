//! The crun container runtime: [`CrunRuntime`], a [`Runtime`] over js-contain's `crun`
//! backend.
//!
//! This is a stub that the conformance suite (`tests/runtime_suite`) fails: every call
//! answers that the runtime is not written yet.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_contain::oci::OciConfig;
use js_contain::oci::image::ImageSource;

use crate::lifecycle::{
    FilesRoot, Found, Prepared, RunHandle, RunSpec, Runtime, RuntimeError, Status, Ticket,
};

/// Where the agent's cgroups are.
#[derive(Debug, Clone)]
pub struct CgroupSettings {
    /// The cgroup version 2 mount.
    pub mount: PathBuf,
    /// The agent's delegated cgroup, relative to the mount, starting with `/`.
    pub root: String,
    /// The cgroup the leases' cgroups are made in, in the same form.
    pub actions: String,
}

/// What a [`CrunRuntime`] is opened with.
#[derive(Debug, Clone)]
pub struct CrunSettings {
    /// The backend's configuration: the `crun` program, the agent's own binary for the
    /// helper modes and the job user.
    pub oci: OciConfig,
    /// The directory of the backend's image store and leases.
    pub scratch: PathBuf,
    /// The cgroups.
    pub cgroups: CgroupSettings,
    /// The id the agent registered under: the value of the agent label of every container.
    pub host_id: String,
    /// The threads that pull images.
    pub pull_threads: usize,
    /// The root of the runs' files, when the agent has one.
    pub files: Option<FilesRoot>,
}

/// Why a [`CrunRuntime`] could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum CrunError {
    /// The runtime is not written yet.
    #[error("the crun runtime is not written yet")]
    NotWritten,
}

/// A container runtime over `crun`.
#[derive(Debug)]
pub struct CrunRuntime {
    settings: CrunSettings,
}

impl CrunRuntime {
    /// Opens the runtime.
    ///
    /// # Errors
    /// Always, while the runtime is not written.
    pub fn open(settings: CrunSettings, _source: Arc<dyn ImageSource>) -> Result<Self, CrunError> {
        Ok(Self { settings })
    }

    /// The directory of the images and leases.
    #[must_use]
    pub fn scratch(&self) -> &std::path::Path {
        &self.settings.scratch
    }

    /// Creates the container of `spec` and does not start it.
    ///
    /// # Errors
    /// Always, while the runtime is not written.
    pub fn stage(&mut self, _spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        Err(not_written())
    }

    /// [`Runtime::prepare`], waiting for the fetch to end.
    #[must_use]
    pub fn wait_prepared(&self, _image: &str) -> Prepared {
        Prepared::Failed(not_written())
    }

    /// Waits until the container `id` has exited.
    pub fn wait_ended(&mut self, _id: &RunHandle) {}
}

fn not_written() -> RuntimeError {
    RuntimeError::new("the crun runtime is not written yet")
}

impl Runtime for CrunRuntime {
    fn prepare(&mut self, _image: &str, _waited: Option<Ticket>) -> Prepared {
        Prepared::Failed(not_written())
    }

    fn create_and_start(&mut self, _spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        Err(not_written())
    }

    fn poll(&mut self, _id: &RunHandle) -> Result<Status, RuntimeError> {
        Err(not_written())
    }

    fn stop(&mut self, _id: &RunHandle, _grace: Duration) -> Result<(), RuntimeError> {
        Err(not_written())
    }

    fn remove(&mut self, _id: &RunHandle) -> Result<(), RuntimeError> {
        Err(not_written())
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        Err(not_written())
    }

    fn supports_gpus(&self) -> bool {
        false
    }
}
