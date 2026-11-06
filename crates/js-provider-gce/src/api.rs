//! The calls the catalog makes: a narrow, read-only port over the Compute Engine API and
//! the Cloud Billing Catalog API.

use std::fmt;

use js_elastic::Purchase;

use crate::error::{ApiError, SettingsError};
use crate::model::{
    AcceleratorTypeInfo, MachineTypeInfo, Project, QuotaMetric, Region, SkuPrice, Zone,
};

/// One call of [`GceApi`], named by the cloud's API method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    /// [`GceApi::machine_types`]: Compute Engine `machineTypes.aggregatedList`.
    MachineTypes,
    /// [`GceApi::accelerator_types`]: Compute Engine `acceleratorTypes.aggregatedList`.
    AcceleratorTypes,
    /// [`GceApi::prices`]: the Cloud Billing Catalog API's `services.skus.list`.
    Prices,
    /// [`GceApi::region_quotas`]: Compute Engine `regions.get`.
    RegionQuotas,
}

impl Method {
    /// The API method's name, as the cloud spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::MachineTypes => "machineTypes.aggregatedList",
            Self::AcceleratorTypes => "acceleratorTypes.aggregatedList",
            Self::Prices => "services.skus.list",
            Self::RegionQuotas => "regions.get",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// A provisioning model that this provider's configuration may list in `purchase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PurchaseKind {
    /// `on-demand`: the standard provisioning model.
    OnDemand,
    /// `spot`: Spot VMs, which the cloud may preempt.
    Spot,
}

impl PurchaseKind {
    /// The option as the configuration spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::OnDemand => "on-demand",
            Self::Spot => "spot",
        }
    }

    /// The option the configuration spells `text`.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Purchase`] when it is neither `on-demand` nor `spot`.
    pub fn parse(text: &str) -> Result<Self, SettingsError> {
        match text {
            "on-demand" => Ok(Self::OnDemand),
            "spot" => Ok(Self::Spot),
            other => Err(SettingsError::Purchase(other.to_owned())),
        }
    }

    /// The purchase option an offer of this kind states.
    pub fn purchase(self) -> Purchase {
        match self {
            Self::OnDemand => Purchase::OnDemand,
            Self::Spot => Purchase::Spot,
        }
    }
}

/// The read-only calls of the catalog. Every call blocks, answers or fails within the
/// client's bound, and makes exactly one request, or one walk over every page of one list
/// that it hides: a list is never cut short in silence (a walk that ends early, or an
/// aggregated list that names an unreachable scope, is [`ApiError::Incomplete`]), nothing
/// is retried here, and a failure is never an empty answer. No call creates or changes
/// anything; inserting and deleting instances belong to the port's launch side, which this
/// crate does not hold.
pub trait GceApi: Send + Sync {
    /// Every machine type of `project` in each of `zones`, one record per type and zone
    /// (`machineTypes.aggregatedList`, every page).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError>;

    /// Every accelerator type of `project` in each of `zones`, one record per type and zone
    /// (`acceleratorTypes.aggregatedList`, every page).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError>;

    /// The on-demand and Spot price of every core, memory and accelerator SKU of the
    /// Compute Engine service offered in `region` (`services.skus.list`, every page).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError>;

    /// The quotas of `project` in `region` (`regions.get`, its `quotas`).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError>;
}
