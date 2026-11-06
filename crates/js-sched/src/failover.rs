//! Failover of pass 2 (design 5.6, step 5): what the walk does when a provider refuses an
//! acquire, and when it asks again.
//!
//! [`AcquireMemory`] is pure state: it reads no clock, and every method that needs the time
//! takes `now`. It holds what pass 2 consults besides the catalogs:
//!
//! - per request, the attempt in progress: the offers (provider, offer id, zone) that
//!   answered `capacity` since the attempt began, the offer id whose other zones are asked
//!   next, and the time the next attempt may begin once every offer was refused or passed
//!   over. An attempt is not remembered past its end: capacity changes, so the next one
//!   asks again;
//! - the instance types passed over after a `quota` answer, each keyed by (provider,
//!   instance type, region) and kept until `now + quota_backoff`. An expired entry is
//!   dropped on read;
//! - which request each acquire was made for, so an answer is charged to its attempt.
//!
//! [`AcquireMemory::next`] is the walk: [`pick`] over the offers the memory has not ruled
//! out, so the order is the pick's (provider order, then price and the rest), with one
//! rule of its own: after a `capacity` answer the other zones of the same offer of the same
//! provider are asked before any other offer. When every offer the pick would take was
//! refused or passed over, the attempt ends and the request waits `acquire_retry`, or less
//! when a catalog refresh completes. An answer that is no answer (`unknown`, `deferred`)
//! records nothing here: the acquire may have landed, so nothing fails over, and it is
//! settled under its own acquire id. `shape` and `denied` answers are not failed over here
//! either; the caller handles them as before.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_elastic::{AcquireId, Acquired, Instant, Offer, OfferKey, RefusalCode};
use js_types::{ProviderId, RequestId};

use crate::acquire::{AcquireDecision, AcquireInput, ProviderView, Skip, Waiting, pick};

/// An instance type in a region of a provider, as a `quota` answer passes it over.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PassedOver {
    /// The provider.
    pub provider: ProviderId,
    /// The instance type: [`Offer::id`].
    pub instance_type: String,
    /// The region, as [`AcquireMemory::region`] reads it from the offer's zone. `None` for
    /// an offer with no zone: the provider's whole catalog is then one region.
    pub region: Option<String>,
}

/// What [`AcquireMemory::next`] decided for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Ask `provider` for one machine of `offer`.
    Acquire {
        /// The provider to ask.
        provider: ProviderId,
        /// The offer, as its catalog gave it.
        offer: Box<Offer>,
    },
    /// Acquire nothing now, for the pick's reason; no offer was ruled out by a refusal.
    Wait(Waiting),
    /// Every offer the pick would take was refused or passed over: the attempt ended, and
    /// the next one is not before `at` unless a catalog refresh completes first.
    Exhausted {
        /// When the next attempt may begin.
        at: Instant,
    },
    /// The request waits for its next attempt, which may begin at `at`.
    Retrying {
        /// When the next attempt may begin.
        at: Instant,
    },
}

/// A refused acquire as [`AcquireMemory::answered`] recorded it, for the caller's log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The provider that refused.
    pub provider: ProviderId,
    /// The offer id.
    pub offer: String,
    /// The zone.
    pub zone: Option<String>,
    /// The refusal's code.
    pub code: RefusalCode,
    /// For a `quota` answer: what is passed over, and until when.
    pub passed_over: Option<(PassedOver, Instant)>,
}

/// One request's attempt in progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Attempt {
    /// The offers that answered `capacity` in this attempt.
    declined: BTreeSet<OfferKey>,
    /// The provider and offer id whose other zones are asked next.
    following: Option<(ProviderId, String)>,
    /// When the next attempt may begin, after one that ended with nothing to ask.
    next_at: Option<Instant>,
}

/// What pass 2 remembers of refused acquires: see the module documentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcquireMemory {
    /// The region of a zone of a provider, where it is not the zone itself.
    regions: BTreeMap<(ProviderId, String), String>,
    /// The instance types passed over, each until its end.
    passed_over: BTreeMap<PassedOver, Instant>,
    /// The attempt of each request that has one.
    attempts: BTreeMap<RequestId, Attempt>,
    /// The request each acquire still unanswered was made for.
    launches: BTreeMap<AcquireId, RequestId>,
}

