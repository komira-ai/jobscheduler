//! Opening a session: [`Hello`], its answer [`HelloReply`] (a [`Welcome`] or a [`Refused`]),
//! and the parts they share with the heartbeat ([`Phase`]).

use std::collections::BTreeMap;
use std::fmt;

use js_types::{HostId, ProviderId, Resources, Shape, TypeError};
use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::credential::HostCredential;
use crate::error::{Rule, ValidationError};
use crate::heartbeat::HeldRun;
use crate::limits;
use crate::protocol::{self, ProtocolRange};
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
            "memory-pressure" => Self::MemoryPressure,
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
            Phase::MemoryPressure => "memory-pressure",
            Phase::Draining => "draining",
            Phase::Quarantined => "quarantined",
            Phase::Tainted => "tainted",
        };
        text.to_string()
    }
}

/// The capacity vector of a host on the wire: what [`js_types::Resources`] holds, as
/// plain fields, and the lease-count cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireShape {
    /// Thousandths of a CPU core; at most [`limits::MAX_CPU_MILLIS`].
    pub cpu_millis: u64,
    /// Bytes of memory; at most [`limits::MAX_MEMORY_BYTES`].
    pub memory_bytes: u64,
    /// Whole GPU devices; at most [`limits::MAX_GPUS`].
    pub gpus: u64,
    /// The class of the GPUs; required exactly when `gpus` is above zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_class: Option<String>,
    /// The most leases the host takes at once, when the host caps them; from 1 to
    /// [`limits::MAX_LEASE_CAP`]. Absent means the host states no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_leases: Option<u64>,
}

