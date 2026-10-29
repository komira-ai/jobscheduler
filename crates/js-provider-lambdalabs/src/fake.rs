//! [`LambdaApi`] over memory, for tests of code that calls the API: the same answers the
//! real client gives, with failures scripted per call.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::api::LambdaApi;
use crate::error::{ApiError, ErrorCode};
use crate::model::{Instance, InstanceId, InstanceTypeOffer, LaunchRequest, Status};

/// One of the calls of [`LambdaApi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Call {
    /// [`LambdaApi::instance_types`].
    InstanceTypes,
    /// [`LambdaApi::list_instances`].
    ListInstances,
    /// [`LambdaApi::get_instance`].
    GetInstance,
    /// [`LambdaApi::launch`].
    Launch,
    /// [`LambdaApi::terminate`].
    Terminate,
}

/// What a terminate call does to the instance it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateEffect {
    /// The instance becomes `terminating` and the answer lists it.
    Terminating,
    /// The instance is gone at once (a later read answers 404) and the answer lists it.
    Removed,
    /// Nothing happens and the answer lists nothing, as a 200 with an empty list.
    Nothing,
}

#[derive(Default)]
struct State {
    offers: Vec<InstanceTypeOffer>,
    instances: Vec<Instance>,
    faults: HashMap<Call, VecDeque<Option<ApiError>>>,
    lost: HashMap<Call, VecDeque<ApiError>>,
    launch_counts: VecDeque<usize>,
    terminate_effect: Option<TerminateEffect>,
    launched: usize,
    calls: Vec<Call>,
}

/// An account in memory. A read of an instance it does not hold fails as the real API
/// does: 404 with `global/object-does-not-exist`. A launch of a type it does not offer
/// fails as that 404, one into a region without capacity as
/// `instance-operations/launch/insufficient-capacity`, and one with a name over 64
/// characters as `global/invalid-parameters`.
#[derive(Default)]
pub struct FakeLambda {
    state: Mutex<State>,
}

impl FakeLambda {
    /// An account with no instance types and no instances.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the instance types the account offers.
    pub fn set_offers(&self, offers: Vec<InstanceTypeOffer>) {
        self.state().offers = offers;
    }

    /// Adds an instance, after the ones already held.
    pub fn add_instance(&self, instance: Instance) {
        self.state().instances.push(instance);
    }

    /// Makes the next `call` that has no earlier scripted failure fail with `error`;
    /// failures are used in the order they were scripted.
    pub fn fail_next(&self, call: Call, error: ApiError) {
        self.state()
            .faults
            .entry(call)
            .or_default()
            .push_back(Some(error));
    }

    /// Lets the next `call` that has no earlier scripted outcome succeed, so that a
    /// failure scripted after it falls on the call after.
    pub fn pass_next(&self, call: Call) {
        self.state().faults.entry(call).or_default().push_back(None);
    }

    /// Makes the next `call` that has no earlier such script take effect and then fail
    /// with `error`, as a call whose answer was lost does.
    pub fn lose_response(&self, call: Call, error: ApiError) {
        self.state().lost.entry(call).or_default().push_back(error);
    }

    /// Makes the next launch start `count` instances instead of one (none, or several).
    pub fn launch_creates(&self, count: usize) {
        self.state().launch_counts.push_back(count);
    }

    /// Sets what terminate calls do from now on.
    pub fn set_terminate_effect(&self, effect: TerminateEffect) {
        self.state().terminate_effect = Some(effect);
    }

    /// Sets the status of the instance `id`, if the account holds it.
    pub fn set_status(&self, id: &InstanceId, status: Status) {
        self.update_instance(id, &|instance| instance.status = status.clone());
    }

    /// Changes the instance `id` in place, if the account holds it, as an edit made in
    /// the provider's console would.
    pub fn update_instance(&self, id: &InstanceId, edit: &dyn Fn(&mut Instance)) {
        if let Some(instance) = self.state().instances.iter_mut().find(|i| i.id == *id) {
            edit(instance);
        }
    }

