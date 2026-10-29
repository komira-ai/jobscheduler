//! The calls the provider makes.

use crate::error::ApiError;
use crate::model::{Instance, InstanceId, InstanceTypeOffer, LaunchRequest};

/// Lambda's API. Every call blocks, answers or fails within a bound, and
/// makes exactly one request: nothing is retried here, and a failure is never turned into
/// an empty answer.
pub trait LambdaApi: Send + Sync {
    /// Every instance type with its GPU facts, its price and the regions that have
    /// capacity now, ordered by type name.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError>;

    /// Every instance of the account, in the order the API gave. Whether the API lists
    /// terminated instances is not documented, so an instance missing from this list is
    /// not evidence that it is gone.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails, the answer cannot be read, or the answer says it
    /// holds only one page ([`ApiError::Incomplete`]).
    fn list_instances(&self) -> Result<Vec<Instance>, ApiError>;

    /// One instance. An instance the API does not know is
    /// [`ApiError::Api`] with [`ErrorCode::ObjectDoesNotExist`](crate::ErrorCode) and
    /// status 404; [`ApiError::is_absent`] tells that case from every other failure (it
    /// is for this call only), and the instance returned is always the one asked for.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError>;

    /// Launches one instance. The answer lists the ids the API says it started; a lost
    /// answer is an error even though the instance may exist, so the caller lists before
    /// it asks again. The call checks nothing about what it launches: the guards live in
    /// [`Elastic`](crate::Elastic).
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn launch(&self, request: &LaunchRequest) -> Result<Vec<InstanceId>, ApiError>;

    /// Asks the API to terminate one instance (one id per call: a bad id in a batch may
    /// fail the whole call). The answer lists the ids the API says it terminated, which
    /// may be none; it is not proof that the instance is gone. The call does not check
    /// that the instance is the caller's: [`Elastic::release`](crate::Elastic::release)
    /// does.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn terminate(&self, id: &InstanceId) -> Result<Vec<InstanceId>, ApiError>;
}
