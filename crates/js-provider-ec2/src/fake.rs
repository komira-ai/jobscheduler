//! [`Ec2Api`] over memory, for tests of code that calls the port: one region's answers,
//! built by hand from the documented shapes, with failures scripted per action. It holds
//! no account id, key or endpoint, and answers every region it is asked for with the same
//! data (the calls it records say which region was asked). It also holds the instances of
//! the launch side: the ones this fake launched and the ones someone else did, which a
//! tag read shows and a launch with a repeated client token does not duplicate.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::api::{Action, Ec2Api, PurchaseKind};
use crate::error::ApiError;
use crate::launch::{DescribeFilter, Instance, InstanceId, InstanceState, RunRequest};
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
    block_hours: Vec<u32>,
    actions: Vec<Action>,
    any_next: VecDeque<ApiError>,
    lose_after: BTreeMap<Action, VecDeque<ApiError>>,
    instances: Vec<Instance>,
    launched: usize,
    run_requests: Vec<RunRequest>,
    terminates: usize,
    tokens: BTreeMap<String, Vec<InstanceId>>,
    next_id: u64,
    page_size: Option<usize>,
    fail_page: Option<usize>,
    read_after_terminate: VecDeque<ApiError>,
    read_armed: VecDeque<ApiError>,
}

/// Records a call of `action` and takes the failure scripted for it: the next call of any
/// kind first, then the next of this action, then the standing one.
fn begin(state: &mut State, action: Action) -> Option<ApiError> {
    state.actions.push(action);
    state
        .any_next
        .pop_front()
        .or_else(|| state.next.get_mut(&action).and_then(VecDeque::pop_front))
        .or_else(|| state.always.get(&action).cloned())
}

/// Answers `value`, unless the answer of this call is scripted to be lost: the call has
/// taken effect and the caller is told `error`.
fn finish<T>(state: &mut State, action: Action, value: T) -> Result<T, ApiError> {
    match state
        .lose_after
        .get_mut(&action)
        .and_then(VecDeque::pop_front)
    {
        Some(error) => Err(error),
        None => Ok(value),
    }
}

impl State {
    /// Launches the instance `request` asks for and answers its id.
    fn launch(&mut self, request: &RunRequest) -> Vec<InstanceId> {
        let id = InstanceId::numbered(self.next_id);
        self.next_id += 1;
        self.instances.push(Instance {
            id: id.clone(),
            state: InstanceState::Pending,
            tags: request.tags.clone(),
            instance_type: request.instance_type.clone(),
            zone: request.zone.clone(),
            image: request.image.clone(),
        });
        self.tokens
            .insert(request.client_token.as_str().to_owned(), vec![id.clone()]);
        self.run_requests.push(request.clone());
        self.launched += 1;
        vec![id]
    }
}

