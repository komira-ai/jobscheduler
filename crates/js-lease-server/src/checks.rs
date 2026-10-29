//! The live checks of an elastic pool: what only the network can say, asked in the
//! background and never at start.
//!
//! One task per pool waits on an injected [`Ticker`]. At each tick it asks whether a pass
//! is due by the injected clock; if it is, it runs one pass and records the verdict. A pass
//! asks the provider for its catalog (a refused key fails there) and checks what came
//! back, and asks the network mode whether it can issue what a launch needs. The two
//! halves run on the runtime's blocking pool, each with its own panic caught, and each call
//! is bounded by the time limit of the client that makes it. A pass never exits the process
//! and never holds up a tick of the lease table: the loop is a task of its own.
//!
//! A pool begins `Checking` and stays so until one pass has been recorded. It is
//! `Available` when every check of the last pass held, and `Unavailable` otherwise, with
//! the reasons shown. A healthy pool is checked every 60 s; an unavailable one after 10 s,
//! then after twice as long each time it fails again, up to 5 min.
//!
//! No reason holds a secret: they are the provider's and the network mode's own messages,
//! which are written to be shown (a message never quotes the body of an answer), and offer
//! ids and numbers.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_elastic::{ElasticProvider, Offer};
use js_lease::{Clock, Timestamp};
use serde::Serialize;
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::adapt::SharedClock;
use crate::kinds::Floor;
use crate::network::{NetworkError, NetworkMode};
use crate::ticker::Ticker;

/// How long a pool that passed waits for its next pass.
pub(crate) const HEALTHY_EVERY: Duration = Duration::from_secs(60);

/// How long a pool waits after its first failed pass; each further failure doubles it.
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(10);

/// The longest a pool that keeps failing waits.
pub(crate) const RETRY_MAX: Duration = Duration::from_secs(300);

/// Where a pool is: whether it may launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    /// No pass has been recorded yet. Nothing launches.
    Checking,
    /// Every check of the last pass held.
    Available,
    /// A check of the last pass failed. Nothing launches.
    Unavailable,
}

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// Why it failed; `None` when it held.
    pub(crate) reason: Option<String>,
    /// When it was made, by the injected clock.
    pub(crate) at: Timestamp,
}

/// What the checks have found so far, as the status routes read it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Health {
    passes: u64,
    /// The provider's answer to its catalog call: the credentials.
    pub(crate) credentials: Option<Verdict>,
    /// The network mode's check.
    pub(crate) network: Option<Verdict>,
    /// What is wrong with the catalog, and nothing else that has no verdict of its own.
    pub(crate) notes: Vec<String>,
    /// The offers of the last catalog that was had.
    pub(crate) offers: Vec<Offer>,
}

impl Health {
    /// How the pool stands.
    pub(crate) fn state(&self) -> State {
        match (self.passes, self.reasons().is_empty()) {
            (0, _) => State::Checking,
            (_, true) => State::Available,
            (_, false) => State::Unavailable,
        }
    }

    /// How the scaler reads the pool: it launches only while the checks pass.
    pub(crate) fn for_scaler(&self) -> js_scaler::Health {
        match self.state() {
            State::Checking => js_scaler::Health::Checking,
            State::Available => js_scaler::Health::Available,
            State::Unavailable => js_scaler::Health::Failing,
        }
    }

    /// Every reason a check of the last pass failed, in the order the checks run, each
    /// once.
    pub(crate) fn reasons(&self) -> Vec<String> {
        let failed = |verdict: &Option<Verdict>| verdict.as_ref().and_then(|v| v.reason.clone());
        let all = [failed(&self.credentials)]
            .into_iter()
            .chain(self.notes.iter().cloned().map(Some))
            .chain([failed(&self.network)]);
        all.flatten().collect()
    }

    /// Takes in a pass made at `at`.
    fn record(&mut self, pass: Pass, at: Timestamp) {
        self.passes += 1;
        let verdict = |result: Result<(), String>| Verdict {
            reason: result.err(),
            at,
        };
        let Provided {
            credentials,
            notes,
            offers,
        } = pass.provider;
        self.credentials = Some(verdict(credentials));
        self.network = Some(verdict(pass.network));
        self.notes = notes;
        // A catalog that could not be had keeps the last one that could, so a status
        // page still has offers to plan with while the credentials are being fixed.
        if !offers.is_empty() {
            self.offers = offers;
        }
    }
}

