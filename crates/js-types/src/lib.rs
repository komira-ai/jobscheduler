//! Plain data types of the job scheduler's provider slice: resources, resource
//! classes, host shapes, ids, labels and run requests.
//!
//! Nothing here does I/O or reads a clock. The types map to sections 3, 5.2, 6.2 and
//! 12.5 of the design: [`Resources`] is the resource vector, [`ResourceClass`] is a
//! resource class as data, [`Shape`] is what a host offers, [`LeaseId`] identifies a
//! lease, [`Labels`] carry the reserved `jobscheduler.dev/` schema, and [`Request`] is
//! what a caller asks for.
//!
//! The [`sched`] module holds the scheduler's own vocabulary (operations, leases,
//! control-log records, QoS, farm time, the state-machine trait). It shares names with
//! the provider slice above (`Resources`, `LeaseId`) and is not re-exported at the root,
//! so both are reached by their module path.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod class;
mod ids;
mod labels;
mod request;
mod resources;
pub mod sched;
mod shape;

pub use class::{Admission, ClassError, ResourceClass};
pub use ids::{ClassId, HostId, IdError, LeaseId, ProviderId, RequestId, RequestIdError};
pub use labels::{LabelError, Labels, RESERVED_PREFIX, ReservedKey};
pub use request::{Request, RequestError};
pub use resources::{Dim, Dims, Resources, TypeError};
pub use shape::{Capabilities, Shape};
