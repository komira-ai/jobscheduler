//! [`GceApi`] over memory, for tests of code that calls the port: one project's answers,
//! built by hand from the documented response shapes, with failures scripted per method.
//! It holds no project id, key or endpoint of a real project. It answers every project it
//! is asked for with the same data, and the machine and accelerator types of every zone
//! it holds whatever zones are asked for (the calls it records say what was asked).
//!
//! It also holds the instances of the launch side: the ones this fake inserted and the
//! ones someone else did, which a label list shows, and the zone operations an insert or a
//! delete starts. An insert with a `requestId` the fake still remembers answers the first
//! insert's operation and inserts nothing, as the cloud does within its window.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::api::{GceApi, Method};
use crate::error::{ApiError, ErrorReason};
use crate::launch::{
    InsertRequest, Instance, InstanceId, InstanceKey, InstanceRef, InstanceStatus, ListFilter,
    Operation, OperationRef, OperationStatus, Page,
};
use crate::model::{
    AcceleratorTypeInfo, AcceleratorTypeName, MachineTypeInfo, Project, QuotaMetric, Region,
    SkuPrice, Zone,
};

/// One call the fake answered or failed: the method, the project and the region asked
/// for (`None` for a call that names none), and the zones asked for (the zone of the
/// instance or operation, for a call of the launch side that names one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The method.
    pub method: Method,
    /// The project, for a Compute Engine call.
    pub project: Option<Project>,
    /// The region, for a price or quota call.
    pub region: Option<Region>,
    /// The zones, for a machine or accelerator type call, or the zone of a launch call.
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
    any_next: VecDeque<ApiError>,
    lose_after: BTreeMap<Method, VecDeque<ApiError>>,
    instances: Vec<Instance>,
    inserts: Vec<InsertRequest>,
    deletes: usize,
    request_ids: BTreeMap<String, Operation>,
    operations: BTreeMap<String, Operation>,
    failing_operations: VecDeque<ErrorReason>,
    next_id: u64,
    next_operation: u64,
    page_size: Option<usize>,
    fail_page: Option<usize>,
    unreachable: bool,
    read_after_delete: VecDeque<ApiError>,
    read_armed: VecDeque<ApiError>,
}

impl State {
    /// Records `call` and takes the failure scripted for it: the next call of any kind
    /// first, then the next of this method, then the standing one.
    fn begin(&mut self, call: Call) -> Option<ApiError> {
        let method = call.method;
        self.calls.push(call);
        self.any_next
            .pop_front()
            .or_else(|| self.next.get_mut(&method).and_then(VecDeque::pop_front))
            .or_else(|| self.always.get(&method).cloned())
    }

    /// Answers `value`, unless the answer of this call is scripted to be lost: the call has
    /// taken effect and the caller is told the error.
    fn finish<T>(&mut self, method: Method, value: T) -> Result<T, ApiError> {
        match self
            .lose_after
            .get_mut(&method)
            .and_then(VecDeque::pop_front)
        {
            Some(error) => Err(error),
            None => Ok(value),
        }
    }

    /// A new operation in `zone` of `project`, pending, about `target`.
    fn operation(
        &mut self,
        project: &Project,
        zone: &Zone,
        target: Option<InstanceId>,
    ) -> Operation {
        self.next_operation += 1;
        Operation {
            reference: OperationRef {
                project: project.clone(),
                zone: zone.clone(),
                name: format!("operation-{}", self.next_operation),
            },
            status: OperationStatus::Pending,
            target_id: target,
            error_codes: Vec::new(),
        }
    }

    /// The position of the instance `instance` names, in its zone.
    fn position(&self, instance: &InstanceRef) -> Option<usize> {
        self.instances.iter().position(|row| {
            let same = match &instance.instance {
                InstanceKey::Name(name) => row.name == name.as_str(),
                InstanceKey::Id(id) => row.id == *id,
            };
            same & (row.zone == instance.zone)
        })
    }