impl AcquireMemory {
    /// A memory that holds nothing, where each zone is its own region.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// This memory, where `zone` of `provider` lies in `region`. A zone not named so is its
    /// own region, which is how every provider built today names them: a zone is a region.
    #[must_use]
    pub fn with_region(mut self, provider: &ProviderId, zone: &str, region: &str) -> Self {
        self.regions
            .insert((provider.clone(), zone.to_owned()), region.to_owned());
        self
    }

    /// The region `zone` of `provider` lies in: the one [`AcquireMemory::with_region`]
    /// named, else the zone itself. `None` for no zone.
    #[must_use]
    pub fn region(&self, provider: &ProviderId, zone: Option<&str>) -> Option<String> {
        zone.map(|zone| {
            self.regions
                .get(&(provider.clone(), zone.to_owned()))
                .map_or_else(|| zone.to_owned(), Clone::clone)
        })
    }

    /// What a `quota` answer for `offer` of `provider` passes over.
    #[must_use]
    pub fn passed_over_key(&self, provider: &ProviderId, offer: &Offer) -> PassedOver {
        PassedOver {
            provider: provider.clone(),
            instance_type: offer.id.clone(),
            region: self.region(provider, offer.zone.as_deref()),
        }
    }

    /// Whether `offer` of `provider` is passed over at `now`: a `quota` answer for its
    /// instance type in its region ends after `now`.
    #[must_use]
    pub fn is_passed_over(&self, provider: &ProviderId, offer: &Offer, now: Instant) -> bool {
        self.passed_over
            .get(&self.passed_over_key(provider, offer))
            .is_some_and(|until| *until > now)
    }

    /// Forgets the pass-overs that have ended at `now`.
    pub fn forget_expired(&mut self, now: Instant) {
        self.passed_over.retain(|_, until| *until > now);
    }

    /// How many instance types are passed over, ended ones not yet forgotten included.
    #[must_use]
    pub fn passed_over_len(&self) -> usize {
        self.passed_over.len()
    }

    /// How many requests have an attempt or a wait for their next one.
    #[must_use]
    pub fn attempts_len(&self) -> usize {
        self.attempts.len()
    }

    /// Notes that `acquire` was made for `request`, so its answer is charged to the
    /// request's attempt.
    pub fn launched(&mut self, request: &RequestId, acquire: &AcquireId) {
        self.launches.insert(acquire.clone(), request.clone());
    }

    /// Takes in `provider`'s `answer` to `acquire`, made for `offer`, at `now`:
    ///
    /// - `capacity`: the offer in its zone is declined for the rest of the attempt of the
    ///   request it was made for, and that offer's other zones are asked next;
    /// - `quota`: its instance type in its region is passed over until `now +
    ///   quota_backoff`, for every request, whichever it was made for;
    /// - accepted: the request's attempt is over;
    /// - `shape`, `denied`: the acquire is forgotten and nothing else changes;
    /// - `unknown`, `deferred`: nothing changes; the acquire is settled under its own id.
    ///
    /// Returns what a refusal recorded, for the caller's log line.
    pub fn answered(
        &mut self,
        acquire: &AcquireId,
        (provider, offer): (&ProviderId, &Offer),
        answer: &Acquired,
        now: Instant,
        quota_backoff: Duration,
    ) -> Option<Refused> {
        let code = match answer {
            Acquired::Accepted { .. } => {
                if let Some(request) = self.launches.remove(acquire) {
                    self.attempts.remove(&request);
                }
                return None;
            }
            Acquired::Unknown | Acquired::Deferred { .. } => return None,
            Acquired::Refused(refusal) => refusal.code,
        };
        let request = self.launches.remove(acquire);
        let mut passed_over = None;
        match code {
            RefusalCode::Capacity => {
                if let Some(request) = request {
                    let attempt = self.attempts.entry(request).or_default();
                    attempt.declined.insert(OfferKey::of(provider, offer));
                    attempt.following = Some((provider.clone(), offer.id.clone()));
                }
            }
            RefusalCode::Quota => {
                let key = self.passed_over_key(provider, offer);
                let until = now.saturating_add(quota_backoff);
                let kept = self.passed_over.entry(key.clone()).or_insert(until);
                *kept = (*kept).max(until);
                passed_over = Some((key, until));
            }
            RefusalCode::Shape | RefusalCode::Denied => return None,
        }
        Some(Refused {
            provider: provider.clone(),
            offer: offer.id.clone(),
            zone: offer.zone.clone(),
            code,
            passed_over,
        })
    }

