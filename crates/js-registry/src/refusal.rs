use js_agent_wire::{ValidationError, refusal};
use js_caps::Breach;
use js_types::IdError;

/// Why the registry refused a hello or a heartbeat. Each has a constant code of
/// [`js_agent_wire::refusal`] for the wire ([`Refusal::code`]); the text never repeats
/// anything the peer sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The message broke a rule of the wire: bounds, alphabets, protocol, reserved labels.
    #[error("{0}")]
    Invalid(ValidationError),
    /// The host id is not a valid scheduler host id.
    #[error("the host id is not a valid host id")]
    BadHostId(IdError),
    /// The host id of a heartbeat is not a valid scheduler host id.
    #[error("the host id is not a valid host id")]
    BadHeartbeatHostId(IdError),
    /// The hello names another pool.
    #[error("the pool is not served here")]
    UnknownPool,
    /// The host id is bound to another credential, or to another instance.
    #[error("the host id is bound to another credential or instance")]
    HostBound,
    /// The host id has a live session.
    #[error("the host id has a live session")]
    HostLive,
    /// The host id is reserved to another source of hosts (see [`crate::Registry::reserve`]).
    #[error("the host id belongs to another source of hosts")]
    HostListed,
    /// The host's shape or scratch quota does not fit the pool; the [`ShapeFault`] says
    /// which condition failed.
    #[error("{0}")]
    ShapeNotAccepted(ShapeFault),
    /// The pool declares an execution mode and the host lacks what that needs: no
    /// `runtime`, or no `gpu-toolkit` on a host booked GPUs. `field` names the capability.
    #[error("the host contract is not met: the capability {field} is missing")]
    HostContract {
        /// The capability at fault.
        field: &'static str,
    },
    /// The pool declares an execution mode and the host reports another. `field` names
    /// the capability that does.
    #[error("the host runs jobs in another mode than the pool's: the capability {field}")]
    ExecutionMismatch {
        /// The capability at fault.
        field: &'static str,
    },
    /// The pool already holds as many hosts as it allows.
    #[error("the pool is full")]
    PoolFull,
    /// The heartbeat comes from a host with no session.
    #[error("the host has no session")]
    UnknownHost,
    /// The heartbeat names a session that was replaced.
    #[error("the session epoch is not the current one")]
    StaleEpoch,
    /// The heartbeat does not carry the session secret issued at the host's last hello.
    #[error("the heartbeat does not carry the session secret")]
    SessionMismatch,
    /// No session secret could be made, so no host is admitted.
    #[error("no session secret could be made")]
    SessionUnavailable,
}

/// Which condition made a hello `shape-not-accepted`. Each has a constant `reason` of
/// [`js_agent_wire::refusal`] ([`ShapeFault::code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeFault {
    /// The host's capabilities do not meet the pool's: a capability or label the pool names
    /// is missing or has another value, or the GPUs it reports are of another class than
    /// the pool's. The CPU, memory, disk and GPUs a host reports are never compared with the
    /// pool's shape (a host is booked at what it reports), so
    /// this is no resource floor; its wire reason is `below-floor`, as before.
    BelowFloor,
    /// The pool names `scratch-quota` and the host's quota default is absent, does not
    /// count, or is more than the disk the host reports, at which it is booked.
    QuotaDefaultUnfit,
    /// The host enforces a scratch quota while it is booked no disk (the pool books none, or
    /// the host reports none) and the pool does not name `scratch-quota`.
    QuotaWithoutPoolDisk,
}

impl ShapeFault {
    /// The constant reason that goes on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BelowFloor => refusal::SHAPE_BELOW_FLOOR,
            Self::QuotaDefaultUnfit => refusal::SHAPE_QUOTA_DEFAULT,
            Self::QuotaWithoutPoolDisk => refusal::SHAPE_QUOTA_WITHOUT_DISK,
        }
    }
}

impl std::fmt::Display for ShapeFault {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::BelowFloor => {
                "the host's capabilities do not meet the pool's: a capability, label or GPU \
                 class the pool names is missing or has another value"
            }
            Self::QuotaDefaultUnfit => {
                "the pool names scratch-quota and the host's scratch default is absent, zero \
                 or more than the scratch disk the host reports: set the host's \
                 scratch-default-gib within that disk, or give the host more scratch disk"
            }
            Self::QuotaWithoutPoolDisk => {
                "the host enforces a scratch quota and is booked no disk (the pool books none, \
                 or the host reports none) in a pool that does not name scratch-quota: give \
                 the pool disk_bytes and scratch-quota and the host scratch disk, or run the \
                 host without --scratch-quota"
            }
        };
        out.write_str(text)
    }
}

impl From<Breach> for Refusal {
    fn from(breach: Breach) -> Self {
        match breach {
            Breach::Missing { field } => Self::HostContract { field },
            Breach::Mismatch { field } => Self::ExecutionMismatch { field },
        }
    }
}

impl Refusal {
    /// The capability a refusal names, for the two refusals of the host contract
    /// ([`Refusal::HostContract`], [`Refusal::ExecutionMismatch`]); `None` for the rest.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            Self::HostContract { field } | Self::ExecutionMismatch { field } => Some(*field),
            _ => None,
        }
    }

    /// The reason a `shape-not-accepted` refusal names ([`ShapeFault::code`]); `None` for the
    /// rest.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::ShapeNotAccepted(fault) => Some(fault.code()),
            _ => None,
        }
    }

    /// The constant code that goes on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(err) => err.code(),
            Self::BadHostId(_) => refusal::INVALID_HELLO,
            Self::BadHeartbeatHostId(_) => refusal::INVALID_HEARTBEAT,
            Self::UnknownPool => refusal::UNKNOWN_POOL,
            Self::HostBound => refusal::HOST_BOUND,
            Self::HostLive => refusal::HOST_LIVE,
            Self::HostListed => refusal::HOST_LISTED,
            Self::ShapeNotAccepted(_) => refusal::SHAPE_NOT_ACCEPTED,
            Self::HostContract { .. } => refusal::HOST_CONTRACT,
            Self::ExecutionMismatch { .. } => refusal::EXECUTION_MISMATCH,
            Self::PoolFull => refusal::POOL_FULL,
            Self::UnknownHost => refusal::UNKNOWN_HOST,
            Self::StaleEpoch => refusal::STALE_EPOCH,
            Self::SessionMismatch => refusal::SESSION_MISMATCH,
            Self::SessionUnavailable => refusal::SESSION_UNAVAILABLE,
        }
    }
}