    /// Drops the instance `id`, so that a read of it answers 404.
    pub fn remove_instance(&self, id: &InstanceId) {
        self.state().instances.retain(|i| i.id != *id);
    }

    /// The instances held now.
    pub fn instances(&self) -> Vec<Instance> {
        self.state().instances.clone()
    }

    /// How many instances launch calls have started so far.
    pub fn launched(&self) -> usize {
        self.state().launched
    }

    /// How many times `call` was made.
    pub fn count(&self, call: Call) -> usize {
        self.state().calls.iter().filter(|c| **c == call).count()
    }

    /// The calls made so far, in order, failed ones included.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// Records `call` and returns its scripted failure, if one is due.
    fn enter(&self, call: Call) -> Result<MutexGuard<'_, State>, ApiError> {
        let mut state = self.state();
        state.calls.push(call);
        match state.faults.get_mut(&call).and_then(VecDeque::pop_front) {
            Some(Some(error)) => Err(error),
            Some(None) | None => Ok(state),
        }
    }
}

impl LambdaApi for FakeLambda {
    fn instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError> {
        let mut offers = self.enter(Call::InstanceTypes)?.offers.clone();
        offers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(offers)
    }

    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        Ok(self.enter(Call::ListInstances)?.instances.clone())
    }

    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError> {
        let state = self.enter(Call::GetInstance)?;
        state
            .instances
            .iter()
            .find(|instance| instance.id == *id)
            .cloned()
            .ok_or_else(missing)
    }

    fn launch(&self, request: &LaunchRequest) -> Result<Vec<InstanceId>, ApiError> {
        let mut state = self.enter(Call::Launch)?;
        let Some(offer) = state
            .offers
            .iter()
            .find(|o| o.name == request.instance_type)
        else {
            return Err(missing());
        };
        if !offer.available_in(&request.region) {
            return Err(refusal(400, ErrorCode::InsufficientCapacity));
        }
        if request.name.is_empty() || request.name.len() > 64 {
            return Err(refusal(400, ErrorCode::InvalidParameters));
        }
        let count = state.launch_counts.pop_front().unwrap_or(1);
        let mut ids = Vec::new();
        for _ in 0..count {
            state.launched += 1;
            let id = InstanceId::made(format!("fake{:04}", state.launched));
            state.instances.push(Instance {
                id: id.clone(),
                name: Some(request.name.clone()),
                status: Status::Booting,
                region: request.region.clone(),
                instance_type: request.instance_type.clone(),
                ip: None,
                private_ip: None,
                hostname: None,
                first_healthy: None,
                tags: request.tags.clone(),
            });
            ids.push(id);
        }
        match state
            .lost
            .get_mut(&Call::Launch)
            .and_then(VecDeque::pop_front)
        {
            Some(error) => Err(error),
            None => Ok(ids),
        }
    }

    fn terminate(&self, id: &InstanceId) -> Result<Vec<InstanceId>, ApiError> {
        let mut state = self.enter(Call::Terminate)?;
        let effect = state
            .terminate_effect
            .unwrap_or(TerminateEffect::Terminating);
        let Some(instance) = state.instances.iter_mut().find(|i| i.id == *id) else {
            return Err(missing());
        };
        match effect {
            TerminateEffect::Nothing => return Ok(Vec::new()),
            TerminateEffect::Terminating => instance.status = Status::Terminating,
            TerminateEffect::Removed => state.instances.retain(|i| i.id != *id),
        }
        match state
            .lost
            .get_mut(&Call::Terminate)
            .and_then(VecDeque::pop_front)
        {
            Some(error) => Err(error),
            None => Ok(vec![id.clone()]),
        }
    }
}

fn refusal(status: u16, code: ErrorCode) -> ApiError {
    ApiError::Api {
        status,
        code,
        request_id: None,
    }
}

fn missing() -> ApiError {
    refusal(404, ErrorCode::ObjectDoesNotExist)
}