    /// Forgets the attempts of the requests `waiting` no longer holds, and the acquires
    /// `pending` no longer holds, so the memory is bounded by what waits and what is out.
    pub fn retain(
        &mut self,
        waiting: impl Fn(&RequestId) -> bool,
        pending: impl Fn(&AcquireId) -> bool,
    ) {
        self.attempts.retain(|request, _| waiting(request));
        self.launches.retain(|acquire, _| pending(acquire));
    }

    /// Pass 2 for `request` at `now`: what to acquire, or why nothing.
    ///
    /// A request waiting for its next attempt gets [`Next::Retrying`] until `at`, unless
    /// `refreshed` (a catalog of the pool's providers was fetched since the last time it
    /// was asked), which ends the wait at once. Otherwise the offers declined in the
    /// attempt and the instance types passed over are left out of each provider's
    /// catalog, and:
    ///
    /// 1. after a `capacity` answer, the other zones of the same offer of the same provider
    ///    come first, picked among themselves by the pick's order;
    /// 2. else [`pick`] decides over what is left;
    /// 3. when it acquires nothing but would have acquired with nothing left out, every
    ///    offer it would take was refused or passed over: the attempt ends, and the next
    ///    one may begin at `now + retry` ([`Next::Exhausted`]);
    /// 4. else it is the pick's [`Next::Wait`].
    ///
    /// Expired pass-overs are forgotten first. The skips are those of the last pick made.
    pub fn next(
        &mut self,
        request: &RequestId,
        input: &AcquireInput<'_>,
        now: Instant,
        (retry, refreshed): (Duration, bool),
    ) -> (Next, Vec<Skip>) {
        self.forget_expired(now);
        let attempt = self.attempts.entry(request.clone()).or_default();
        if let Some(at) = attempt.next_at {
            if (at > now) & !refreshed {
                return (Next::Retrying { at }, Vec::new());
            }
            attempt.next_at = None;
        }
        let attempt = attempt.clone();
        if let Some((provider, id)) = &attempt.following {
            let narrowed = self.narrowed(input, &attempt, now, Some((provider, id)));
            let views = views(input, &narrowed);
            if let (AcquireDecision::Acquire { provider, offer }, skips) = pick(&AcquireInput {
                providers: &views,
                ..*input
            }) {
                return (Next::Acquire { provider, offer }, skips);
            }
        }
        let narrowed = self.narrowed(input, &attempt, now, None);
        let views = views(input, &narrowed);
        let (decision, skips) = pick(&AcquireInput {
            providers: &views,
            ..*input
        });
        let waiting = match decision {
            AcquireDecision::Acquire { provider, offer } => {
                return (Next::Acquire { provider, offer }, skips);
            }
            AcquireDecision::Wait(waiting) => waiting,
        };
        let ruled_out = matches!(pick(input).0, AcquireDecision::Acquire { .. });
        if ruled_out {
            let at = now.saturating_add(retry);
            self.attempts.insert(
                request.clone(),
                Attempt {
                    next_at: Some(at),
                    ..Attempt::default()
                },
            );
            return (Next::Exhausted { at }, skips);
        }
        (Next::Wait(waiting), skips)
    }

    /// Each provider's offers less those declined in `attempt` and those passed over at
    /// `now`; with `only`, only the offers of that provider with that id, and no offer of
    /// any other provider.
    fn narrowed(
        &self,
        input: &AcquireInput<'_>,
        attempt: &Attempt,
        now: Instant,
        only: Option<(&ProviderId, &String)>,
    ) -> Vec<Vec<Offer>> {
        input
            .providers
            .iter()
            .map(|view| {
                view.offers
                    .iter()
                    .filter(|offer| {
                        let followed = only
                            .is_none_or(|(provider, id)| (&view.id, &offer.id) == (provider, id));
                        let declined = attempt.declined.contains(&OfferKey::of(&view.id, offer));
                        followed & !declined & !self.is_passed_over(&view.id, offer, now)
                    })
                    .cloned()
                    .collect()
            })
            .collect()
    }
}

/// The providers of `input`, each with its offers replaced by those of `narrowed` at its
/// position.
fn views<'a>(input: &AcquireInput<'a>, narrowed: &'a [Vec<Offer>]) -> Vec<ProviderView<'a>> {
    input
        .providers
        .iter()
        .zip(narrowed)
        .map(|(view, offers)| ProviderView {
            offers,
            ..view.clone()
        })
        .collect()
}
