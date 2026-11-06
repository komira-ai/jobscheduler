//! The answers of the calls, as typed records. A client of the port parses the cloud's JSON
//! into these once; nothing else in the crate sees the cloud's JSON.

use std::fmt;

use crate::error::SettingsError;

/// The longest zone name kept.
const MAX_ZONE: usize = 63;

/// The shortest and longest project id (Resource Manager: 6 to 30 characters).
const PROJECT_LENGTH: std::ops::RangeInclusive<usize> = 6..=30;

/// Whether `text` is lower-case letters and digits joined by single `-`, with no `-` at
/// either end.
fn dashed_words(text: &str) -> bool {
    text.split('-').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// A Google Cloud project id, such as the configuration's `project` key names: 6 to 30
/// lower-case letters, digits and `-`, starting with a letter and not ending with `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Project(String);

impl Project {
    /// The project `id`.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Project`] when `id` is not a project id.
    pub fn new(id: &str) -> Result<Self, SettingsError> {
        let bytes = id.as_bytes();
        let fits = [
            PROJECT_LENGTH.contains(&id.len()),
            bytes.first().is_some_and(u8::is_ascii_lowercase),
            bytes.last().is_some_and(|b| *b != b'-'),
            bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-'),
        ];
        match fits.into_iter().all(|fit| fit) {
            true => Ok(Self(id.to_owned())),
            false => Err(SettingsError::Project),
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Project {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// A region's name (`us-central1`): the name of a [`Zone`] without its last word.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Region(String);

impl Region {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Region {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// A zone's name (`us-central1-a`): lower-case words of letters and digits joined by
/// single `-`, at least two of them and at most 63 characters; its region is the name
/// without the last word.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Zone(String);

impl Zone {
    /// The zone `name`.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Zone`] when `name` is not a zone name.
    pub fn new(name: &str) -> Result<Self, SettingsError> {
        let fits = [
            name.len() <= MAX_ZONE,
            name.contains('-'),
            dashed_words(name),
        ];
        match fits.into_iter().all(|fit| fit) {
            true => Ok(Self(name.to_owned())),
            false => Err(SettingsError::Zone(name.to_owned())),
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The zone's region: its name without the last `-` and the word after it.
    pub fn region(&self) -> Region {
        // A zone name always holds a `-` (`Zone::new` checks it).
        let (region, _) = self.0.rsplit_once('-').unwrap_or((self.0.as_str(), ""));
        Region(region.to_owned())
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// A machine type's name as the cloud spells it (`a3-highgpu-8g`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MachineTypeName(pub String);

impl MachineTypeName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MachineTypeName {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// An accelerator type's name as the cloud spells it (`nvidia-tesla-t4`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AcceleratorTypeName(pub String);

impl AcceleratorTypeName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AcceleratorTypeName {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// A machine type's `architecture` as the cloud spells it (`X86_64`, `ARM64`, or a value
/// this release does not know, such as `ARCHITECTURE_UNSPECIFIED`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchName(pub String);

/// One entry of a machine type's `accelerators`: accelerators built into the type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedAccelerator {
    /// `guestAcceleratorType`: the accelerator type's name, not a URL.
    pub type_name: AcceleratorTypeName,
    /// `guestAcceleratorCount`: how many of them the guest sees.
    pub count: u32,
}

/// One machine type in one zone, from `machineTypes.aggregatedList` (a type is listed once
/// per zone that offers it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineTypeInfo {
    /// `name`.
    pub name: MachineTypeName,
    /// `zone`: the zone's name, not a URL.
    pub zone: Zone,
    /// `guestCpus`.
    pub vcpus: u32,
    /// `memoryMb`, which is in MiB (an `n1-standard-1` states 3840).
    pub memory_mib: u64,
    /// `accelerators`; empty for a type with none built in.
    pub accelerators: Vec<AttachedAccelerator>,
    /// `architecture`, when the answer gives one.
    pub arch: Option<ArchName>,
}

/// One accelerator type in one zone, from `acceleratorTypes.aggregatedList`: the zone
/// offers it, to attach to a machine type that takes attached accelerators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceleratorTypeInfo {
    /// `name`.
    pub name: AcceleratorTypeName,
    /// `zone`: the zone's name, not a URL.
    pub zone: Zone,
    /// `maximumCardsPerInstance`.
    pub max_per_instance: u32,
}

/// An amount of money as the Cloud Billing API's `Money` gives it: `units` is the int64
/// of whole units, which the JSON answer writes as a string, and `nanos` the billionths of
/// a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Money {
    /// `currencyCode` (`USD`).
    pub currency: String,
    /// `units`, the text as the answer writes it (`"2"`).
    pub units: String,
    /// `nanos`.
    pub nanos: i32,
}

/// How a SKU is consumed, from its `category.usageType`: the two the catalog reads (a
/// client leaves out a SKU of any other, such as a commitment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UsageType {
    /// `OnDemand`: the price of the standard provisioning model.
    OnDemand,
    /// `Preemptible`: the price of Spot VMs ("Spot VMs are the latest version of
    /// preemptible VMs"; their SKUs are described as "Spot Preemptible").
    Preemptible,
}

/// A machine series as the Cloud Billing catalog names it in a SKU of its cores or memory,
/// in lower case (`n1` for "N1 Predefined Instance Core", `a2` for "A2 Instance Core").
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesName(pub String);

/// What a SKU prices, as a client reads it from the SKU's description and category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PricedItem {
    /// One vCPU of a series for an hour (`usageUnit` `h`).
    Cores(SeriesName),
    /// One GiB of memory of a series for an hour (`usageUnit` `GiBy.h`).
    Memory(SeriesName),
    /// One accelerator of a type for an hour (`usageUnit` `h`).
    Accelerator(AcceleratorTypeName),
}

/// One SKU of the Compute Engine service from the Cloud Billing Catalog API's
/// `services.skus.list`, offered in the region asked for: what it prices, how it is
/// consumed and the unit price of the rate that starts at zero usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkuPrice {
    /// What it prices.
    pub item: PricedItem,
    /// `category.usageType`.
    pub usage: UsageType,
    /// `pricingInfo[].pricingExpression.tieredRates[].unitPrice` of the current pricing
    /// and the tier with `startUsageAmount` zero.
    pub unit_price: Money,
}

/// One entry of a region's `quotas` from `regions.get`. The answer gives `limit` and
/// `usage` as JSON numbers; a client keeps a whole number that is not negative and fails a
/// value that is not one as unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaMetric {
    /// `metric` (`CPUS`, `PREEMPTIBLE_NVIDIA_H100_GPUS`).
    pub metric: String,
    /// `limit`.
    pub limit: u64,
    /// `usage`, when the answer gives it.
    pub usage: Option<u64>,
}
