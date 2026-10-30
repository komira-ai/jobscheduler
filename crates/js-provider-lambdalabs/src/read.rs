//! The read-only side of the API, for a caller that must be unable to spend: a handle
//! that can list and cannot launch or terminate.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_elastic::{Claim, Held, HostStatus, SchedulerId};

use crate::api::LambdaApi;
use crate::error::ApiError;
use crate::http::{HttpApi, SetupError};
use crate::key::ApiKey;
use crate::kind::API_TIMEOUT;
use crate::model::{Instance, InstanceTypeOffer};
use crate::own::Owner;
use crate::provider::held_of;

/// The calls of [`LambdaApi`] that only read. A `dyn LambdaRead` has no launch and no
/// terminate to call, so code that holds one cannot spend: every [`LambdaApi`] is one, by
/// the blanket impl, and the type of the handle is what limits it.
pub trait LambdaRead: Send + Sync {
    /// [`LambdaApi::list_instances`].
    ///
    /// # Errors
    ///
    /// [`ApiError`] as that call fails.
    fn read_instances(&self) -> Result<Vec<Instance>, ApiError>;

    /// [`LambdaApi::instance_types`].
    ///
    /// # Errors
    ///
    /// [`ApiError`] as that call fails.
    fn read_instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError>;
}

impl<T: LambdaApi + ?Sized> LambdaRead for T {
    fn read_instances(&self) -> Result<Vec<Instance>, ApiError> {
        self.list_instances()
    }

    fn read_instance_types(&self) -> Result<Vec<InstanceTypeOffer>, ApiError> {
        self.instance_types()
    }
}

/// The instances of an account that carry the marks of one scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tagged {
    /// Every mark whole: the scheduler's own, terminated ones left out.
    pub ours: Vec<Held>,
    /// Some marks, not all: they may be the scheduler's, renamed or retagged. They bill.
    pub suspects: Vec<Held>,
}

/// A read-only view of one scheduler's instances.
pub struct Reader {
    api: Arc<dyn LambdaRead>,
    owner: Owner,
}

impl Reader {
    /// A view of what `scheduler` launched, over `api`.
    pub fn new(api: Arc<dyn LambdaRead>, scheduler: SchedulerId) -> Self {
        Self {
            api,
            owner: Owner::new(scheduler),
        }
    }

    /// A view over the real API at `base_url` (the rules of [`HttpApi::connect`]), with
    /// `key`. The client is held only as a [`LambdaRead`].
    ///
    /// # Errors
    ///
    /// [`SetupError`] when the URL is unusable.
    pub fn connect(
        base_url: &str,
        key: &ApiKey,
        scheduler: SchedulerId,
    ) -> Result<Self, SetupError> {
        let api = HttpApi::connect(base_url, key, None, API_TIMEOUT)?;
        Ok(Self::new(Arc::new(api), scheduler))
    }

    /// The instances of the account that carry the scheduler's marks, whole or partial,
    /// in every region. An instance with none of the marks is another user's and is left
    /// out.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the list cannot be had whole.
    pub fn tagged(&self) -> Result<Tagged, ApiError> {
        let mut found = Tagged::default();
        for instance in self.api.read_instances()? {
            if instance.status == HostStatus::Terminated {
                continue;
            }
            match self.owner.claim(&instance) {
                Claim::Ours => found.ours.push(held_of(&instance)),
                Claim::Suspect => found.suspects.push(held_of(&instance)),
                Claim::Foreign => {}
            }
        }
        Ok(found)
    }

    /// The price of each instance type in US cents per hour, by name.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the catalog cannot be had.
    pub fn prices(&self) -> Result<BTreeMap<String, u64>, ApiError> {
        let offers = self.api.read_instance_types()?;
        Ok(offers
            .into_iter()
            .map(|offer| (offer.name, offer.price_cents_per_hour))
            .collect())
    }
}
