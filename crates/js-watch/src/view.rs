//! What the watcher may know of a cloud: reads only.

use std::fmt;

use js_elastic::HostStatus;
use js_provider_lambdalabs::Reader;

/// A cloud could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ViewError(String);

impl ViewError {
    /// An error that reads as `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// An instance of the scheduler that the cloud lists, still running or booting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The cloud's id for it.
    pub id: String,
    /// The name it carries, when it has one.
    pub name: Option<String>,
    /// The region or zone it runs in.
    pub region: String,
}

/// What kind of leftover a [`Stray`] is. Every kind bills while it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrayKind {
    /// An instance with some of the scheduler's marks, not all.
    Instance,
    /// A block volume.
    Volume,
    /// A network interface.
    NetworkInterface,
    /// A public address.
    Address,
    /// A file system (Lambda's persistent file systems bill after the instance is gone).
    FileSystem,
    /// A disk.
    Disk,
    /// A queued resource.
    QueuedResource,
}

impl StrayKind {
    /// Every kind.
    pub const ALL: [Self; 7] = [
        Self::Instance,
        Self::Volume,
        Self::NetworkInterface,
        Self::Address,
        Self::FileSystem,
        Self::Disk,
        Self::QueuedResource,
    ];

    /// The word that names the kind in keys and texts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Volume => "volume",
            Self::NetworkInterface => "network-interface",
            Self::Address => "address",
            Self::FileSystem => "file-system",
            Self::Disk => "disk",
            Self::QueuedResource => "queued-resource",
        }
    }
}

impl fmt::Display for StrayKind {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.as_str())
    }
}

/// A leftover that carries the scheduler's marks and is not an instance the server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    /// What it is.
    pub kind: StrayKind,
    /// The cloud's id for it.
    pub id: String,
    /// The region or zone it is in.
    pub region: String,
    /// What it costs per hour in US cents, when the cloud says.
    pub cents_per_hour: Option<u64>,
}

/// A cloud as the watcher sees it. Every call reads; there is no call that makes, changes
/// or ends anything, so a watcher that holds only this cannot spend or release.
pub trait CloudView: Send + Sync {
    /// The cloud's name, such as `lambda`.
    fn cloud(&self) -> &str;

    /// The scheduler's instances that run or boot now, in every region it may launch in.
    ///
    /// # Errors
    ///
    /// [`ViewError`] when the list cannot be had whole. An error is never an empty list.
    fn instances(&self) -> Result<Vec<Seen>, ViewError>;

    /// The leftovers of the scheduler besides its running instances: volumes, network
    /// interfaces, addresses, file systems, disks, queued resources, and instances with
    /// partial marks. Counted and reported, never released.
    ///
    /// # Errors
    ///
    /// [`ViewError`] when the leftovers cannot be listed whole.
    fn strays(&self) -> Result<Vec<Stray>, ViewError>;
}

/// Lambda, over the read-only [`Reader`]. Lambda's persistent file systems and its
/// `/audit-events` are not modelled by the client yet, so its leftovers are the instances
/// with partial marks.
pub struct LambdaView {
    reader: Reader,
}

impl LambdaView {
    /// A view over `reader`.
    pub fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl CloudView for LambdaView {
    fn cloud(&self) -> &str {
        "lambda"
    }

    fn instances(&self) -> Result<Vec<Seen>, ViewError> {
        let tagged = self
            .reader
            .tagged()
            .map_err(|why| ViewError::new(why.to_string()))?;
        Ok(tagged
            .ours
            .into_iter()
            .filter(|held| held.status != HostStatus::Terminating)
            .map(|held| Seen {
                id: held.id.as_str().to_owned(),
                name: held.name,
                region: held.zone.unwrap_or_default(),
            })
            .collect())
    }

    fn strays(&self) -> Result<Vec<Stray>, ViewError> {
        let tagged = self
            .reader
            .tagged()
            .map_err(|why| ViewError::new(why.to_string()))?;
        // A price is a nicety: a leftover is reported without one.
        let prices = self.reader.prices().unwrap_or_default();
        Ok(tagged
            .suspects
            .into_iter()
            .map(|held| Stray {
                kind: StrayKind::Instance,
                id: held.id.as_str().to_owned(),
                cents_per_hour: prices.get(&held.offer).copied(),
                region: held.zone.unwrap_or_default(),
            })
            .collect())
    }
}