/// What the provider half of a pass found.
struct Provided {
    credentials: Result<(), String>,
    notes: Vec<String>,
    offers: Vec<Offer>,
}

/// What one pass found.
struct Pass {
    provider: Provided,
    network: Result<(), String>,
}

/// The provider half: the catalog call is the credential check, and the catalog it
/// returns is checked against the pool's floor.
fn provider_checks(
    provider: &dyn ElasticProvider,
    floor: &Floor,
    pins: Option<&AgentPins>,
) -> Provided {
    match provider.catalog() {
        Err(error) => Provided {
            credentials: Err(format!("credentials: failed ({error})")),
            notes: Vec::new(),
            offers: Vec::new(),
        },
        Ok(offers) => Provided {
            credentials: Ok(()),
            notes: catalog_notes(&offers, floor, pins),
            offers,
        },
    }
}

/// What is wrong with a catalog: no offer at all, an offer with no price, an offer with
/// fewer GPUs than the pool's sizes give at least, an offer with GPUs of another class.
/// Each kind is said once, for the first offer that has it.
fn catalog_notes(offers: &[Offer], floor: &Floor, pins: Option<&AgentPins>) -> Vec<String> {
    let mut notes = Vec::new();
    if offers.is_empty() {
        notes.push("catalog: no offers".to_owned());
    }
    if let Some(offer) = offers.iter().find(|o| o.price_cents_per_hour.is_none()) {
        notes.push(format!("catalog: {} has no price", offer.id));
    }
    if let Some(offer) = offers.iter().find(|o| u64::from(o.gpus) < floor.gpus) {
        notes.push(format!(
            "catalog: {} has {} gpus, below the pool floor of {}",
            offer.id, offer.gpus, floor.gpus
        ));
    }
    let other_class = |o: &&Offer| o.gpus > 0 && o.gpu_class != floor.gpu_class;
    if let Some(offer) = offers.iter().filter(|_| floor.gpus > 0).find(other_class) {
        notes.push(format!(
            "catalog: {} has gpus of another class than the pool's",
            offer.id
        ));
    }
    if let Some(offer) = pins.and_then(|pins| pins.missing(offers)) {
        notes.push(format!(
            "catalog: {} is for the architecture {}, which has no agent pin (agent.{})",
            offer.id, offer.arch, offer.arch
        ));
    }
    notes
}

/// The architectures a pool that launches has an agent pinned for. An offer of another
/// is never launched, so the pool says so while it is unavailable.
#[derive(Debug, Clone)]
pub(crate) struct AgentPins(pub(crate) BTreeSet<String>);

impl AgentPins {
    /// The first of `offers` of an architecture with no pin.
    fn missing<'a>(&self, offers: &'a [Offer]) -> Option<&'a Offer> {
        offers.iter().find(|offer| !self.0.contains(&offer.arch))
    }
}