    /// Inserts what `request` asks for and answers its operation, as it was when the insert
    /// answered; the operation as a later read shows it is kept.
    fn insert(&mut self, request: &InsertRequest) -> Result<Operation, ApiError> {
        if let Some(seen) = self.request_ids.get(request.request_id.as_str()) {
            return Ok(seen.clone());
        }
        let taken = self
            .instances
            .iter()
            .any(|row| (row.name == request.name.as_str()) & (row.zone == request.zone));
        if taken {
            return Err(ApiError::answered("alreadyExists"));
        }
        let (answer, done) = match self.failing_operations.pop_front() {
            Some(code) => {
                let answer = self.operation(&request.project, &request.zone, None);
                let done = Operation {
                    status: OperationStatus::Done,
                    error_codes: vec![code],
                    ..answer.clone()
                };
                (answer, done)
            }
            None => {
                self.next_id += 1;
                let id = InstanceId::numbered(self.next_id);
                self.instances.push(Instance {
                    id: id.clone(),
                    name: request.name.as_str().to_owned(),
                    status: InstanceStatus::Provisioning,
                    labels: request.labels.clone(),
                    machine_type: request.machine_type.clone(),
                    zone: request.zone.clone(),
                });
                self.inserts.push(request.clone());
                let answer = self.operation(&request.project, &request.zone, Some(id));
                let done = Operation {
                    status: OperationStatus::Done,
                    ..answer.clone()
                };
                (answer, done)
            }
        };
        self.request_ids
            .insert(request.request_id.as_str().to_owned(), answer.clone());
        self.operations.insert(answer.reference.name.clone(), done);
        Ok(answer)
    }
}

/// One project in memory. The answers are in the order they were added, as a cloud's
/// answers are in its own order; instances are listed in the order they were inserted or
/// added, whatever their status.
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

    /// Makes the next call of `method` fail with `error`, nothing having happened; several
    /// queue in order. With `ApiError::answered(reason)` it is the cloud refusing the call
    /// with that reason.
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

    /// Makes the next call of any kind fail with `error`, nothing having happened; several
    /// queue in order. It is the call whose answer was lost before it landed.
    pub fn fail_next_call(&self, error: ApiError) -> &Self {
        self.state().any_next.push_back(error);
        self
    }

    /// Makes the next call of `method` take effect and then answer `error`: the answer is
    /// lost after the call landed. Several queue in order.
    pub fn lose_after_next(&self, method: Method, error: ApiError) -> &Self {
        self.state()
            .lose_after
            .entry(method)
            .or_default()
            .push_back(error);
        self
    }

    /// Makes the operation of the next insert that is not a repeat finish with the error
    /// `code` (such as `ZONE_RESOURCE_POOL_EXHAUSTED`): the insert answers a pending
    /// operation, inserts no instance, and a read of the operation shows it done with that
    /// error. Several queue in order.
    pub fn fail_next_operation(&self, code: &str) -> &Self {
        self.state()
            .failing_operations
            .push_back(ErrorReason::new(code));
        self
    }

    /// Makes the first [`GceApi::get_instance`] after the next [`GceApi::delete_instance`]
    /// that takes effect fail with `error`. Several queue in order, one per delete.
    pub fn fail_read_after_next_delete(&self, error: ApiError) -> &Self {
        self.state().read_after_delete.push_back(error);
        self
    }

    /// Makes [`GceApi::list_instances`] answer in pages of `size` instances (at least one):
    /// each page names the token of the next, which the caller follows.
    pub fn paginate(&self, size: usize) -> &Self {
        self.state().page_size = Some(size.max(1));
        self
    }

    /// Makes the next [`GceApi::list_instances`] that asks for the page `page` (the first
    /// is 1) fail with [`ApiError::Incomplete`]. It fails one call.
    pub fn fail_page(&self, page: usize) -> &Self {
        self.state().fail_page = Some(page);
        self
    }

    /// Makes the next [`GceApi::list_instances`] answer a page that names a scope it could
    /// not reach.
    pub fn answer_unreachable_once(&self) -> &Self {
        self.state().unreachable = true;
        self
    }

    /// Makes the fake forget every `requestId` it has seen, as the cloud does after its
    /// window: the same request id inserts again.
    pub fn forget_request_ids(&self) -> &Self {
        self.state().request_ids.clear();
        self
    }

    /// Adds an instance that someone else inserted. It is not counted in
    /// [`Self::inserted`].
    pub fn add_instance(&self, instance: Instance) -> &Self {
        self.state().instances.push(instance);
        self
    }

    /// Puts the instance `id` in `status`; nothing happens when there is none.
    pub fn set_status(&self, id: &InstanceId, status: InstanceStatus) -> &Self {
        self.state()
            .instances
            .iter_mut()
            .filter(|instance| instance.id == *id)
            .for_each(|instance| instance.status = status.clone());
        self
    }

    /// Makes the cloud forget the instance `id`: a read of it answers `notFound`.
    pub fn remove_instance(&self, id: &InstanceId) -> &Self {
        self.state().instances.retain(|instance| instance.id != *id);
        self
    }

    /// Every instance the fake holds, in order.
    pub fn instances(&self) -> Vec<Instance> {
        self.state().instances.clone()
    }

    /// Every insert that created an instance, in order. A repeat of a request id is not in
    /// it.
    pub fn inserts(&self) -> Vec<InsertRequest> {
        self.state().inserts.clone()
    }

    /// How many instances the fake inserted.
    pub fn inserted(&self) -> usize {
        self.state().inserts.len()
    }

    /// How many delete calls reached the fake and took effect (a call that failed before
    /// it was sent does not count; one whose answer was lost does).
    pub fn deletes(&self) -> usize {
        self.state().deletes
    }

    /// Every call made so far, in order, failed ones included.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// Every method called so far, in order, failed calls included.
    pub fn methods(&self) -> Vec<Method> {
        self.state().calls.iter().map(|call| call.method).collect()
    }

    /// Records `call` and answers it with `answer` of the state, or the scripted failure.
    fn answer<T>(&self, call: Call, answer: impl FnOnce(&State) -> T) -> Result<T, ApiError> {
        let mut state = self.state();
        let method = call.method;
        if let Some(error) = state.begin(call) {
            return Err(error);
        }
        let value = answer(&state);
        state.finish(method, value)
    }
}

