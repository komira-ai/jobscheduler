//! The catalogs of the server's launching machine providers, as admission and the pick of
//! an offer read them: from a cache that a refresh fills once per tick, never from a call
//! to the provider for each question (design 5.6.1, "Catalog freshness").
//!
//! [`Catalogs`] holds, per provider id, the provider, its [`CatalogCache`] and how long a
//! cool-down of one of its offers lasts, and one [`Cooldowns`] for every offer. Each cache
//! is behind a lock taken only to read it or to record a fetch, never across a call to the
//! provider, so a read ([`Catalogs::view`], [`Catalogs::offers_for_pass2`]) answers at once
//! while a fetch is in flight.
//!
//! - [`Catalogs::refresh_due`] fetches each catalog whose cache says a refresh is due, at
//!   the server's tick: the tick spaces the fetches, so a cloud that keeps failing is asked
//!   once per tick, and the first failure of an outage is logged once.
//! - [`Catalogs::acquired`] invalidates a provider's cache when it refuses an acquire for
//!   capacity or quota: its offers are out of date. They stay readable until the fetch it
//!   makes due replaces them.
//! - [`Catalogs::fetches`] counts the fetches that completed, so the scaler sees that a
//!   refresh completed and asks again for a request every offer was refused for.
//! - A catalog never fetched reads as [`CatalogView::Unread`], which admission counts as
//!   "may offer"; a stale catalog is still the offers it holds. A restart starts with every
//!   catalog never fetched, and with only the cool-downs the scaler's records hold.
//! - [`Catalogs::cool_down`] and [`Catalogs::offers_for_pass2`] are the cool-down and the
//!   view of the catalog the pick of an offer (design 5.6, pass 2) is to read. The scaler
//!   cools an offer down when the hello of a machine acquired for it does not match it
//!   (design 5.6.1, step 6): it writes the cool-down into its records, and the records'
//!   cool-downs are handed to [`Catalogs::hold`] when the scaler is built and after every
//!   write of them, so a restart keeps them. The scaler's pick reads the offers of the pool's checks, not this
//!   view.
//!
//! Caches are keyed by [`CapacityProvider::id`]. The server has at most one launching
//! provider today (one elastic pool); two providers of one kind would share an id.
//!
//! The `js check` credential probe (the pool's checks) does not read through here: its point
//! is a live answer from the provider, so it calls the provider itself.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::{
    Acquired, CapacityProvider, Catalog, CatalogCache, Cooldowns, Instant, Offer, OfferKey,
    ProviderError, Recorded, Refusal, RefusalCode,
};
use js_sched::admission_ever::CatalogView;
use js_types::Clock;
use js_types::ProviderId;
use tracing::warn;

use crate::clock::SharedClock;

/// One provider's entry.
struct Entry {
    provider: Arc<dyn CapacityProvider>,
    cache: Mutex<CatalogCache>,
    offer_cooldown: Duration,
    /// How many fetches of its catalog have completed.
    fetches: AtomicU64,
}

impl Entry {
    fn cache(&self) -> MutexGuard<'_, CatalogCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The cached catalogs and the offer cool-downs of the server's launching machine
/// providers.
#[derive(Default)]
pub struct Catalogs {
    providers: BTreeMap<ProviderId, Entry>,
    cooldowns: Mutex<Cooldowns>,
}

impl Catalogs {
    /// No provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// These catalogs and that of `provider`, under its id, fetched again `catalog_refresh`
    /// after each fetch that worked, whose offers cool down for `offer_cooldown`. Nothing is
    /// fetched until the first [`Catalogs::refresh_due`]. A second provider with the same id
    /// takes the place of the first.
    #[must_use]
    pub fn with(
        mut self,
        provider: Arc<dyn CapacityProvider>,
        catalog_refresh: Duration,
        offer_cooldown: Duration,
    ) -> Self {
        let entry = Entry {
            cache: Mutex::new(CatalogCache::new(catalog_refresh)),
            offer_cooldown,
            provider,
            fetches: AtomicU64::new(0),
        };
        self.providers.insert(entry.provider.id(), entry);
        self
    }

    /// The catalog of `provider` as admission reads it at `now`: [`CatalogView::Unread`]
    /// when none has been fetched (or the provider is not one of these), else the offers of
    /// the last one fetched, stale or not, cooling down or not (a cooled-down offer will
    /// cool off, so it still says the provider could serve). Calls no provider.
    pub fn view(&self, provider: &ProviderId, now: Instant) -> CatalogView {
        let Some(entry) = self.providers.get(provider) else {
            return CatalogView::Unread;
        };
        match entry.cache().read(now) {
            Catalog::Never => CatalogView::Unread,
            Catalog::Offers { offers, .. } => CatalogView::Offers(offers.to_vec()),
        }
    }

    /// When the catalog of `provider` that is read now was fetched, and how many offers it
    /// holds; `None` when none has been fetched (or the provider is not one of these). A
    /// failed fetch moves neither. Calls no provider.
    pub fn fetched(&self, provider: &ProviderId) -> Option<(Instant, usize)> {
        self.providers.get(provider)?.cache().fetched()
    }