/// One region of an account in memory. The answers are in the order they were added, as a
/// cloud's answers are in its own order; a Capacity Block offering is answered whatever
/// length is asked for, and the length asked is recorded ([`FakeEc2::block_hours_asked`]).
/// Instances are listed in the order they were launched or added, whatever their state.
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

    /// Makes the next call of any kind fail with `error`, nothing having happened; several
    /// queue in order. It is the call that failed before it was sent.
    pub fn fail_next_call(&self, error: ApiError) -> &Self {
        self.state().any_next.push_back(error);
        self
    }

    /// Makes the next call of `action` take effect and then answer `error`: the answer is
    /// lost after the call landed. Several queue in order.
    pub fn lose_after_next(&self, action: Action, error: ApiError) -> &Self {
        self.state()
            .lose_after
            .entry(action)
            .or_default()
            .push_back(error);
        self
    }

    /// Makes the first [`Ec2Api::describe_instances`] after the next
    /// [`Ec2Api::terminate_instances`] that takes effect fail with `error`. Several queue
    /// in order, one per terminate.
    pub fn fail_read_after_next_terminate(&self, error: ApiError) -> &Self {
        self.state().read_after_terminate.push_back(error);
        self
    }

    /// Makes [`Ec2Api::describe_instances`] answer in pages of `size` instances (at least
    /// one), the walk the client hides. The answer is the same; only [`Self::fail_page`]
    /// can tell.
    pub fn paginate(&self, size: usize) -> &Self {
        self.state().page_size = Some(size.max(1));
        self
    }

    /// Makes the page `page` (the first is 1) of the next walk of
    /// [`Ec2Api::describe_instances`] that reaches it fail, so the whole call fails with
    /// [`ApiError::Incomplete`]. Without [`Self::paginate`] a walk is one page. It fails
    /// one walk.
    pub fn fail_page(&self, page: usize) -> &Self {
        self.state().fail_page = Some(page);
        self
    }

    /// Makes the fake forget every client token it has seen, as the cloud does after its
    /// window: the same token launches again.
    pub fn forget_client_tokens(&self) -> &Self {
        self.state().tokens.clear();
        self
    }

    /// Adds an instance that someone else launched. It is not counted in
    /// [`Self::launched`].
    pub fn add_instance(&self, instance: Instance) -> &Self {
        self.state().instances.push(instance);
        self
    }

    /// Puts the instance `id` in `state`; nothing happens when there is none.
    pub fn set_state(&self, id: &InstanceId, state: InstanceState) -> &Self {
        self.state()
            .instances
            .iter_mut()
            .filter(|instance| instance.id == *id)
            .for_each(|instance| instance.state = state);
        self
    }

    /// Makes the cloud forget the instance `id`: a read by its id answers
    /// `InvalidInstanceID.NotFound`.
    pub fn remove_instance(&self, id: &InstanceId) -> &Self {
        self.state().instances.retain(|instance| instance.id != *id);
        self
    }

    /// Every instance the fake holds, in order.
    pub fn instances(&self) -> Vec<Instance> {
        self.state().instances.clone()
    }

    /// Every launch that created an instance, in order. A repeat of a client token is not
    /// in it.
    pub fn run_requests(&self) -> Vec<RunRequest> {
        self.state().run_requests.clone()
    }

    /// How many instances the fake launched.
    pub fn launched(&self) -> usize {
        self.state().launched
    }

    /// How many terminate calls reached the fake and took effect (a call that failed before
    /// it was sent does not count; one whose answer was lost does).
    pub fn terminates(&self) -> usize {
        self.state().terminates
    }

    /// Every action called so far, in order: the catalog's and the launch side's, failed
    /// calls included. [`Self::calls`] holds the catalog's alone, with their regions.
    pub fn actions(&self) -> Vec<Action> {
        self.state().actions.clone()
    }

    /// Every call of the catalog's actions made so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// The block length, in hours, of every Capacity Block offering read so far, in order.
    pub fn block_hours_asked(&self) -> Vec<u32> {
        self.state().block_hours.clone()
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
        if let Some(error) = begin(&mut state, action) {
            return Err(error);
        }
        let value = answer(&state);
        finish(&mut state, action, value)
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
        hours: u32,
    ) -> Result<Vec<BlockOffering>, ApiError> {
        self.state().block_hours.push(hours);
        self.answer(Action::DescribeCapacityBlockOfferings, region, |state| {
            state.blocks.clone()
        })
    }

    fn run_instances(&self, request: &RunRequest) -> Result<Vec<InstanceId>, ApiError> {
        let mut state = self.state();
        if let Some(error) = begin(&mut state, Action::RunInstances) {
            return Err(error);
        }
        let seen = state.tokens.get(request.client_token.as_str()).cloned();
        let ids = match seen {
            Some(ids) => ids,
            None => state.launch(request),
        };
        finish(&mut state, Action::RunInstances, ids)
    }

    fn describe_instances(&self, filter: &DescribeFilter) -> Result<Vec<Instance>, ApiError> {
        let mut state = self.state();
        if let Some(error) = begin(&mut state, Action::DescribeInstances) {
            return Err(error);
        }
        if let Some(error) = state.read_armed.pop_front() {
            return Err(error);
        }
        let rows: Vec<Instance> = match filter {
            DescribeFilter::Tags(tags) => state
                .instances
                .iter()
                .filter(|instance| tags.iter().all(|tag| instance.tags.contains(tag)))
                .cloned()
                .collect(),
            DescribeFilter::Ids(ids) => {
                let held = |id: &InstanceId| state.instances.iter().any(|row| row.id == *id);
                if !ids.iter().all(held) {
                    return Err(ApiError::answered("InvalidInstanceID.NotFound"));
                }
                let wanted = |row: &&Instance| ids.contains(&row.id);
                state.instances.iter().filter(wanted).cloned().collect()
            }
        };
        let pages = rows
            .len()
            .div_ceil(state.page_size.unwrap_or(usize::MAX))
            .max(1);
        if state
            .fail_page
            .is_some_and(|page| (1..=pages).contains(&page))
        {
            state.fail_page = None;
            return Err(ApiError::Incomplete);
        }
        finish(&mut state, Action::DescribeInstances, rows)
    }

    fn terminate_instances(&self, ids: &[InstanceId]) -> Result<(), ApiError> {
        let mut state = self.state();
        if let Some(error) = begin(&mut state, Action::TerminateInstances) {
            return Err(error);
        }
        let held = |id: &InstanceId| state.instances.iter().any(|row| row.id == *id);
        if !ids.iter().all(held) {
            return Err(ApiError::answered("InvalidInstanceID.NotFound"));
        }
        state.terminates += 1;
        state
            .instances
            .iter_mut()
            .filter(|row| ids.contains(&row.id))
            .for_each(|row| row.state = InstanceState::ShuttingDown);
        if let Some(error) = state.read_after_terminate.pop_front() {
            state.read_armed.push_back(error);
        }
        finish(&mut state, Action::TerminateInstances, ())
    }
}