/// The record of a launch-side call of `method` in `zone` of `project`.
fn launch_call(method: Method, project: &Project, zone: Option<&Zone>) -> Call {
    Call {
        method,
        project: Some(project.clone()),
        region: None,
        zones: zone.into_iter().cloned().collect(),
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

    fn insert_instance(&self, request: &InsertRequest) -> Result<Operation, ApiError> {
        let mut state = self.state();
        let call = launch_call(
            Method::InsertInstance,
            &request.project,
            Some(&request.zone),
        );
        if let Some(error) = state.begin(call) {
            return Err(error);
        }
        let operation = state.insert(request)?;
        state.finish(Method::InsertInstance, operation)
    }

    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        let mut state = self.state();
        let call = launch_call(Method::GetInstance, &instance.project, Some(&instance.zone));
        if let Some(error) = state.begin(call) {
            return Err(error);
        }
        if let Some(error) = state.read_armed.pop_front() {
            return Err(error);
        }
        let row = match state.position(instance) {
            Some(at) => state.instances[at].clone(),
            None => return Err(ApiError::answered("notFound")),
        };
        state.finish(Method::GetInstance, row)
    }

    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        let mut state = self.state();
        let call = launch_call(
            Method::DeleteInstance,
            &instance.project,
            Some(&instance.zone),
        );
        if let Some(error) = state.begin(call) {
            return Err(error);
        }
        let Some(at) = state.position(instance) else {
            return Err(ApiError::answered("notFound"));
        };
        state.deletes += 1;
        state.instances[at].status = InstanceStatus::Stopping;
        let target = Some(state.instances[at].id.clone());
        let operation = state.operation(&instance.project, &instance.zone, target);
        let done = Operation {
            status: OperationStatus::Done,
            ..operation.clone()
        };
        state
            .operations
            .insert(operation.reference.name.clone(), done);
        if let Some(error) = state.read_after_delete.pop_front() {
            state.read_armed.push_back(error);
        }
        state.finish(Method::DeleteInstance, operation)
    }

    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        let mut state = self.state();
        let call = launch_call(Method::ListInstances, &filter.project, None);
        if let Some(error) = state.begin(call) {
            return Err(error);
        }
        let rows: Vec<Instance> = state
            .instances
            .iter()
            .filter(|row| filter.labels.iter().all(|label| row.labels.contains(label)))
            .cloned()
            .collect();
        let start: usize = filter
            .page_token
            .as_deref()
            .and_then(|token| token.strip_prefix("page-"))
            .and_then(|at| at.parse().ok())
            .unwrap_or(0);
        let size = state.page_size.unwrap_or(usize::MAX);
        let page = start / size.max(1) + 1;
        if state.fail_page == Some(page) {
            state.fail_page = None;
            return Err(ApiError::Incomplete);
        }
        let end = start.saturating_add(size).min(rows.len());
        let items = rows
            .get(start..end)
            .map(<[Instance]>::to_vec)
            .unwrap_or_default();
        let next_page_token = (end < rows.len()).then(|| format!("page-{end}"));
        let unreachable = std::mem::take(&mut state.unreachable);
        let answer = Page {
            items,
            next_page_token,
            unreachable,
        };
        state.finish(Method::ListInstances, answer)
    }

    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError> {
        let mut state = self.state();
        let call = launch_call(
            Method::GetOperation,
            &operation.project,
            Some(&operation.zone),
        );
        if let Some(error) = state.begin(call) {
            return Err(error);
        }
        let Some(found) = state.operations.get(&operation.name).cloned() else {
            return Err(ApiError::answered("notFound"));
        };
        state.finish(Method::GetOperation, found)
    }
}
