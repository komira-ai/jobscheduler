//! The messages of the provider protocol, as the agent writes and reads them: package
//! `jobscheduler.provider.v1` of `js-proto`'s `provider.proto`, declared here field by field
//! so that the agent does not link `js-proto`, the job service's crate. They are the proto's
//! messages to the byte: the agent's tests encode each one beside `js-proto`'s generated
//! message and compare the bytes, and decode each from the other's.
//!
//! Every field keeps the number, the type and the name the proto gives it; a field the proto
//! adds later is added here with the same number, or a reader of this crate skips it as any
//! protobuf reader skips a field it does not know.

/// The path of `Prepare`.
pub const PREPARE: &str = "/jobscheduler.provider.v1.MountProvider/Prepare";
/// The path of `Mount`.
pub const MOUNT: &str = "/jobscheduler.provider.v1.MountProvider/Mount";
/// The path of `Release`.
pub const RELEASE: &str = "/jobscheduler.provider.v1.MountProvider/Release";
/// The path of `Sweep`.
pub const SWEEP: &str = "/jobscheduler.provider.v1.MountProvider/Sweep";

/// `MountMode.MOUNT_MODE_READ_ONLY`: the job reads the tree and cannot write it.
pub const MODE_READ_ONLY: i32 = 1;
/// `MountMode.MOUNT_MODE_READ_WRITE`: the job's writes reach the tree.
pub const MODE_READ_WRITE: i32 = 2;
/// `MountMode.MOUNT_MODE_COPY_ON_WRITE`: the job's changes are its own.
pub const MODE_COPY_ON_WRITE: i32 = 3;

/// `Source`: one source of one run, named in full in every call.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Source {
    /// The agent that asks: the id its host registered under.
    #[prost(string, tag = "1")]
    pub agent: String,
    /// The run: `js.` and the job's id.
    #[prost(string, tag = "2")]
    pub run: String,
    /// The source's place in the job: its layers first, then its mounts, from 0.
    #[prost(uint32, tag = "3")]
    pub index: u32,
}

/// `PrepareRequest`: begin making a source's tree.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct PrepareRequest {
    /// The source.
    #[prost(message, optional, tag = "1")]
    pub source: Option<Source>,
    /// What the job named, untouched.
    #[prost(string, tag = "2")]
    pub r#ref: String,
    /// How the job will use the tree: one of the `MODE_` constants.
    #[prost(int32, tag = "3")]
    pub mode: i32,
}

/// `PrepareResponse`: the work has begun.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct PrepareResponse {}

/// `MountRequest`: the directory of a prepared source.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct MountRequest {
    /// The source.
    #[prost(message, optional, tag = "1")]
    pub source: Option<Source>,
}

/// `MountResponse`: the source's directory.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct MountResponse {
    /// An absolute path of a directory on the host.
    #[prost(string, tag = "1")]
    pub path: String,
}

/// `ReleaseRequest`: give up a source.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ReleaseRequest {
    /// The source.
    #[prost(message, optional, tag = "1")]
    pub source: Option<Source>,
}

/// `ReleaseResponse`: the source is given up.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ReleaseResponse {}

/// `SweepRequest`: release an agent's sources of every run it does not keep.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct SweepRequest {
    /// The agent, as [`Source::agent`].
    #[prost(string, tag = "1")]
    pub agent: String,
    /// The runs whose sources are kept, as [`Source::run`].
    #[prost(string, repeated, tag = "2")]
    pub keep: Vec<String>,
}

/// `SweepResponse`: how many sources were released.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct SweepResponse {
    /// How many sources were released.
    #[prost(uint32, tag = "1")]
    pub released: u32,
}