    /// The offers of `provider` that pass 2 may pick from at `now`: those of the last
    /// catalog fetched, in its order, less each one cooling down. None when no catalog has
    /// been fetched. Calls no provider.
    pub fn offers_for_pass2(&self, provider: &ProviderId, now: Instant) -> Vec<Offer> {
        let offers = match self.view(provider, now) {
            CatalogView::Unread => Vec::new(),
            CatalogView::Offers(offers) => offers,
        };
        let cooldowns = self.cooldowns();
        offers
            .into_iter()
            .filter(|offer| !cooldowns.is_cooled_down(&OfferKey::of(provider, offer), now))
            .collect()
    }

    /// Cools the offer `key` down from `now` for its provider's `offer_cooldown`, and forgets
    /// the cool-downs that have ended. A key of a provider that is not one of these is not
    /// kept: none of its offers is ever read here.
    pub fn cool_down(&self, key: OfferKey, now: Instant) {
        let Some(entry) = self.providers.get(&key.provider) else {
            return;
        };
        let mut cooldowns = self.cooldowns();
        cooldowns.forget_expired(now);
        cooldowns.cool_down(key, now, entry.offer_cooldown);
    }

    /// Takes in the cool-downs `cooled`, each until its end (decided earlier and kept in the
    /// scaler's records, [`js_scaler::Records::offer_cooldowns`]); a key already cooling down
    /// keeps the later end. A key of a provider that is not one of these is kept and never
    /// read: none of its offers is read here.
    pub fn hold(&self, cooled: &Cooldowns) {
        self.cooldowns().merge(cooled);
    }

    /// How many fetches of `provider`'s catalog have completed (a failed one is not
    /// counted); zero for a provider that is not one of these. Pass 2 reads a change of the
    /// count as a completed catalog refresh (design 5.6, step 5): a request waiting
    /// `acquire_retry` after every offer was refused for it is asked about again at once.
    pub fn fetches(&self, provider: &ProviderId) -> u64 {
        self.providers
            .get(provider)
            .map_or(0, |entry| entry.fetches.load(Ordering::Relaxed))
    }

    /// Takes in `provider`'s answer to an acquire: a refusal for capacity or quota
    /// invalidates its cache, so the next [`Catalogs::refresh_due`] fetches it, and the
    /// offers stay readable until then. Any other answer changes nothing: a `shape` or
    /// `denied` refusal is about the request or the key, not the catalog, and no answer
    /// (`unknown`, `deferred`) says nothing.
    pub fn acquired(&self, provider: &ProviderId, answer: &Acquired) {
        if let Acquired::Refused(Refusal {
            code: RefusalCode::Capacity | RefusalCode::Quota,
            ..
        }) = answer
            && let Some(entry) = self.providers.get(provider)
        {
            entry.cache().invalidate();
        }
    }

    /// Fetches, one after another, the catalog of each provider whose cache says a refresh
    /// is due at `now`, and records what came. The cache's lock is not held while the
    /// provider is called, and the call runs on a task of its own, so a panic in it is a
    /// failed fetch. A failure keeps the last good catalog and is fetched again at the next
    /// call; the first failure of an outage is logged as a warning, the ones after it are
    /// not. Called once per tick of the server, on the scaler's task before the scaler's
    /// own tick, so an acquire (and its invalidation) never runs while a fetch is in flight.
    pub async fn refresh_due(&self, now: Instant) {
        for (id, entry) in &self.providers {
            let due = entry.cache().refresh_due(now);
            if !due {
                continue;
            }
            let fetched = fetch(Arc::clone(&entry.provider)).await;
            let recorded = entry.cache().record(now, fetched);
            if recorded == Recorded::Fetched {
                entry.fetches.fetch_add(1, Ordering::Relaxed);
            }
            if recorded == (Recorded::Failed { first: true }) {
                let error = entry.cache().last_error().unwrap_or_default().to_owned();
                warn!(
                    provider = %id,
                    %error,
                    "cannot read the provider's catalog; the last one read stays in use and the read is tried again at each tick"
                );
            }
        }
    }

    fn cooldowns(&self) -> MutexGuard<'_, Cooldowns> {
        self.cooldowns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The text of a fetch whose provider call panicked.
const FETCH_PANICKED: &str = "the catalog call panicked";

/// `provider`'s catalog, awaited on a task of its own so that a panic in the provider's
/// call is a failed fetch (`the catalog call panicked`) and does not end the caller's task
/// (the scaler's, which must go on ticking).
async fn fetch(provider: Arc<dyn CapacityProvider>) -> Result<Vec<Offer>, ProviderError> {
    let fetched = tokio::spawn(async move { provider.catalog().await }).await;
    fetched.unwrap_or_else(|_| Err(ProviderError::new(FETCH_PANICKED)))
}

/// The server's catalogs with its clock, for the parts of the server that read them at the
/// time of the clock: admission and the tick.
#[derive(Clone)]
pub(crate) struct Cached {
    catalogs: Arc<Catalogs>,
    clock: SharedClock,
}

impl Cached {
    /// `catalogs`, read at the time of `clock`.
    pub(crate) fn new(catalogs: Arc<Catalogs>, clock: SharedClock) -> Self {
        Self { catalogs, clock }
    }

    /// The catalogs.
    pub(crate) fn catalogs(&self) -> &Arc<Catalogs> {
        &self.catalogs
    }

    /// The clock's time as the catalogs take it.
    pub(crate) fn now(&self) -> Instant {
        Instant::from_millis(self.clock.now().millis())
    }

    /// [`Catalogs::refresh_due`] at the clock's time: the tick's call.
    pub(crate) async fn refresh(&self) {
        self.catalogs.refresh_due(self.now()).await;
    }
}
