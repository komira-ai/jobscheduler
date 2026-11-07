//! What a scheduler remembers of a provider's catalog, and which offers it has cooled
//! down: plain state that time is handed to.
//!
//! [`CatalogCache`] holds the last catalog a provider returned whole, with when it came.
//! Admission (design 5.6, step 2) and the pick of an offer (pass 2) read it; a refresh,
//! asked for once per tick of the caller, fills it when [`CatalogCache::refresh_due`]
//! says so: before the first catalog, after an invalidation, and once the refresh interval
//! has passed since the last catalog that came. A failed refresh keeps the last good
//! catalog and leaves the refresh due, so it is asked again at the next tick and not after
//! another interval. A catalog never fetched reads as [`Catalog::Never`], which is not an
//! empty catalog: a catalog that was read and is empty is [`Catalog::Offers`] with no
//! offers.
//!
//! [`Cooldowns`] holds the offers that are cooling down, by [`OfferKey`] (provider,
//! instance type, zone), each until its end.
//!
//! Nothing here reads a clock, waits or calls a provider: the caller passes the time as an
//! [`Instant`] and the result of the provider's call to [`CatalogCache::record`].

use std::collections::BTreeMap;
use std::time::Duration;

use js_types::ProviderId;

use crate::offer::Offer;
use crate::provider::ProviderError;

/// A point in time, in milliseconds since an origin the caller chooses. Only differences
/// and order mean anything, so the caller's clock (whatever its origin) converts to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64);

impl Instant {
    /// The instant `millis` milliseconds after the origin.
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// The milliseconds since the origin.
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// The instant `after` later, or the last instant there is when that is past it.
    #[must_use]
    pub fn saturating_add(self, after: Duration) -> Self {
        let after = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(after))
    }

    /// How long after `earlier` this instant is; nothing when it is not after it.
    fn since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

/// What a read of the cache gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog<'a> {
    /// No catalog has come yet: none was asked for, or every one asked for failed. It is
    /// not an empty catalog; nothing is known.
    Never,
    /// The last catalog that came whole.
    Offers {
        /// Its offers, in the provider's order; empty when the provider offered nothing.
        offers: &'a [Offer],
        /// How long ago it came.
        age: Duration,
        /// Whether a refresh is due for it: it was invalidated, or it is at least one
        /// refresh interval old. A stale catalog is still the best one known.
        stale: bool,
    },
}

/// What [`CatalogCache::record`] made of a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The catalog came and replaced the one before.
    Fetched,
    /// The fetch failed and the last good catalog, if any, stays.
    Failed {
        /// Whether this failure starts an outage: the record before it was a success, or
        /// there was none. A caller warns on the first failure and not on each one after.
        first: bool,
    },
}

/// The cached catalog of one provider.
#[derive(Debug, Clone)]
pub struct CatalogCache {
    refresh: Duration,
    /// The last catalog that came, and when.
    good: Option<(Vec<Offer>, Instant)>,
    /// Whether [`CatalogCache::invalidate`] was called since the last catalog came.
    invalidated: bool,
    /// The error of the last fetch, while it failed.
    error: Option<String>,
}

impl CatalogCache {
    /// An empty cache, whose catalog is fetched again `refresh` after each one that came.
    pub fn new(refresh: Duration) -> Self {
        Self {
            refresh,
            good: None,
            invalidated: false,
            error: None,
        }
    }

    /// The catalog as it stands at `now`.
    pub fn read(&self, now: Instant) -> Catalog<'_> {
        match &self.good {
            None => Catalog::Never,
            Some((offers, at)) => Catalog::Offers {
                offers,
                age: now.since(*at),
                stale: self.refresh_due(now),
            },
        }
    }

    /// Whether the catalog should be fetched at `now`: no catalog has come yet, it was
    /// invalidated, or `now` is at least the refresh interval after the last one that came.
    /// A failed fetch moves nothing, so after one the fetch stays due.
    pub fn refresh_due(&self, now: Instant) -> bool {
        match &self.good {
            None => true,
            Some((_, at)) => self.invalidated | (now >= at.saturating_add(self.refresh)),
        }
    }

    /// Takes in the result of a fetch made at `now`. A catalog replaces the one before and
    /// clears the invalidation and the error; an error keeps the last good catalog as it
    /// was and is kept as text. An invalidation made while the fetch was in flight is
    /// cleared too, so a caller does not invalidate while a fetch is in flight.
    pub fn record(&mut self, now: Instant, fetched: Result<Vec<Offer>, ProviderError>) -> Recorded {
        match fetched {
            Ok(offers) => {
                self.good = Some((offers, now));
                self.invalidated = false;
                self.error = None;
                Recorded::Fetched
            }
            Err(error) => {
                let first = self.error.is_none();
                self.error = Some(error.to_string());
                Recorded::Failed { first }
            }
        }
    }

    /// Marks the catalog out of date: the next [`CatalogCache::refresh_due`] is true. The
    /// catalog stays readable until a fetch replaces it: a refusal to launch says the
    /// offers are out of date, not that the provider could never offer them.
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    /// The error of the last fetch, while the fetches fail; `None` once one succeeds.
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// An offer as a cool-down knows it: the provider, the instance type and the zone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OfferKey {
    /// The provider of the offer.
    pub provider: ProviderId,
    /// The provider's instance type: [`Offer::id`].
    pub offer: String,
    /// The zone of the offer: [`Offer::zone`].
    pub zone: Option<String>,
}

impl OfferKey {
    /// The key of `offer` of `provider`.
    pub fn of(provider: &ProviderId, offer: &Offer) -> Self {
        Self {
            provider: provider.clone(),
            offer: offer.id.clone(),
            zone: offer.zone.clone(),
        }
    }
}

/// The offers that are cooling down, each until its end.
#[derive(Debug, Clone, Default)]
pub struct Cooldowns {
    ends: BTreeMap<OfferKey, Instant>,
}

impl Cooldowns {
    /// No offer cooling down.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cools `key` down from `now` for `length`. A key already cooling down keeps the
    /// later of its end and the new one: a cool-down is never shortened.
    pub fn cool_down(&mut self, key: OfferKey, now: Instant, length: Duration) {
        let end = now.saturating_add(length);
        let kept = self.ends.entry(key).or_insert(end);
        *kept = (*kept).max(end);
    }

    /// Cools `key` down until `end`, an end decided and written down earlier (a cool-down
    /// read back after a restart). A key already cooling down keeps the later of its end and
    /// `end`, as [`Cooldowns::cool_down`] does.
    pub fn hold_until(&mut self, key: OfferKey, end: Instant) {
        let kept = self.ends.entry(key).or_insert(end);
        *kept = (*kept).max(end);
    }

    /// Takes in every cool-down of `other`, each to its end ([`Cooldowns::hold_until`]): a key
    /// held by both keeps the later end.
    pub fn merge(&mut self, other: &Self) {
        for (key, end) in &other.ends {
            self.hold_until(key.clone(), *end);
        }
    }

    /// Whether `key` is cooling down at `now`: `now` is before its end. At the end instant
    /// it no longer is.
    pub fn is_cooled_down(&self, key: &OfferKey, now: Instant) -> bool {
        self.ends.get(key).is_some_and(|end| now < *end)
    }

    /// Drops the cool-downs that have ended at `now`, and keeps the others.
    pub fn forget_expired(&mut self, now: Instant) {
        self.ends.retain(|_, end| now < *end);
    }

    /// How many cool-downs are held, ended ones not yet forgotten included.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether no cool-down is held.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