impl WireShape {
    /// The wire form of a resource vector, with no lease-count cap.
    pub fn from_resources(resources: &Resources) -> Self {
        Self {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            gpus: resources.gpus(),
            gpu_class: resources.gpu_class().map(str::to_string),
            max_leases: None,
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

    fn check(&self, at: At) -> Result<(), ValidationError> {
        let cpu = at.field("shape.cpu_millis");
        check::range(cpu, self.cpu_millis, 0, limits::MAX_CPU_MILLIS)?;
        let memory = at.field("shape.memory_bytes");
        check::range(memory, self.memory_bytes, 0, limits::MAX_MEMORY_BYTES)?;
        let gpus = at.field("shape.gpus");
        check::range(gpus, self.gpus, 0, limits::MAX_GPUS)?;
        let class = at.field("shape.gpu_class");
        check::opt_ident(class, self.gpu_class.as_ref())?;
        if (self.gpus > 0) != self.gpu_class.is_some() {
            return Err(class.fail(Rule::Inconsistent));
        }
        if let Some(cap) = self.max_leases {
            check::range(at.field("shape.max_leases"), cap, 1, limits::MAX_LEASE_CAP)?;
        }
        Ok(())
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

impl GpuDevice {
    fn check(&self, at: At) -> Result<(), ValidationError> {
        let index = at.field("gpu_devices.index");
        check::range(index, u64::from(self.index), 0, limits::MAX_GPUS - 1)?;
        let uuid = at.field("gpu_devices.uuid");
        check::text(uuid, &self.uuid, limits::MAX_GPU_TEXT_LEN)?;
        let model = at.field("gpu_devices.model");
        check::text(model, &self.model, limits::MAX_GPU_TEXT_LEN)?;
        let memory = at.field("gpu_devices.memory_bytes");
        check::range(memory, self.memory_bytes, 0, limits::MAX_MEMORY_BYTES)
    }
}

/// Where a launched instance says it runs: the provider's id and the instance id the
/// provider gave it. The proof of that identity travels in the transport, not here; the
/// server matches the two against the instance it launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceIdentity {
    /// The provider's id, as in `js-types` provider ids.
    pub provider: String,
    /// The provider's id for the instance, such as an instance id.
    pub instance_id: String,
}

/// The first message of a session, agent to server (design 5.2). Proof of admission is
/// not a field: it travels in the transport's headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The majors the agent speaks (see [`crate::protocol`]). Absent means major 1.
    #[serde(default = "protocol::first_major_range")]
    pub protocol: ProtocolRange,
    /// The host's id.
    pub host_id: String,
    /// The pool the host asks to join.
    pub pool: String,
    /// The agent's version text.
    pub agent_version: String,
    /// The provider identity of a launched instance; absent for a machine that joins
    /// with the pool's join credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<InstanceIdentity>,
    /// What the host offers.
    pub shape: WireShape,
    /// Typed facts the host reports, such as `os`.
    #[serde(default)]
    pub capabilities: BTreeMap<String, String>,
    /// The GPU devices behind `shape.gpus`; when listed, as many as `shape.gpus`.
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
    /// Whether the agent keeps a [`HostCredential`] it is given: it has a durable place to
    /// write one and uses it for every later call. A server that issues credentials
    /// issues one only to an agent that says so, and an agent that does not (an older
    /// one, or one with no state root) goes on with its join token. Absent means false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub accepts_host_credential: bool,
}

/// Whether `value` is false: a hello that does not ask for a credential does not carry the
/// field, so it reads as it did before the field existed.
fn is_false(value: &bool) -> bool {
    !*value
}

impl Hello {
    /// The shape the host offers: its resource vector and its capabilities.
    pub fn to_shape(&self) -> Result<Shape, TypeError> {
        let resources = self.shape.to_resources()?;
        Ok(Shape::new(resources, self.capabilities.clone()))
    }

    /// The protocol major to answer with: the newest both sides speak. A range that is
    /// empty, or shares no major with this crate's, is refused; the second reports
    /// [`refusal::UNSUPPORTED_PROTOCOL`] so the agent can tell it from a malformed hello.
    pub fn negotiate(&self) -> Result<u32, ValidationError> {
        self.protocol.check("protocol", refusal::INVALID_HELLO)?;
        self.protocol
            .negotiate(&ProtocolRange::THIS)
            .ok_or_else(|| {
                ValidationError::new(
                    "protocol",
                    Rule::UnsupportedProtocol,
                    refusal::UNSUPPORTED_PROTOCOL,
                )
            })
    }

    /// Checks the hello against every bound of [`crate::limits`]: the protocol range, the
    /// ids, the shape, GPU facts, labels and capabilities, and the held runs (no run id
    /// twice). The server calls it on every hello before it looks at the pool or the
    /// credential; `validate` reads no state.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.negotiate()?;
        let at = At::new("hello", refusal::INVALID_HELLO);
        check_host_id(at.field("host_id"), &self.host_id)?;
        check::ident(at.field("pool"), &self.pool)?;
        check::text(
            at.field("agent_version"),
            &self.agent_version,
            limits::MAX_VERSION_LEN,
        )?;
        if let Some(instance) = &self.instance {
            instance.check(at)?;
        }
        self.shape.check(at)?;
        self.check_capabilities(at.field("capabilities"))?;
        self.check_gpu_devices(at.field("gpu_devices"))?;
        check::labels(at.field("labels"), &self.labels)?;
        crate::heartbeat::check_held(at.field("held"), &self.held)
    }

    fn check_capabilities(&self, at: At) -> Result<(), ValidationError> {
        check::count(at, self.capabilities.len(), limits::MAX_CAPABILITIES)?;
        for (key, value) in &self.capabilities {
            check::token(at, key, limits::MAX_CAPABILITY_KEY_LEN, check::ident_byte)?;
            check::text(at, value, limits::MAX_CAPABILITY_VALUE_LEN)?;
        }
        Ok(())
    }

    fn check_gpu_devices(&self, at: At) -> Result<(), ValidationError> {
        check::count(at, self.gpu_devices.len(), limits::MAX_GPUS as usize)?;
        let listed = !self.gpu_devices.is_empty();
        let unequal = self.gpu_devices.len() as u64 != self.shape.gpus;
        if [listed, unequal].into_iter().all(|both| both) {
            return Err(at.fail(Rule::Inconsistent));
        }
        check::distinct(at, self.gpu_devices.iter().map(|device| device.index))?;
        for device in &self.gpu_devices {
            device.check(at)?;
        }
        Ok(())
    }
}

impl InstanceIdentity {
    /// Checks the identity by the rules a hello applies to its `instance`: the provider
    /// is a provider id and the instance id is an id of the id alphabet. A server that
    /// names an instance (to bind a join token to it) uses this, so that it never binds
    /// to an identity no hello could carry.
    ///
    /// # Errors
    ///
    /// The field and the rule that failed.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("hello", refusal::INVALID_HELLO))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        let provider = at.field("instance.provider");
        check::ident(provider, &self.provider)?;
        ProviderId::new(&self.provider).map_err(|_| provider.fail(Rule::BadChars))?;
        check::ident(at.field("instance.instance_id"), &self.instance_id)
    }
}

/// A host id: the id alphabet and cap of [`check::ident`], and the stricter rules of
/// `js-types` host ids (lower case, at most 63 bytes).
pub(crate) fn check_host_id(at: At, value: &str) -> Result<(), ValidationError> {
    check::ident(at, value)?;
    HostId::new(value).map_err(|_| at.fail(Rule::BadChars))?;
    Ok(())
}

