//! Opening a session: [`Hello`], its answer [`Welcome`], and the parts they share with
//! the heartbeat ([`Phase`]).

use std::collections::BTreeMap;

use js_types::{HostId, IdError, LabelError, Labels, Resources, Shape, TypeError};
use serde::{Deserialize, Serialize};

use crate::heartbeat::HeldRun;
use crate::refusal;

/// What a host announces about itself (design 5.3). A host takes new work only in
/// [`Phase::Serving`].
///
/// An unknown phase text decodes as [`Phase::Quarantined`] (fail closed): skipping a
/// phase that is not understood could place work on a node that is not serving. This is
/// the one place a value is mapped rather than skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Phase {
    /// Normal: takes new work.
    Serving,
    /// Switching sessions for a whole-machine lease.
    Preparing,
    /// Switching sessions back after a whole-machine lease.
    Restoring,
    /// Under memory pressure.
    MemoryPressure,
    /// Announced by the agent on SIGTERM: running work finishes, nothing new starts.
    Draining,
    /// Held out of service. Also what a phase this version does not know reads as.
    Quarantined,
    /// A host job ran or a post-job check found something (design 5.12).
    Tainted,
}

impl From<String> for Phase {
    fn from(text: String) -> Self {
        match text.as_str() {
            "serving" => Self::Serving,
            "preparing" => Self::Preparing,
            "restoring" => Self::Restoring,
            "memory_pressure" => Self::MemoryPressure,
            "draining" => Self::Draining,
            "tainted" => Self::Tainted,
            _ => Self::Quarantined,
        }
    }
}

impl From<Phase> for String {
    fn from(phase: Phase) -> Self {
        let text = match phase {
            Phase::Serving => "serving",
            Phase::Preparing => "preparing",
            Phase::Restoring => "restoring",
            Phase::MemoryPressure => "memory_pressure",
            Phase::Draining => "draining",
            Phase::Quarantined => "quarantined",
            Phase::Tainted => "tainted",
        };
        text.to_string()
    }
}

/// The capacity vector of a host on the wire: what [`js_types::Resources`] holds, as
/// plain fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireShape {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Whole GPU devices.
    pub gpus: u64,
    /// The class of the GPUs; required exactly when `gpus` is above zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_class: Option<String>,
}

impl WireShape {
    /// The wire form of a resource vector.
    pub fn from_resources(resources: &Resources) -> Self {
        Self {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            gpus: resources.gpus(),
            gpu_class: resources.gpu_class().map(str::to_string),
        }
    }

    /// The resource vector, refusing a GPU class that does not match the GPU count.
    pub fn to_resources(&self) -> Result<Resources, TypeError> {
        Resources::new(
            self.cpu_millis,
            self.memory_bytes,
            self.gpus,
            self.gpu_class.clone(),
        )
    }
}

/// One GPU device a host reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    /// The device index a job is told to use.
    pub index: u32,
    /// The device's UUID.
    pub uuid: String,
    /// The device's model name.
    pub model: String,
    /// The device's memory in bytes.
    pub memory_bytes: u64,
}

/// The first message of a session, agent to server (design 5.2). Proof of admission is
/// not a field: it travels in the transport's headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The host's id.
    pub host_id: String,
    /// The pool the host asks to join.
    pub pool: String,
    /// The agent's version text.
    pub agent_version: String,
    /// What the host offers.
    pub shape: WireShape,
    /// Typed facts the host reports, such as `os`.
    #[serde(default)]
    pub capabilities: BTreeMap<String, String>,
    /// The GPU devices behind `shape.gpus`.
    #[serde(default)]
    pub gpu_devices: Vec<GpuDevice>,
    /// Labels the operator set on the host; none may use the reserved prefix.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The host's phase.
    pub phase: Phase,
    /// The runs the agent already holds, listed first by an agent that restarted so the
    /// server never sees a gap that would make it requeue running work.
    #[serde(default)]
    pub held: Vec<HeldRun>,
}

/// Why a [`Hello`] is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    /// The host id is empty, long or outside the id alphabet.
    #[error("host_id: {0}")]
    HostId(IdError),
    /// An id the hello must name is empty.
    #[error("{0} must not be empty")]
    EmptyId(&'static str),
    /// A label is refused by the label rules (reserved prefix, empty key, bad value).
    #[error("labels: {0}")]
    Label(LabelError),
    /// The shape counts GPUs without a class, or names a class for no GPUs.
    #[error("shape: {0}")]
    Shape(TypeError),
}

impl HelloError {
    /// The refusal code that goes on the wire for this error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Label(LabelError::Reserved(_)) => refusal::RESERVED_LABEL,
            _ => refusal::INVALID_HELLO,
        }
    }
}

impl Hello {
    /// The shape the host offers: its resource vector and its capabilities.
    pub fn to_shape(&self) -> Result<Shape, TypeError> {
        let resources = self.shape.to_resources()?;
        Ok(Shape::new(resources, self.capabilities.clone()))
    }

    /// Checks the hello: a valid host id, a pool and run ids that are not empty, labels
    /// free of the reserved prefix, and a GPU class named exactly when there are GPUs.
    pub fn validate(&self) -> Result<(), HelloError> {
        HostId::new(&self.host_id).map_err(HelloError::HostId)?;
        if self.pool.is_empty() {
            return Err(HelloError::EmptyId("pool"));
        }
        if self.held.iter().any(|held| held.run_id.is_empty()) {
            return Err(HelloError::EmptyId("held run_id"));
        }
        Labels::from_caller(&self.labels).map_err(HelloError::Label)?;
        self.to_shape().map_err(HelloError::Shape)?;
        Ok(())
    }
}

/// The server's answer to an accepted [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The session. Every heartbeat names it; a heartbeat for another epoch is stale.
    pub epoch: u64,
    /// How often the agent heartbeats, in milliseconds.
    pub heartbeat_interval_ms: u64,
    /// How long the server waits in silence before it treats the host as lost, in
    /// milliseconds. It exceeds two heartbeat intervals ([`crate::grace_covers_heartbeats`]).
    pub grace_ms: u64,
    /// The pool the host was admitted to.
    pub pool: String,
}
