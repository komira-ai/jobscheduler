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
//! The [`sched`] module holds the scheduler's own vocabulary (operations, leases,
//! control-log records, QoS, farm time, the state-machine trait). It shares names with
//! the provider slice above (`Resources`, `LeaseId`) and is not re-exported at the root,
//! so both are reached by their module path.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod accel;
mod booking;
mod class;
mod clock;
mod digest;
pub mod env;
mod ids;
mod isa;
mod job;
mod labels;
mod memory;
mod network;
mod queue;
mod request;
mod resources;
pub mod sched;
mod shape;
mod start;

pub use accel::{
    ACCEL_COUNT_KEY, ACCEL_FORM_FACTOR_KEY, ACCEL_INTERCONNECT_KEY, ACCEL_KEYS,
    ACCEL_MEMORY_GIB_KEY, ACCEL_MODEL_KEY, ACCEL_VENDOR_KEY, ALIASES, APPLE_M, AccelError,
    Accelerator, AcceleratorMiss, AcceleratorNeed, Alias, FAMILIES, Family, FormFactor,
    Interconnect, MAX_MEMORY_GIB, Model, NVIDIA_A10, NVIDIA_A100, NVIDIA_B200, NVIDIA_H100,
    NVIDIA_RTX_A6000, Resolved, Variant, Vendor, accelerator_of, vendor_of,
};
pub use booking::{Candidate, Held, HostCapacity};
pub use class::{Admission, ClassError, ResourceClass};
pub use clock::{Clock, ManualClock, Monotonic, Timestamp};
pub use digest::{BundleRef, BundleRefError, DigestRef, DigestRefError};
pub use ids::{ClassId, HostId, IdError, LeaseId, ProviderId, RequestId, RequestIdError};
pub use isa::{Arch, ArmVersion, IsaLevel, UnknownArch, UnknownIsaLevel, X86Level, normalise_arch};
pub use job::{
    ContainerSpec, DISK_GIB_KEY, EXEC_KEY, ExecMode, ImageDigestRef, ImageRefError, Os,
    PlatformConflict, Requirements, RequirementsError, UnknownExecMode, UnknownOs,
};
pub use labels::{LabelError, Labels, RESERVED_PREFIX, ReservedKey};
pub use memory::{MEMORY_PERCENT_KEY, MemoryHeadroom, MemoryPercent, MemoryPercentError};
pub use network::{JobNetwork, PortMap, UnknownJobNetwork};
pub use queue::{JobEventKind, QueueCode, QueueNote};
pub use request::{Request, RequestError};
pub use resources::{Dim, Dims, Resources, TypeError};
pub use shape::{Capabilities, Shape};
pub use start::{RefusalCode, StartSpec};
