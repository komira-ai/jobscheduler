//! Plain data types of the job scheduler's provider slice: resources, resource
//! classes, host shapes, ids, labels and run requests.
//!
//! Nothing here does I/O or reads a clock. The types map to sections 3, 5.2, 6.2 and
//! 12.5 of the design: [`Resources`] is the resource vector, [`ResourceClass`] is a
//! resource class as data, [`Shape`] is what a host offers, [`LeaseId`] identifies a
//! lease, [`Labels`] carry the reserved `jobscheduler.dev/` schema, and [`Request`] is
//! what a caller asks for. [`ContainerSpec`] and [`Requirements`] say what a job runs and
//! what it needs of a host, on the platform of [`Os`], in the CPU vocabulary of [`Arch`] and
//! [`IsaLevel`] and the accelerator vocabulary of [`Accelerator`] and [`AcceleratorNeed`],
//! resolved through the alias table ([`ALIASES`], [`FAMILIES`]) that `js-caps` re-exports;
//! a container's network is a [`JobNetwork`] level and the [`PortMap`]s it declares. Why a
//! queued job waits is a [`QueueCode`], and what one pass records of it a [`QueueNote`].
//!
//! Time is an input: a [`Timestamp`] read through a [`Clock`], elapsed time through a
//! [`Monotonic`] clock, and [`ManualClock`] for code that moves time itself. A start is a
//! [`StartSpec`], a request already checked against the start contract, or a
//! [`RefusalCode`]; what a host holds is a list of [`Held`] leases, a host a start could
//! take is a [`Candidate`], and a listed host with its leases is a [`HostCapacity`].
//!
//! A job is named by its [`JobId`]. Whether an attempt of a job is followed by another is
//! [`decide`]d from the job's [`RetryPolicy`] and how the attempt ended ([`AttemptEnd`]); a
//! job's completion callback is signed with its [`SigningKey`], and each try of it comes to a
//! [`CallbackAnswer`].
//!
//! The paths a job names are held to one set of rules, [`path`], which the server and the
//! agent share, as they share [`text::hidden`], the characters no text a log may show can
//! hold.
//!
//! The [`sched`] module holds the scheduler's own vocabulary (operations, leases,
//! control-log records, QoS, farm time, the state-machine trait). It shares names with
//! the provider slice above (`Resources`, `LeaseId`) and is not re-exported at the root,
//! so both are reached by their module path.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod accel;
mod booking;
mod callback;
mod class;
mod clock;
pub mod cordon;
mod cpu_ceiling;
mod cpu_series;
mod digest;
pub mod env;
mod history_key;
mod host_state;
mod ids;
mod isa;
mod job;
mod job_id;
mod labels;
mod memory;
mod memory_series;
mod network;
pub mod path;
mod queue;
mod request;
mod resources;
mod retry;
pub mod sched;
mod shape;
mod size_class;
mod start;
pub mod text;

pub use accel::{
    ACCEL_COUNT_KEY, ACCEL_FORM_FACTOR_KEY, ACCEL_INTERCONNECT_KEY, ACCEL_KEYS,
    ACCEL_MEMORY_GIB_KEY, ACCEL_MODEL_KEY, ACCEL_VENDOR_KEY, ALIASES, APPLE_M, AccelError,
    Accelerator, AcceleratorMiss, AcceleratorNeed, Alias, FAMILIES, Family, FormFactor,
    Interconnect, MAX_MEMORY_GIB, Model, NVIDIA_A10, NVIDIA_A100, NVIDIA_B200, NVIDIA_H100,
    NVIDIA_RTX_A6000, Resolved, Variant, Vendor, accelerator_of, vendor_of,
};
pub use booking::{Candidate, Held, HostCapacity};
pub use callback::{
    CallbackAnswer, FailureClass, MAX_SIGNING_KEY_LEN, MIN_SIGNING_KEY_LEN, SigningKey,
    SigningKeyError,
};
pub use class::{Admission, ClassError, ResourceClass};
pub use clock::{Clock, ManualClock, Monotonic, Timestamp};
pub use cpu_ceiling::{CpuCeiling, CpuCeilingError};
pub use cpu_series::{CPU_SERIES_MAX_POINTS, CpuSeries};
pub use digest::{BundleRef, BundleRefError, DigestRef, DigestRefError};
pub use history_key::{HistoryKey, HistoryKeyError, MAX_HISTORY_KEY_LEN};
pub use host_state::HostState;
pub use ids::{
    ClassId, CordonHolder, HostId, IdError, LeaseId, ProviderId, RequestId, RequestIdError,
};
pub use isa::{Arch, ArmVersion, IsaLevel, UnknownArch, UnknownIsaLevel, X86Level, normalise_arch};
pub use job::{
    ContainerSpec, DISK_GIB_KEY, EXEC_KEY, ExecMode, ImageDigestRef, ImageRefError, OS_VERSION_KEY,
    Os, PlatformConflict, Requirements, RequirementsError, UnknownExecMode, UnknownOs,
};
pub use job_id::{JobId, JobIdError, MAX_JOB_ID_LEN};
pub use labels::{LabelError, Labels, RESERVED_PREFIX, ReservedKey};
pub use memory::{MEMORY_PERCENT_KEY, MemoryHeadroom, MemoryPercent, MemoryPercentError};
pub use memory_series::{MEMORY_SERIES_BYTES_PER_POINT, MEMORY_SERIES_MAX_POINTS, MemorySeries};
pub use network::{JobNetwork, PortMap, UnknownJobNetwork};
pub use queue::{JobEventKind, QueueCode, QueueNote};
pub use request::{Request, RequestError};
pub use resources::{Dim, Dims, Resources, TypeError};
pub use retry::{AttemptEnd, Decision, RetryPolicy, decide};
pub use shape::{Capabilities, Shape, os_major};
pub use size_class::{SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL, SizeClass};
pub use start::{RefusalCode, StartSpec};
