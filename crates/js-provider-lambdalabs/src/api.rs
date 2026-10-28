//! The read calls the provider makes.

use crate::error::ApiError;
use crate::model::{Instance, InstanceId, InstanceTypeOffer};

/// The read side of Lambda's API. Every call blocks, answers or fails within a bound, and
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
    /// status 404; [`ApiError::is_absent`] tells that case from every other failure.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the call fails or the answer cannot be read.
    fn get_instance(&self, id: &InstanceId) -> Result<Instance, ApiError>;
}
