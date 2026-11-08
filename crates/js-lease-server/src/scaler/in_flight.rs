//! The launches whose provider call is out: each acquire the shell has handed to the
//! provider and whose answer has not come back yet.
//!
//! A tick works on a copy of the shell's memory and swaps it in only when the tick is done,
//! so the record of a launch being made is not readable until its answer is in. This is
//! what a reader (the operator's listing of the providers) sees of the call meanwhile. An
//! entry is put in just before the provider is called, with the time of the server's clock
//! then, and taken out as soon as the call returns or unwinds ([`Landing`]), so it holds only
//! calls in progress. Nothing here is written to disk: after a restart no call is out.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use js_scaler::{AcquirePlan, Millis};

/// One launch whose provider call is out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// The pool the machine is for.
    pub pool: String,
    /// The name the machine is given, which it registers under.
    pub host: String,
    /// The offer asked for: the provider's instance type.
    pub offer: String,
    /// The offer's zone, when it names one.
    pub zone: Option<String>,
    /// When the provider was called, by the server's clock.
    pub since: Millis,
}

impl InFlight {
    /// The launch `plan` describes, called at `since`.
    fn of(plan: &AcquirePlan, since: Millis) -> Self {
        Self {
            pool: plan.pool.clone(),
            host: plan.name.clone(),
            offer: plan.offer.id.clone(),
            zone: plan.offer.zone.clone(),
            since,
        }
    }
}

/// The launches out now, by acquire id. The lock is taken only to put an entry in, take it
/// out or copy the entries, never across the provider's call.
#[derive(Debug, Default)]
pub(super) struct Out(Mutex<BTreeMap<String, InFlight>>);

impl Out {
    fn entries(&self) -> MutexGuard<'_, BTreeMap<String, InFlight>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that the provider is called now, at `since`, for `plan`; the entry lasts as
    /// long as the [`Landing`] returned.
    pub(super) fn begin(&self, plan: &AcquirePlan, since: Millis) -> Landing<'_> {
        let key = plan.acquire.as_str().to_owned();
        self.entries()
            .insert(key.clone(), InFlight::of(plan, since));
        Landing { out: self, key }
    }

    /// The launches out now, by acquire id.
    pub(super) fn now(&self) -> Vec<InFlight> {
        self.entries().values().cloned().collect()
    }
}

/// A launch out: its entry is taken out when this is dropped, as the provider's call returns
/// or unwinds.
pub(super) struct Landing<'a> {
    out: &'a Out,
    key: String,
}

impl Drop for Landing<'_> {
    fn drop(&mut self) {
        self.out.entries().remove(&self.key);
    }
}
