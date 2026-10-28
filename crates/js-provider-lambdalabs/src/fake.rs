//! [`LambdaApi`] over memory, for tests of code that calls the API: the same answers the
//! real client gives, with failures scripted per call.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::api::LambdaApi;
use crate::error::{ApiError, ErrorCode};
use crate::model::{Instance, InstanceId, InstanceTypeOffer};

/// One of the calls of [`LambdaApi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Call {
    /// [`LambdaApi::instance_types`].
    InstanceTypes,
    /// [`LambdaApi::list_instances`].
    ListInstances,
    /// [`LambdaApi::get_instance`].
    GetInstance,
}

#[derive(Default)]
struct State {
    offers: Vec<InstanceTypeOffer>,
    instances: Vec<Instance>,
    faults: HashMap<Call, VecDeque<ApiError>>,
    calls: Vec<Call>,
}

/// An account in memory. A read of an instance it does not hold fails as the real API
/// does: 404 with `global/object-does-not-exist`.
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
            .push_back(error);
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
            Some(error) => Err(error),
            None => Ok(state),
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
            .ok_or(ApiError::Api {
                status: 404,
                code: ErrorCode::ObjectDoesNotExist,
                request_id: None,
            })
    }
}
