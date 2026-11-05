//! The answers of the calls, as typed records. A client of the port parses the cloud's
//! answer into these once; nothing else in the crate sees the cloud's JSON or XML.

use std::fmt;

use crate::error::SettingsError;

/// A region's code, such as the one the configuration's `region` key names: lower-case
/// letters, digits and `-`, 1 to 32 of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Region(String);

/// The longest region code kept.
const MAX_REGION: usize = 32;

impl Region {
    /// The region `code`.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Region`] when `code` is not lower-case letters, digits and `-`,
    /// 1 to 32 long.
    pub fn new(code: &str) -> Result<Self, SettingsError> {
        let fits = (1..=MAX_REGION).contains(&code.len())
            && code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        match fits {
            true => Ok(Self(code.to_owned())),
            false => Err(SettingsError::Region),
        }
    }

    /// The code as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Region {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// An instance type's name as the cloud spells it (`p5.48xlarge`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceType(pub String);

impl InstanceType {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The instance family: the name before its first `.` (`p5` of `p5.48xlarge`), or the
    /// whole name when it has none.
    pub fn family(&self) -> &str {
        self.0.split('.').next().unwrap_or_default()
    }
}

impl fmt::Display for InstanceType {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// An availability zone's name as the cloud spells it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Zone(pub String);

impl Zone {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An architecture as `ProcessorInfo.SupportedArchitectures` spells it: `x86_64`, `arm64`,
/// `i386`, `x86_64_mac`, `arm64_mac`, or a value this release does not know.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchName(pub String);

/// One GPU entry of `GpuInfo.Gpus` of `DescribeInstanceTypes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    /// `Name`, as the cloud spells it (`A100`).
    pub name: String,
    /// `Manufacturer` (`NVIDIA`).
    pub manufacturer: String,
    /// `Count`: how many of these GPUs the type has.
    pub count: u32,
    /// `MemoryInfo.SizeInMiB`: the memory of one GPU, in MiB, as the cloud states it.
    pub memory_mib: u64,
}

/// One instance type of `DescribeInstanceTypes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceTypeInfo {
    /// `InstanceType`.
    pub name: InstanceType,
    /// `VCpuInfo.DefaultVCpus`.
    pub vcpus: u32,
    /// `MemoryInfo.SizeInMiB`.
    pub memory_mib: u64,
    /// `ProcessorInfo.SupportedArchitectures`, in the order the cloud lists them.
    pub architectures: Vec<ArchName>,
    /// `GpuInfo.Gpus`; empty for a type with no GPU.
    pub gpus: Vec<GpuInfo>,
    /// `InstanceStorageInfo.TotalSizeInGB`, the local instance store in GB (10^9 bytes, the
    /// unit the answer states); `None` when the type has none.
    pub instance_storage_gb: Option<u64>,
    /// `BareMetal`. Read for the launch side; the catalog does not use it.
    pub bare_metal: bool,
}

/// One row of `DescribeInstanceTypeOfferings` with `LocationType=availability-zone`: the
/// type is offered in the zone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ZoneOffering {
    /// `InstanceType`.
    pub instance_type: InstanceType,
    /// `Location`: the availability zone.
    pub zone: Zone,
}

/// The on-demand price of one instance type in the region, from the Pricing API's
/// `GetProducts`: Linux, shared tenancy, no pre-installed software.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypePrice {
    /// The instance type.
    pub instance_type: InstanceType,
    /// `pricePerUnit.USD` of the hourly price dimension, the decimal text as the answer
    /// writes it (`"32.7726000000"`).
    pub usd_per_hour: String,
}

/// The latest spot price of one instance type in one zone, from `DescribeSpotPriceHistory`
/// (`Linux/UNIX`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZonePrice {
    /// `InstanceType`.
    pub instance_type: InstanceType,
    /// `AvailabilityZone`.
    pub zone: Zone,
    /// `SpotPrice`, the decimal text in US dollars per hour.
    pub usd_per_hour: String,
}

/// An account's vCPU quota for one quota family and purchase option, from Service Quotas'
/// `GetServiceQuota`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpuQuota {
    /// `Quota.Value`: the most vCPUs the account may run.
    pub limit: u32,
    /// The vCPUs in use, when the answer gives a number; `GetServiceQuota` itself does not.
    pub in_use: Option<u32>,
}

/// One offering of `DescribeCapacityBlockOfferings`: a block of instances of one type in
/// one zone for a fixed time, at a price paid up front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockOffering {
    /// `CapacityBlockOfferingId`.
    pub id: String,
    /// `InstanceType`.
    pub instance_type: InstanceType,
    /// `AvailabilityZone`.
    pub zone: Zone,
    /// `InstanceCount`: the instances in the block.
    pub instance_count: u32,
    /// `StartDate`, in seconds since the Unix epoch (UTC).
    pub start_unix_seconds: u64,
    /// `CapacityBlockDurationHours` and `CapacityBlockDurationMinutes`, together, in
    /// minutes.
    pub duration_minutes: u64,
    /// `UpfrontFee`: the price of the whole block, the decimal text as the answer writes it.
    pub upfront_fee: String,
    /// `CurrencyCode` (`USD`).
    pub currency: String,
}
