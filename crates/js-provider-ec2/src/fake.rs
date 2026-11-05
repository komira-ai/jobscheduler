//! [`Ec2Api`] over memory, for tests of code that calls the port: one region's answers,
//! built by hand from the documented shapes, with failures scripted per action. It holds
//! no account id, key or endpoint, and answers every region it is asked for with the same
//! data (the calls it records say which region was asked).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::api::{Action, Ec2Api, PurchaseKind};
use crate::error::ApiError;
use crate::model::{
    BlockOffering, InstanceType, InstanceTypeInfo, Region, TypePrice, VcpuQuota, Zone,
    ZoneOffering, ZonePrice,
};
use crate::quota::QuotaFamily;

/// One call the fake answered or failed: the action and the region asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The action.
    pub action: Action,
    /// The region.
    pub region: Region,
}

#[derive(Default)]
struct State {
    types: Vec<InstanceTypeInfo>,
    zones: Vec<ZoneOffering>,
    on_demand: Vec<TypePrice>,
    spot: Vec<ZonePrice>,
    quotas: BTreeMap<(QuotaFamily, PurchaseKind), VcpuQuota>,
    blocks: Vec<BlockOffering>,
    next: BTreeMap<Action, VecDeque<ApiError>>,
    always: BTreeMap<Action, ApiError>,
    calls: Vec<Call>,
}

/// One region of an account in memory. The answers are in the order they were added, as a
/// cloud's answers are in its own order; a Capacity Block offering is answered whatever
/// length is asked for.
#[derive(Default)]
pub struct FakeEc2 {
    state: Mutex<State>,
}

impl FakeEc2 {
    /// An account with nothing in it: every list is empty and there is no quota entry.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds an instance type to `DescribeInstanceTypes`.
    pub fn add_instance_type(&self, info: InstanceTypeInfo) -> &Self {
        self.state().types.push(info);
        self
    }

    /// Offers `instance_type` in `zone` (`DescribeInstanceTypeOfferings`).
    pub fn add_zone_offering(&self, instance_type: &str, zone: &str) -> &Self {
        self.state().zones.push(ZoneOffering {
            instance_type: InstanceType(instance_type.to_owned()),
            zone: Zone(zone.to_owned()),
        });
        self
    }

    /// Prices `instance_type` on demand at `usd_per_hour`, the decimal text the Pricing API
    /// would give.
    pub fn add_on_demand_price(&self, instance_type: &str, usd_per_hour: &str) -> &Self {
        self.state().on_demand.push(TypePrice {
            instance_type: InstanceType(instance_type.to_owned()),
            usd_per_hour: usd_per_hour.to_owned(),
        });
        self
    }

    /// Prices `instance_type` in `zone` on spot at `usd_per_hour`.
    pub fn add_spot_price(&self, instance_type: &str, zone: &str, usd_per_hour: &str) -> &Self {
        self.state().spot.push(ZonePrice {
            instance_type: InstanceType(instance_type.to_owned()),
            zone: Zone(zone.to_owned()),
            usd_per_hour: usd_per_hour.to_owned(),
        });
        self
    }

    /// Gives the account a quota entry for `family` and `purchase`.
    pub fn set_quota(
        &self,
        family: QuotaFamily,
        purchase: PurchaseKind,
        quota: VcpuQuota,
    ) -> &Self {
        self.state().quotas.insert((family, purchase), quota);
        self
    }

    /// Adds a Capacity Block offering.
    pub fn add_block_offering(&self, offering: BlockOffering) -> &Self {
        self.state().blocks.push(offering);
        self
    }

    /// Makes the next call of `action` fail with `error`; several queue in order.
    pub fn fail_next(&self, action: Action, error: ApiError) -> &Self {
        self.state()
            .next
            .entry(action)
            .or_default()
            .push_back(error);
        self
    }

    /// Makes every call of `action` fail with `error`, after any [`Self::fail_next`].
    pub fn fail_always(&self, action: Action, error: ApiError) -> &Self {
        self.state().always.insert(action, error);
        self
    }

    /// Every call made so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// Records a call of `action` and answers it with `answer` of the state, or the
    /// scripted failure.
    fn answer<T>(
        &self,
        action: Action,
        region: &Region,
        answer: impl FnOnce(&State) -> T,
    ) -> Result<T, ApiError> {
        let mut state = self.state();
        state.calls.push(Call {
            action,
            region: region.clone(),
        });
        let scripted = state.next.get_mut(&action).and_then(VecDeque::pop_front);
        match scripted.or_else(|| state.always.get(&action).cloned()) {
            Some(error) => Err(error),
            None => Ok(answer(&state)),
        }
    }
}

impl Ec2Api for FakeEc2 {
    fn instance_types(&self, region: &Region) -> Result<Vec<InstanceTypeInfo>, ApiError> {
        self.answer(Action::DescribeInstanceTypes, region, |state| {
            state.types.clone()
        })
    }

    fn zone_offerings(&self, region: &Region) -> Result<Vec<ZoneOffering>, ApiError> {
        self.answer(Action::DescribeInstanceTypeOfferings, region, |state| {
            state.zones.clone()
        })
    }

    fn on_demand_prices(&self, region: &Region) -> Result<Vec<TypePrice>, ApiError> {
        self.answer(Action::GetProducts, region, |state| state.on_demand.clone())
    }

    fn spot_prices(&self, region: &Region) -> Result<Vec<ZonePrice>, ApiError> {
        self.answer(Action::DescribeSpotPriceHistory, region, |state| {
            state.spot.clone()
        })
    }

    fn vcpu_quota(
        &self,
        region: &Region,
        family: QuotaFamily,
        purchase: PurchaseKind,
    ) -> Result<Option<VcpuQuota>, ApiError> {
        self.answer(Action::GetServiceQuota, region, |state| {
            state.quotas.get(&(family, purchase)).copied()
        })
    }

    fn capacity_block_offerings(
        &self,
        region: &Region,
        _hours: u32,
    ) -> Result<Vec<BlockOffering>, ApiError> {
        self.answer(Action::DescribeCapacityBlockOfferings, region, |state| {
            state.blocks.clone()
        })
    }
}
