//! The calls the provider makes: a narrow port over the Compute Engine API and the Cloud
//! Billing Catalog API. The catalog side reads; the launch side inserts, reads, lists and
//! deletes instances and reads the zone operations they answer with.

use std::fmt;

use js_elastic::Purchase;

use crate::error::{ApiError, SettingsError};
use crate::launch::{InsertRequest, Instance, InstanceRef, ListFilter, Operation, OperationRef, Page};
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
    /// [`GceApi::insert_instance`]: Compute Engine `instances.insert`.
    InsertInstance,
    /// [`GceApi::get_instance`]: Compute Engine `instances.get`.
    GetInstance,
    /// [`GceApi::delete_instance`]: Compute Engine `instances.delete`.
    DeleteInstance,
    /// [`GceApi::list_instances`]: Compute Engine `instances.aggregatedList`.
    ListInstances,
    /// [`GceApi::get_operation`]: Compute Engine `zoneOperations.get`.
    GetOperation,
}

impl Method {
    /// The API method's name, as the cloud spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::MachineTypes => "machineTypes.aggregatedList",
            Self::AcceleratorTypes => "acceleratorTypes.aggregatedList",
            Self::Prices => "services.skus.list",
            Self::RegionQuotas => "regions.get",
            Self::InsertInstance => "instances.insert",
            Self::GetInstance => "instances.get",
            Self::DeleteInstance => "instances.delete",
            Self::ListInstances => "instances.aggregatedList",
            Self::GetOperation => "zoneOperations.get",
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

/// The calls of the provider. Every call blocks, answers or fails within the client's
/// bound ([`ApiError::Timeout`] when it runs out), and makes exactly one request; a catalog
/// list is one walk over every page that it hides, never cut short in silence (a walk that
/// ends early, or an aggregated list that names an unreachable scope, is
/// [`ApiError::Incomplete`]). Nothing is retried here, and a failure is never an empty
/// answer. The catalog's calls only read. The launch side, the last five calls, holds no
/// logic: one call of the cloud each, and [`GceApi::insert_instance`] and
/// [`GceApi::delete_instance`] are the only calls that change anything.
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

    /// Inserts one instance (`instances.insert` with the request's `requestId`) and answers
    /// the zone operation the cloud started for it. A repeat with the same `requestId`
    /// within the cloud's window answers the first insert's operation and inserts nothing.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read. A name taken already
    /// is the reason `alreadyExists` ([`ApiError::is_already_exists`]). A failure that is
    /// not a cloud answer (a timeout, a transport error) may have inserted the instance.
    fn insert_instance(&self, request: &InsertRequest) -> Result<Operation, ApiError>;

    /// Reads one instance (`instances.get`).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read. An instance the cloud
    /// does not know is the reason `notFound` ([`ApiError::is_not_found`]).
    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError>;

    /// Deletes one instance (`instances.delete`) and answers the zone operation the cloud
    /// started for it.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read; an instance the cloud
    /// does not know is the reason `notFound`.
    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError>;

    /// One page of the instances of every zone of the project that carry the filter's
    /// labels (`instances.aggregatedList` with a label `filter` and the page token), of
    /// every status the cloud still shows. The caller asks for the next page with
    /// [`Page::next_page_token`].
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError>;

    /// Reads one zone operation (`zoneOperations.get`).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError>;
}
