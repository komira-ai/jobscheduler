//! The typed answers.

use std::fmt;

use time::OffsetDateTime;

/// The id of an instance: 1 to 64 ASCII letters, digits, `-` or `_`. It becomes a path
/// segment of a request, so nothing else is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(String);

/// Text that is not an instance id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("an instance id is 1 to 64 letters, digits, '-' or '_'")]
pub struct InvalidInstanceId;

impl InstanceId {
    /// The id `text`.
    ///
    /// # Errors
    ///
    /// [`InvalidInstanceId`] when `text` is empty, longer than 64 characters or holds any
    /// other character.
    pub fn new(text: &str) -> Result<Self, InvalidInstanceId> {
        let fits = (1..=64).contains(&text.len())
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        match fits {
            true => Ok(Self(text.to_owned())),
            false => Err(InvalidInstanceId),
        }
    }

    /// An id the crate made itself from letters and digits.
    pub(crate) fn made(text: String) -> Self {
        Self(text)
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// An instance's status. A value the API adds later reads as [`Status::Unknown`]; one
/// unknown value never fails a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Starting.
    Booting,
    /// Up.
    Active,
    /// Up and failing its health check.
    Unhealthy,
    /// Gone.
    Terminated,
    /// Being terminated.
    Terminating,
    /// Reclaimed by Lambda. Whether it still bills is not documented.
    Preempted,
    /// A status this crate does not know, printable ASCII only and shortened. Treat it as
    /// an instance that exists and may bill.
    Unknown(String),
}

impl Status {
    /// The status named by `text`.
    pub fn parse(text: &str) -> Self {
        match text {
            "booting" => Self::Booting,
            "active" => Self::Active,
            "unhealthy" => Self::Unhealthy,
            "terminated" => Self::Terminated,
            "terminating" => Self::Terminating,
            "preempted" => Self::Preempted,
            other => Self::Unknown(crate::error::plain(other, 40)),
        }
    }
}

/// A tag of an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// The key.
    pub key: String,
    /// The value.
    pub value: String,
}

/// A region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// The region code, such as `us-west-1`.
    pub name: String,
    /// The region's description.
    pub description: String,
}

/// The CPU architecture of an instance type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Architecture {
    /// `x86_64`.
    X86_64,
    /// `arm64`.
    Arm64,
    /// Anything else, or nothing.
    Other(String),
}

/// An instance type as `GET /instance-types` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceTypeOffer {
    /// The type's name, such as `gpu_1x_a10`.
    pub name: String,
    /// The type's description.
    pub description: String,
    /// The GPU as free text. Do not parse it: the GPU class comes from the agent.
    pub gpu_description: String,
    /// Price in US cents per hour.
    pub price_cents_per_hour: u64,
    /// Virtual CPUs.
    pub vcpus: u32,
    /// Memory in GiB.
    pub memory_gib: u32,
    /// Storage in GiB.
    pub storage_gib: u64,
    /// GPUs.
    pub gpus: u32,
    /// The CPU architecture.
    pub architecture: Architecture,
    /// The regions that have capacity for this type now.
    pub regions: Vec<Region>,
}

impl InstanceTypeOffer {
    /// Whether any region has capacity now.
    pub fn has_capacity(&self) -> bool {
        !self.regions.is_empty()
    }

    /// Whether the region `name` has capacity now.
    pub fn available_in(&self, name: &str) -> bool {
        self.regions.iter().any(|region| region.name == name)
    }
}

/// An instance as `GET /instances` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The id.
    pub id: InstanceId,
    /// The name, when one was set.
    pub name: Option<String>,
    /// The status.
    pub status: Status,
    /// The region's code.
    pub region: String,
    /// The instance type's name.
    pub instance_type: String,
    /// The public IPv4 address, when it has one.
    pub ip: Option<String>,
    /// The private IPv4 address, when it has one.
    pub private_ip: Option<String>,
    /// The hostname, when one was assigned.
    pub hostname: Option<String>,
    /// When the instance first became healthy; absent while it never has, or when the
    /// API's timestamp cannot be read. For logs only: there is no field for the launch
    /// time.
    pub first_healthy: Option<OffsetDateTime>,
    /// The tags.
    pub tags: Vec<Tag>,
}

/// What one launch asks for. The ssh key is one name: Lambda takes exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// The region's code.
    pub region: String,
    /// The instance type's name.
    pub instance_type: String,
    /// The name of the ssh key the account holds, to put on the instance.
    pub ssh_key: String,
    /// The name to give the instance (1 to 64 characters).
    pub name: String,
    /// The tags to put on the instance.
    pub tags: Vec<Tag>,
}
