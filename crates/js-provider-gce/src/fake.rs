//! [`GceApi`] over memory, for tests of code that calls the port: one project's answers,
//! built by hand from the documented response shapes, with failures scripted per method.
//! It holds no project id, key or endpoint of a real project. It answers every project it
//! is asked for with the same data, and the machine and accelerator types of every zone
//! it holds whatever zones are asked for (the calls it records say what was asked).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::api::{GceApi, Method};
use crate::error::ApiError;
use crate::model::{
    AcceleratorTypeInfo, AcceleratorTypeName, MachineTypeInfo, Project, QuotaMetric, Region,
    SkuPrice, Zone,
};

/// One call the fake answered or failed: the method, the project and the region asked
/// for (`None` for a call that names none), and the zones asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The method.
    pub method: Method,
    /// The project, for a Compute Engine call.
    pub project: Option<Project>,
    /// The region, for a price or quota call.
    pub region: Option<Region>,
    /// The zones, for a machine or accelerator type call.
    pub zones: Vec<Zone>,
}

#[derive(Default)]
struct State {
    machine_types: Vec<MachineTypeInfo>,
    accelerator_types: Vec<AcceleratorTypeInfo>,
    prices: BTreeMap<Region, Vec<SkuPrice>>,
    quotas: BTreeMap<Region, Vec<QuotaMetric>>,
    next: BTreeMap<Method, VecDeque<ApiError>>,
    always: BTreeMap<Method, ApiError>,
    calls: Vec<Call>,
}

/// One project in memory. The answers are in the order they were added, as a cloud's
/// answers are in its own order.
#[derive(Default)]
pub struct FakeGce {
    state: Mutex<State>,
}

impl FakeGce {
    /// A project with nothing in it: every list is empty and every region has no quota.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a machine type in one zone to `machineTypes.aggregatedList`.
    pub fn add_machine_type(&self, info: MachineTypeInfo) -> &Self {
        self.state().machine_types.push(info);
        self
    }

    /// Offers the accelerator type `name` in `zone`, at most `max_per_instance` to a
    /// machine (`acceleratorTypes.aggregatedList`).
    pub fn add_accelerator_type(&self, name: &str, zone: &Zone, max_per_instance: u32) -> &Self {
        self.state().accelerator_types.push(AcceleratorTypeInfo {
            name: AcceleratorTypeName(name.to_owned()),
            zone: zone.clone(),
            max_per_instance,
        });
        self
    }

    /// Adds a SKU to the prices of `region` (`services.skus.list`).
    pub fn add_price(&self, region: &Region, price: SkuPrice) -> &Self {
        self.state()
            .prices
            .entry(region.clone())
            .or_default()
            .push(price);
        self
    }

    /// Adds a quota metric to `region` (`regions.get`).
    pub fn add_quota(&self, region: &Region, quota: QuotaMetric) -> &Self {
        self.state()
            .quotas
            .entry(region.clone())
            .or_default()
            .push(quota);
        self
    }

    /// Makes the next call of `method` fail with `error`; several queue in order.
    pub fn fail_next(&self, method: Method, error: ApiError) -> &Self {
        self.state()
            .next
            .entry(method)
            .or_default()
            .push_back(error);
        self
    }

    /// Makes every call of `method` fail with `error`, after any [`Self::fail_next`].
    pub fn fail_always(&self, method: Method, error: ApiError) -> &Self {
        self.state().always.insert(method, error);
        self
    }

    /// Every call made so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// Records `call` and answers it with `answer` of the state, or the scripted failure.
    fn answer<T>(&self, call: Call, answer: impl FnOnce(&State) -> T) -> Result<T, ApiError> {
        let mut state = self.state();
        let method = call.method;
        state.calls.push(call);
        let scripted = state.next.get_mut(&method).and_then(VecDeque::pop_front);
        match scripted.or_else(|| state.always.get(&method).cloned()) {
            Some(error) => Err(error),
            None => Ok(answer(&state)),
        }
    }
}

impl GceApi for FakeGce {
    fn machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError> {
        let call = Call {
            method: Method::MachineTypes,
            project: Some(project.clone()),
            region: None,
            zones: zones.to_vec(),
        };
        self.answer(call, |state| state.machine_types.clone())
    }

    fn accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
        let call = Call {
            method: Method::AcceleratorTypes,
            project: Some(project.clone()),
            region: None,
            zones: zones.to_vec(),
        };
        self.answer(call, |state| state.accelerator_types.clone())
    }

    fn prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError> {
        let call = Call {
            method: Method::Prices,
            project: None,
            region: Some(region.clone()),
            zones: Vec::new(),
        };
        self.answer(call, |state| {
            state.prices.get(region).cloned().unwrap_or_default()
        })
    }

    fn region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError> {
        let call = Call {
            method: Method::RegionQuotas,
            project: Some(project.clone()),
            region: Some(region.clone()),
            zones: Vec::new(),
        };
        self.answer(call, |state| {
            state.quotas.get(region).cloned().unwrap_or_default()
        })
    }
}
