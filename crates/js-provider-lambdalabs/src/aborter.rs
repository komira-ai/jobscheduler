//! Stopping everything one scheduler launched, for a tool that is told to.
//!
//! The stop goes through [`LambdaProvider`]'s own release, the one the scaler uses: it
//! terminates only an instance whose tags and name are both the scheduler's, and reads it
//! back. The provider is built with launches disabled, so it cannot launch even if asked.
//! A tool outside this crate holds an [`Aborter`], not the raw API.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{ElasticProvider, Held, HostRef, Released, SchedulerId};

use crate::api::LambdaApi;
use crate::clock::{Clock, SystemClock};
use crate::config::ElasticConfig;
use crate::error::ApiError;
use crate::http::{HttpApi, SetupError};
use crate::key::ApiKey;
use crate::kind::API_TIMEOUT;
use crate::options::OptionTable;
use crate::provider::LambdaProvider;

/// An instance the abort would stop, or leaves alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The instance id.
    pub id: String,
    /// The instance type.
    pub kind: String,
    /// The region.
    pub region: String,
    /// The catalog's price in US cents per hour, when the catalog could be read.
    pub cents_per_hour: Option<u64>,
}

/// What an abort would do now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AbortPlan {
    /// Wholly the scheduler's and running or booting: these are stopped.
    pub running: Vec<Listed>,
    /// Some of the scheduler's marks, not all: listed, never touched.
    pub partial: Vec<Listed>,
}

/// A stop that was not confirmed. The words hold no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StopError(String);

/// Lists and stops the instances of one scheduler.
pub struct Aborter {
    api: Arc<dyn LambdaApi>,
    provider: LambdaProvider,
}

impl Aborter {
    /// An aborter for `scheduler`'s instances over `api`.
    pub fn new(api: Arc<dyn LambdaApi>, scheduler: SchedulerId, clock: Arc<dyn Clock>) -> Self {
        let config = ElasticConfig::with_account_key(scheduler, NonZeroUsize::MIN);
        let provider = LambdaProvider::new(Arc::clone(&api), clock, OptionTable::empty(), config);
        Self { api, provider }
    }

    /// An aborter over the real API at `base_url` (the rules of [`HttpApi::connect`]),
    /// with `key`.
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
        Ok(Self::new(
            Arc::new(api),
            scheduler,
            Arc::new(SystemClock::new()),
        ))
    }

    /// What an abort would do now. A catalog that cannot be read only costs the prices.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the instances cannot be listed whole.
    pub fn plan(&self) -> Result<AbortPlan, ApiError> {
        // With nothing booked every running instance of ours is an orphan to the sweep.
        let sweep = self.provider.sweep(&[])?;
        let offers = self.api.instance_types().unwrap_or_default();
        let prices: BTreeMap<String, u64> = offers
            .into_iter()
            .map(|offer| (offer.name, offer.price_cents_per_hour))
            .collect();
        let each = |held: Held| Listed {
            id: held.id.as_str().to_owned(),
            cents_per_hour: prices.get(&held.offer).copied(),
            kind: held.offer,
            region: held.zone.unwrap_or_default(),
        };
        Ok(AbortPlan {
            running: sweep.orphans.into_iter().map(each).collect(),
            partial: sweep.suspects.into_iter().map(each).collect(),
        })
    }

    /// Stops one instance, if it is wholly the scheduler's.
    ///
    /// # Errors
    ///
    /// [`StopError`] when the provider refuses the instance or gives no answer.
    pub fn stop(&self, id: &str) -> Result<(), StopError> {
        match self.provider.release(&HostRef(id.to_owned())) {
            Released::Gone | Released::Terminating(_) => Ok(()),
            Released::Refused(claim) => Err(StopError(format!(
                "refused: the instance is not wholly the scheduler's ({claim:?})"
            ))),
            Released::Unknown => Err(StopError("no answer; run the abort again".to_owned())),
        }
    }
}
