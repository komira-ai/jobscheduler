//! The job service as protobuf: package `jobscheduler.v1`, the one definition of every call
//! a client may make (design section 3.6).
//!
//! The `.proto` files are in this crate's `proto/jobscheduler/v1/`: `job.proto` (the job,
//! its states, its result and why a submit is refused), `fleet.proto` (hosts with the
//! resources booked on them, pools, machine providers), `usage.proto` (ended bookings) and
//! `service.proto` (`service JobService`). The header of `service.proto` states how the
//! package may change: field numbers are never reused or renumbered, a shipped field is
//! never removed or given another meaning, new fields are optional and new enum values are
//! appended. The crate's schema test holds the shipped fields to it.
//!
//! [`v1`] is the Rust the build script generates from them: a struct for each message, an
//! enum for each protobuf enum, the client [`v1::job_service_client::JobServiceClient`] and
//! the server trait [`v1::job_service_server::JobService`] with its service
//! [`v1::job_service_server::JobServiceServer`]. A method a server does not implement
//! answers `UNIMPLEMENTED`. The crate neither listens nor connects: a caller brings the
//! transport. No server implements the service yet.
//!
//! A refused submit is a [`tonic::Status`] carrying a [`v1::Refusal`] in its details;
//! [`refusal`] makes one and reads one back. [`FILE_DESCRIPTOR_SET`] is the encoded
//! descriptor set of the package, with its comments, for reflection and for the tests.

pub mod refusal;

/// The messages, enums, client and server of package `jobscheduler.v1`, generated from the
/// `.proto` files by the build script. Every message, field and enum value carries the
/// comment of its `.proto` file as its documentation.
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/jobscheduler.v1.rs"));
}

/// The encoded `google.protobuf.FileDescriptorSet` of package `jobscheduler.v1`: its four
/// files, with their comments.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/jobscheduler_v1_descriptor_set.bin"
));