/// The server's answer to an accepted [`Hello`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The protocol major both sides speak from here on. Absent means major 1.
    #[serde(default = "protocol::first_major")]
    pub protocol: u32,
    /// The session. Every heartbeat names it; a heartbeat for another epoch is stale.
    pub epoch: u64,
    /// How often the agent heartbeats, in milliseconds: at least
    /// [`limits::MIN_HEARTBEAT_INTERVAL_MS`], at most [`limits::MAX_HEARTBEAT_INTERVAL_MS`].
    pub heartbeat_interval_ms: u64,
    /// How long the server waits in silence before it treats the host as lost, in
    /// milliseconds. It exceeds two heartbeat intervals ([`crate::grace_covers_heartbeats`])
    /// and is at most [`limits::MAX_GRACE_MS`].
    pub grace_ms: u64,
    /// The pool the host was admitted to.
    pub pool: String,
    /// The session secret: an opaque token of at most [`limits::MAX_SESSION_LEN`] bytes
    /// of `[A-Za-z0-9._-]`. The server issues a fresh one at every admitted hello, and
    /// every heartbeat of the session must carry it ([`crate::Heartbeat::session`]);
    /// a later hello replaces it, with the new epoch, so the old one stops working. It
    /// is a secret the agent keeps in memory and never logs. A welcome without one (a
    /// server of the first version) decodes as an empty text, which [`Welcome::validate`]
    /// refuses: such an agent cannot keep a session with a server that checks it.
    #[serde(default)]
    pub session: String,
    /// The credential that replaces the join token, present only in the welcome of a
    /// hello that asked for one ([`Hello::accepts_host_credential`]) and was admitted with
    /// a one-time join token for the first time. The token is dead from then on: the agent
    /// writes this to a file only it can read before it uses it, and sends it in place of
    /// the token with every later hello and heartbeat. Absent means the agent goes on with
    /// the token it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_credential: Option<HostCredential>,
}

/// Every field but `session` and `host_credential`, which print as `".."` (the second only
/// when there is one): the secrets never reach a log or a panic message, also through a
/// type that holds a welcome ([`HelloReply`]).
impl fmt::Debug for Welcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Welcome")
            .field("protocol", &self.protocol)
            .field("epoch", &self.epoch)
            .field("heartbeat_interval_ms", &self.heartbeat_interval_ms)
            .field("grace_ms", &self.grace_ms)
            .field("pool", &self.pool)
            .field("session", &"..")
            .field(
                "host_credential",
                &self.host_credential.as_ref().map(|_| ".."),
            )
            .finish()
    }
}

impl Welcome {
    /// Checks the welcome the way the agent must before it acts on it: a server (or
    /// whoever answers in its place) is not trusted to send a zero interval that would
    /// make the agent spin, a grace shorter than the interval, or a protocol the agent
    /// does not speak.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("welcome", refusal::INVALID_WELCOME);
        let speaks = ProtocolRange::THIS;
        let protocol = at.field("protocol");
        check::range(
            protocol,
            u64::from(self.protocol),
            u64::from(speaks.min),
            u64::from(speaks.max),
        )?;
        let interval = at.field("heartbeat_interval_ms");
        check::range(
            interval,
            self.heartbeat_interval_ms,
            limits::MIN_HEARTBEAT_INTERVAL_MS,
            limits::MAX_HEARTBEAT_INTERVAL_MS,
        )?;
        let grace = at.field("grace_ms");
        check::range(grace, self.grace_ms, 1, limits::MAX_GRACE_MS)?;
        if !crate::grace_covers_heartbeats(self.heartbeat_interval_ms, self.grace_ms) {
            return Err(grace.fail(Rule::Inconsistent));
        }
        check::ident(at.field("pool"), &self.pool)?;
        check::session(at.field("session"), &self.session)
    }
}

/// A refusal of a hello: the constant code of [`crate::refusal`] and no host is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    /// Why the hello was refused. A code this version does not know is still a refusal.
    pub code: String,
    /// For [`refusal::UNSUPPORTED_PROTOCOL`]: the majors the server speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<ProtocolRange>,
}

impl Refused {
    /// Checks the refusal: a short code and a sane range.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("refused", refusal::INVALID_WELCOME);
        check::token(
            at.field("code"),
            &self.code,
            limits::MAX_CODE_LEN,
            check::ident_byte,
        )?;
        match &self.supported {
            Some(range) => range.check("supported", refusal::INVALID_WELCOME),
            None => Ok(()),
        }
    }
}

/// What a hello is answered with, as one value: a welcome, or a refusal. This is the
/// transport-neutral form. Over HTTP the lease server sends the welcome bare (status 200,
/// the fields of [`Welcome`]) and a refusal as the error body every route of the API
/// uses: a 4xx or 5xx status and `{"error": "<code>"}`, with `"supported"` (a
/// [`ProtocolRange`]) beside it for [`refusal::UNSUPPORTED_PROTOCOL`]. [`Refused::code`]
/// is that `error`. The agent reads that HTTP shape, not this tagged one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelloReply {
    /// The host is admitted.
    Welcome(Welcome),
    /// The host is not admitted.
    Refused(Refused),
}

impl HelloReply {
    /// Checks whichever answer this is.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Welcome(welcome) => welcome.validate(),
            Self::Refused(refused) => refused.validate(),
        }
    }
}
