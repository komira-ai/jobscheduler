//! What a provider that launches machines offers a scheduler: acquire one, list the ones
//! it holds, release one. The calls block, and each is bound by the time limit of the
//! API client under it; a call that runs out of time answers [`Acquired::Unknown`],
//! [`Released::Unknown`] or an error, never a guess.
//!
//! The trait is this crate's own. It must not be called with a lock held that other work
//! waits on (the server's lease table is one): an acquire takes a request or two, and a
//! deferred one answers at once, but a slow API makes every call slow.

use std::time::Duration;

use crate::error::ApiError;
use crate::error::ErrorCode;
use crate::model::{Instance, InstanceId, Status};
use crate::options::GpuClass;
use crate::own::{AcquireId, Claim, LabelError};

/// What a pool wants of a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRequest {
    /// CPU in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// The least number of GPUs.
    pub gpus: u32,
    /// The class of the GPUs, as the agent reports it.
    pub gpu_class: GpuClass,
}

/// One instance type in one region that has capacity now and fits a [`GpuRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The instance type's name.
    pub instance_type: String,
    /// The region's code.
    pub region: String,
    /// Price in US cents per hour, as the API gave it.
    pub price_cents_per_hour: u64,
    /// GPUs the type has.
    pub gpus: u32,
}

/// One acquire: launch one instance of `candidate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRequest {
    /// The id of the acquire. A second request with the same id launches nothing when the
    /// first one's instance exists.
    pub acquire: AcquireId,
    /// The name of the pool the machine is for.
    pub pool: String,
    /// More tags for the instance: key and value, the key lower case letters, digits and
    /// `-`.
    pub labels: Vec<(String, String)>,
    /// The type and region to launch.
    pub candidate: Candidate,
}

/// An instance this scheduler launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The id.
    pub id: InstanceId,
    /// The acquire id in its tags.
    pub acquire: String,
    /// The name, when it has one.
    pub name: Option<String>,
    /// The instance type's name.
    pub instance_type: String,
    /// The region's code.
    pub region: String,
    /// The status.
    pub status: Status,
}

impl Held {
    pub(crate) fn of(instance: &Instance) -> Self {
        Self {
            id: instance.id.clone(),
            acquire: crate::own::Owner::acquire_of(instance)
                .unwrap_or_default()
                .to_owned(),
            name: instance.name.clone(),
            instance_type: instance.instance_type.clone(),
            region: instance.region.clone(),
            status: instance.status.clone(),
        }
    }
}

/// The plain constants a refusal is one of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalCode {
    /// The account's limit was reached.
    Quota,
    /// The provider has no capacity for that type and region now. The caller falls
    /// through to its next option or provider.
    Capacity,
    /// The request does not fit: a type, region or key the account does not know, or a
    /// label that cannot become a tag. A configuration error, not a state of the world.
    Shape,
    /// The request was not made: the key is refused, or a guard of this crate stopped it.
    Denied,
}

/// Why a request was refused, for logs and operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The provider is not enabled.
    Disabled,
    /// The provider is in dry-run mode: the request was built and logged, not sent.
    DryRun,
    /// The instance type is not in the option table.
    NotAllowed,
    /// A label or the pool name cannot become a tag.
    Label(LabelError),
    /// Instances with some of this scheduler's marks, not all, exist: launching more
    /// could exceed `max_machines` unseen. An operator clears them.
    HalfMarked {
        /// How many there are.
        count: usize,
    },
    /// The instances held, and launches not yet resolved, reach `max_machines`.
    AtMaxMachines {
        /// The count.
        held: usize,
        /// The limit.
        max_machines: usize,
    },
    /// The API refused with this code.
    Api(ErrorCode),
}

/// A refusal: the constant, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The constant.
    pub code: RefusalCode,
    /// The reason.
    pub reason: Reason,
}

/// The release of an instance that was not the one wanted, with the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    /// The instance.
    pub id: InstanceId,
    /// What releasing it read back.
    pub released: Released,
}

/// The answer to an acquire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// An instance of this acquire exists: launched now, or found launched earlier.
    /// Launched, not ready: the machine's own agent says when it is.
    Accepted {
        /// The instance.
        held: Held,
        /// Further instances one launch answer named, which were released at once.
        extras: Vec<Extra>,
    },
    /// Nothing was launched, for a reason that is an answer.
    Refused(Refusal),
    /// No answer: the call timed out or failed in a way that may have launched an
    /// instance. Nothing is retried here; list before asking again.
    Unknown,
    /// Nothing was asked of the API: a launch left too recently. Try again after `wait`.
    Deferred {
        /// How long until a launch is allowed.
        wait: Duration,
    },
}

/// The answer to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Released {
    /// The API said, for a read by id, that the instance does not exist. This is the only
    /// evidence of "gone": a terminate answer, an empty list or a `terminated` status
    /// are not.
    Gone,
    /// The instance is being terminated, or terminate was accepted; the status read.
    Terminating(Status),
    /// The instance is not entirely ours; nothing was sent.
    Refused(Claim),
    /// No answer: nothing observed shows the instance gone or going. Callers must treat
    /// this as "retry", never as "gone".
    Unknown,
}

/// A scheduler's view of a provider that launches machines.
pub trait Elastic: Send + Sync {
    /// Launches one instance for `request`, or finds the one an earlier request with the
    /// same acquire id launched.
    fn acquire(&self, request: &AcquireRequest) -> Acquired;

    /// The instances this scheduler launched, in the API's order, terminated ones
    /// included if the API lists them.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the list cannot be had whole. An error is never an empty list.
    fn list(&self) -> Result<Vec<Held>, ApiError>;

    /// Terminates one instance, if it is ours, and reads it back.
    fn release(&self, id: &InstanceId) -> Released;
}
