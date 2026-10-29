//! The scaler's step: the pure rule that matches waiting demand to elastic pools by their
//! offers, launches machines within the caps, and drains and releases the idle ones.
//!
//! This is a pure crate: no clock, no sleep, no lock, no hashed collection, no socket.
//! [`step`] takes the previous [`State`], one [`Input`] snapshot and the time now, and
//! returns the next [`State`] and the [`Action`]s to run. The same state and input give
//! the same answer. The caller (the server's tick, in a later change) snapshots the
//! registry and the providers, calls [`step`], writes [`State::records`] and runs the
//! actions with no lock held, and feeds the providers' answers into the next input.
//!
//! # What a step decides
//!
//! * Demand. A waiting request is covered, and gets no machine, when it holds a
//!   reservation, when a free host that fits is not set aside for another request, or
//!   when a machine on its way (planned, launching, unresolved or booting) has an offer
//!   that fits it. Matching is greedy in queue order, one request to one host or machine.
//!   A request that fits a draining host takes it back ([`Action::Undrain`]) instead of
//!   launching. [`Offer::fit`](js_elastic::Offer::fit) judges every fit, so a request
//!   never lands on an offer of another operating system or architecture.
//! * Launch. Up to `max_step` launches per step, to the cheapest fitting offer, when the
//!   pool is in `Live` mode, its checks pass, its list of machines was read, it is not
//!   cooling off and the caps hold: `max_instances`, `max_spend_per_hour` over the stored
//!   prices of the machines held, and `max_spend_per_day` over the billed time of the
//!   machines launched in the last 24 hours. The record is an [`Action::Record`] ahead of
//!   the [`Action::Acquire`] it describes.
//! * Answers. An acquire's answer is matched to its record by acquire id. `Unknown`
//!   leaves the record unresolved until a list shows the machine or `unknown_hold` has
//!   passed; a machine found after that is released and never uncounted.
//! * Scale down. A registered host quiet for `idle` is drained; a drained host that is
//!   still quiet is marked for release; once the registry shows the mark the token is
//!   revoked and the machine released, every step, until a read by id says gone. There is
//!   no deadline on any of this, and nothing is released that the registry did not
//!   mark while quiet.
//!
//! The state machine is [`Phase`]. A transition it does not list is refused, counted in
//! [`State::refused`], and changes nothing.
//!
//! The records are [`Records`]; [`Records::encode`] and [`Records::decode`] are their
//! versioned file format.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod caps;
mod input;
mod pass;
mod record;
mod scale;
mod settings;
mod state;
mod time;

use std::collections::BTreeSet;

use js_elastic::Demand;

use crate::caps::{check, choose};
use crate::pass::{Pass, host_named};

pub use input::{
    AcquireResult, HostFacts, HostState, Input, Mark, PoolInput, ReleaseResult, Waiting,
};
pub use record::{
    Cooldown, CooldownReason, Phase, Record, Records, RecordsError, StoredExec, StoredOffer,
    VERSION,
};
pub use settings::{Blocked, Health, LaunchMode, Settings};
pub use state::{AcquirePlan, Action, PlanLine, State};
pub use time::Millis;

/// One step of the scaler at `now`: applies the provider's answers, the lists and the
/// hosts' hellos to `state`, then decides what to drain, release and launch.
///
/// Deterministic: it reads no clock and does no I/O.
#[must_use]
pub fn step(state: State, input: &Input, now: Millis) -> (State, Vec<Action>) {
    let before = state.records.instances.clone();
    let mut pass = Pass::new(state, input, now);
    for pool in &input.pools {
        pass.reconcile(pool);
    }
    let mut waiting: Vec<&Waiting> = input
        .waiting
        .iter()
        .filter(|request| request.reserved.is_none())
        .collect();
    for pool in &input.pools {
        waiting = pass.scale(pool, waiting);
    }
    pass.finish(&before)
}

/// Whether a request for `demand` could be served by launching a machine now: some pool
/// is live and available, an offer of it fits, and the caps allow one more machine once
/// the idle hosts that would be released count as free. This is the registry's
/// `Launchable`; a request it refuses fails at once instead of waiting on a pool that
/// cannot launch. A pool cooling off after a failure still answers yes: the request waits.
#[must_use]
pub fn launchable(state: &State, input: &Input, now: Millis, demand: &Demand) -> bool {
    input.pools.iter().any(|pool| {
        let settings = &pool.settings;
        let idle: BTreeSet<String> = state
            .records
            .instances
            .iter()
            .filter(|record| {
                let quiet = host_named(input, &record.name).is_some_and(|host| {
                    [
                        host.state == HostState::Live,
                        !host.busy,
                        [None, Some(Mark::Draining)].contains(&host.mark),
                    ]
                    .into_iter()
                    .all(|ok| ok)
                });
                [
                    record.pool == settings.pool,
                    [Phase::Registered, Phase::Draining].contains(&record.phase),
                    quiet,
                ]
                .into_iter()
                .all(|ok| ok)
            })
            .map(|record| record.name.clone())
            .collect();
        let price = choose(settings, &pool.offers, demand, &[])
            .and_then(|offer| offer.price_cents_per_hour);
        let within = price.is_some_and(|price| {
            check(settings, now, &state.records.instances, &[], &idle, price).is_ok()
        });
        [
            pool.health == Health::Available,
            settings.launch == LaunchMode::Live,
            within,
        ]
        .into_iter()
        .all(|ok| ok)
    })
}
