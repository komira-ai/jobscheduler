//! The calls the provider makes: a narrow port over EC2 and the services beside it (the
//! Pricing API and Service Quotas). The catalog side reads; the launch side launches,
//! reads and terminates instances.

use std::fmt;

use js_elastic::Purchase;

use crate::error::{ApiError, SettingsError};
use crate::launch::{DescribeFilter, Instance, InstanceId, RunRequest};
use crate::model::{
    BlockOffering, InstanceTypeInfo, Region, TypePrice, VcpuQuota, ZoneOffering, ZonePrice,
};
use crate::quota::QuotaFamily;

/// One call of [`Ec2Api`], named by the cloud's API action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// [`Ec2Api::instance_types`]: EC2 `DescribeInstanceTypes`.
    DescribeInstanceTypes,
    /// [`Ec2Api::zone_offerings`]: EC2 `DescribeInstanceTypeOfferings`.
    DescribeInstanceTypeOfferings,
    /// [`Ec2Api::on_demand_prices`]: the Pricing API's `GetProducts`.
    GetProducts,
    /// [`Ec2Api::spot_prices`]: EC2 `DescribeSpotPriceHistory`.
    DescribeSpotPriceHistory,
    /// [`Ec2Api::vcpu_quota`]: Service Quotas `GetServiceQuota`.
    GetServiceQuota,
    /// [`Ec2Api::capacity_block_offerings`]: EC2 `DescribeCapacityBlockOfferings`.
    DescribeCapacityBlockOfferings,
    /// [`Ec2Api::run_instances`]: EC2 `RunInstances`.
    RunInstances,
    /// [`Ec2Api::describe_instances`]: EC2 `DescribeInstances`.
    DescribeInstances,
    /// [`Ec2Api::terminate_instances`]: EC2 `TerminateInstances`.
    TerminateInstances,
}

impl Action {
    /// The API action's name, as the cloud spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::DescribeInstanceTypes => "DescribeInstanceTypes",
            Self::DescribeInstanceTypeOfferings => "DescribeInstanceTypeOfferings",
            Self::GetProducts => "GetProducts",
            Self::DescribeSpotPriceHistory => "DescribeSpotPriceHistory",
            Self::GetServiceQuota => "GetServiceQuota",
            Self::DescribeCapacityBlockOfferings => "DescribeCapacityBlockOfferings",
            Self::RunInstances => "RunInstances",
            Self::DescribeInstances => "DescribeInstances",
            Self::TerminateInstances => "TerminateInstances",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// A way of buying an instance that this provider's configuration may list in `purchase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PurchaseKind {
    /// `on-demand`.
    OnDemand,
    /// `spot`.
    Spot,
    /// `capacity-block`: an EC2 Capacity Block for ML, a block of instances reserved for a
    /// fixed time and paid up front.
    CapacityBlock,
}

impl PurchaseKind {
    /// The option as the configuration spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::OnDemand => "on-demand",
            Self::Spot => "spot",
            Self::CapacityBlock => "capacity-block",
        }
    }

    /// The option the configuration spells `text`.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Purchase`] when it is none of `on-demand`, `spot` and
    /// `capacity-block`.
    pub fn parse(text: &str) -> Result<Self, SettingsError> {
        match text {
            "on-demand" => Ok(Self::OnDemand),
            "spot" => Ok(Self::Spot),
            "capacity-block" => Ok(Self::CapacityBlock),
            other => Err(SettingsError::Purchase(other.to_owned())),
        }
    }

    /// The purchase option an offer of this kind states.
    pub fn purchase(self) -> Purchase {
        match self {
            Self::OnDemand => Purchase::OnDemand,
            Self::Spot => Purchase::Spot,
            Self::CapacityBlock => Purchase::CapacityBlock,
        }
    }
}

/// The calls of the provider. Every call blocks, answers or fails within the client's
/// bound, and makes exactly one request, or one walk over every page of one list that it
/// hides: a list is never cut short in silence (a walk that ends early is
/// [`ApiError::Incomplete`]), nothing is retried here, and a failure is never an empty
/// answer. The catalog's calls only read. The launch side, the last three calls, creates
/// and terminates instances: [`Ec2Api::run_instances`] and [`Ec2Api::terminate_instances`]
/// are the only ones that change anything.
pub trait Ec2Api: Send + Sync {
    /// Every instance type of `region` (`DescribeInstanceTypes`).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn instance_types(&self, region: &Region) -> Result<Vec<InstanceTypeInfo>, ApiError>;

    /// Every (instance type, availability zone) pair of `region` where the type is offered
    /// (`DescribeInstanceTypeOfferings` with `LocationType=availability-zone`).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn zone_offerings(&self, region: &Region) -> Result<Vec<ZoneOffering>, ApiError>;

    /// The on-demand price of every instance type of `region` that has one (the Pricing
    /// API's `GetProducts`: Linux, shared tenancy, no pre-installed software).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn on_demand_prices(&self, region: &Region) -> Result<Vec<TypePrice>, ApiError>;

    /// The latest spot price of every instance type in every zone of `region` that has one
    /// (`DescribeSpotPriceHistory`, `Linux/UNIX`), one per type and zone.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn spot_prices(&self, region: &Region) -> Result<Vec<ZonePrice>, ApiError>;

    /// The account's running-vCPU quota of `family` for `purchase` in `region` (Service
    /// Quotas `GetServiceQuota`; on-demand and spot are separate quotas). `None` when the
    /// account has no such quota entry.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn vcpu_quota(
        &self,
        region: &Region,
        family: QuotaFamily,
        purchase: PurchaseKind,
    ) -> Result<Option<VcpuQuota>, ApiError>;

    /// Every Capacity Block offering of `region` lasting `hours`, of any instance type,
    /// from the earliest start the cloud offers (`DescribeCapacityBlockOfferings` with
    /// `CapacityDurationHours`). It reads offerings and buys nothing.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn capacity_block_offerings(
        &self,
        region: &Region,
        hours: u32,
    ) -> Result<Vec<BlockOffering>, ApiError>;

    /// The launch side: one call each. Launches one instance (`RunInstances`) and answers
    /// the ids of the instances the call named: one, or those of the first call with the
    /// same client token.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails, the answer cannot be read or the call times out.
    /// A failure that is not a cloud answer (a timeout, a transport error) may have
    /// launched the instance.
    fn run_instances(&self, request: &RunRequest) -> Result<Vec<InstanceId>, ApiError>;

    /// The instances `filter` selects (`DescribeInstances`), of every state the cloud
    /// still shows, in one hidden walk over every page. A page that fails fails the call.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when a page fails or the answer cannot be read. Asking by id for an
    /// instance the cloud does not know is an error with the code
    /// `InvalidInstanceID.NotFound` ([`ApiError::is_not_found`]).
    fn describe_instances(&self, filter: &DescribeFilter) -> Result<Vec<Instance>, ApiError>;

    /// Terminates the instances `ids` (`TerminateInstances`).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails; an unknown id is `InvalidInstanceID.NotFound`.
    fn terminate_instances(&self, ids: &[InstanceId]) -> Result<(), ApiError>;
}