/// The network half: whether the mode can issue what a launch needs. A mode that could
/// not be built answers with why.
fn network_check(mode: &Result<Arc<dyn NetworkMode>, NetworkError>) -> Result<(), String> {
    match mode {
        Ok(mode) => mode.check().map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// Runs both halves of a pass at once on the blocking pool. A panic in a half fails that
/// half and nothing else.
async fn pass(
    provider: &Arc<dyn ElasticProvider>,
    floor: &Floor,
    mode: &Result<Arc<dyn NetworkMode>, NetworkError>,
    pins: &Option<AgentPins>,
) -> Pass {
    let (provider, floor, pins) = (provider.clone(), floor.clone(), pins.clone());
    let provided =
        spawn_blocking(move || provider_checks(provider.as_ref(), &floor, pins.as_ref()));
    let mode = mode.clone();
    let networked = spawn_blocking(move || network_check(&mode));
    let (provided, networked) = tokio::join!(provided, networked);
    Pass {
        provider: provided.unwrap_or_else(|_| Provided {
            credentials: Err("credentials: the check panicked".to_owned()),
            notes: Vec::new(),
            offers: Vec::new(),
        }),
        network: networked.unwrap_or_else(|_| Err("network: the check panicked".to_owned())),
    }
}

/// When the next pass is due.
#[derive(Debug, Clone, Copy)]
struct Schedule {
    due: Timestamp,
    failures: u32,
}

impl Schedule {
    /// A pass is due at `now`.
    fn first(now: Timestamp) -> Self {
        Self {
            due: now,
            failures: 0,
        }
    }

    fn is_due(&self, now: Timestamp) -> bool {
        now >= self.due
    }

    /// Schedules the pass after one made at `now` that held or did not.
    fn after(&mut self, now: Timestamp, held: bool) {
        self.failures = match held {
            true => 0,
            false => self.failures.saturating_add(1),
        };
        self.due = now.plus(Self::delay(self.failures));
    }

    /// The wait after `failures` failed passes in a row (none: the healthy period).
    fn delay(failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(5);
        let retry = RETRY_FIRST.saturating_mul(1 << doublings).min(RETRY_MAX);
        match failures {
            0 => HEALTHY_EVERY,
            _ => retry,
        }
    }
}

/// What the check loop of one pool holds.
pub(crate) struct Checker {
    pub(crate) pool: String,
    pub(crate) provider: Arc<dyn ElasticProvider>,
    pub(crate) mode: Result<Arc<dyn NetworkMode>, NetworkError>,
    pub(crate) floor: Floor,
    /// The agents pinned, when the pool launches.
    pub(crate) pins: Option<AgentPins>,
    pub(crate) clock: SharedClock,
    pub(crate) health: Arc<Mutex<Health>>,
}

impl Checker {
    /// Waits on `ticker` and runs a pass at each tick that finds one due, until the task
    /// is aborted. The tick is finished when the pass is.
    pub(crate) async fn run<T: Ticker>(self, mut ticker: T) {
        let mut schedule = Schedule::first(self.clock.now());
        loop {
            let tick = ticker.next().await;
            let now = self.clock.now();
            if schedule.is_due(now) {
                let held = self.once().await;
                schedule.after(now, held);
            }
            tick.finish();
        }
    }

    /// One pass. Whether every check held.
    async fn once(&self) -> bool {
        let found = pass(&self.provider, &self.floor, &self.mode, &self.pins).await;
        let mut health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
        let before = health.state();
        health.record(found, self.clock.now());
        let after = health.state();
        let reasons = health.reasons();
        drop(health);
        if before != after {
            match after {
                State::Available => info!(pool = %self.pool, "the pool passed its checks"),
                _ => warn!(pool = %self.pool, ?reasons, "the pool fails its checks"),
            }
        }
        reasons.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the schedule: a healthy pool waits 60 s, a failing one 10 s and then twice
    /// as long each time up to 5 min, and one pass that holds starts it over; and a pass
    /// is due exactly at its time. It catches a backoff that overflows, never stops
    /// growing, or is not reset by a pass that holds.
    #[test]
    fn a_failing_pool_backs_off_to_five_minutes_and_a_pass_that_holds_resets_it() {
        let secs = |s: u64| Duration::from_secs(s);
        let start = Timestamp::from_millis(1_000);
        let mut schedule = Schedule::first(start);
        assert!(schedule.is_due(start));
        schedule.after(start, true);
        assert!(!schedule.is_due(start.plus(secs(59))));
        assert!(schedule.is_due(start.plus(secs(60))));
        let waits: Vec<Duration> = (0..9)
            .map(|_| {
                schedule.after(start, false);
                Duration::from_millis(schedule.due.millis() - start.millis())
            })
            .collect();
        let expected = [10, 20, 40, 80, 160, 300, 300, 300, 300].map(secs);
        assert_eq!(waits, expected);
        schedule.after(start, true);
        assert_eq!(schedule.failures, 0);
        schedule.failures = u32::MAX;
        schedule.after(start, false);
        assert_eq!(schedule.failures, u32::MAX);
        assert!(schedule.is_due(start.plus(secs(300))));
    }

    /// Asserts how the scaler reads a pool: before any pass it is still being checked, a
    /// pass with no failed check makes it available, and one with a failed check makes it
    /// failing. It catches an unavailable pool read as available, which would launch
    /// machines through a refused key.
    #[test]
    fn the_scaler_reads_the_state_of_the_checks() {
        let mut health = Health::default();
        assert_eq!(health.for_scaler(), js_scaler::Health::Checking);
        health.passes = 1;
        assert_eq!(health.for_scaler(), js_scaler::Health::Available);
        health.notes = vec!["catalog: no offers".to_owned()];
        assert_eq!(health.for_scaler(), js_scaler::Health::Failing);
    }
}
