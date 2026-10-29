//! The scaler's step: the pure rule that matches waiting demand to elastic pools by their
//! offers, launches machines within the caps, and drains and releases the idle ones.
//!
//! This is a pure crate: no clock, no sleep, no lock, no hashed collection, no socket.
//! [`step`] takes the previous [`State`], one [`Input`] snapshot and the time now, and
//! returns the next [`State`] and the [`Action`]s to run. The same state and input give
//! the same answer. The caller (the server's tick, in a later change) snapshots the
//! registry and the providers, calls [`step`], runs the actions in order with no lock held,
//! and feeds the providers' answers into the next input. The first action of a step that
//! changed the records is [`Action::Save`]: all of [`Records`], written as one file before
//! the next action runs. Nothing the scaler needs after a restart is kept anywhere else.
//!
//! # What a step decides
//!
//! * Demand. A waiting request is covered, and gets no machine, when it holds a
//!   reservation, when a free host that fits is not set aside for another request, or
//!   when a machine on its way (planned, launching, unresolved or booting) has an offer
//!   that fits it. Matching is greedy in queue order, one request to one host or machine,
//!   across all the pools before any pool launches. A request that fits a draining host
//!   takes it back ([`Action::Undrain`]) instead of launching.
//!   [`Offer::fit`](js_elastic::Offer::fit) judges every fit, so a request never lands on
//!   an offer of another operating system or architecture.
//! * Launch. Up to `max_step` launches per step, to the cheapest fitting offer that has a
//!   price above zero (an offer with no price, or a price of zero, is never launched: zero
//!   would pass both spend caps; a request that fits only such an offer shows
//!   [`Blocked::Unpriced`]), when the pool is in `Live` mode, its checks pass, its list of machines was read, it is not
//!   cooling off, no launch of it is waiting for the time its provider named, and the caps
//!   hold: `max_instances`, `max_spend_per_hour` over the stored prices of the machines
//!   held, and `max_spend_per_day` over what the machines cost in the last 24 hours (a
//!   machine that still runs keeps accruing, a launch that waits counts at its least cost).
//!   The acquire id is the pool and 128 random bits the caller gives in [`Input::entropy`].
//!   A launch the provider told to wait is retried when its time comes, if the pool is
//!   `Live` and every cap still holds.
//! * Answers. An acquire's answer is matched to its record by acquire id. `Unknown`
//!   leaves the record unresolved until a list shows the machine or `unknown_hold` has
//!   passed; a machine found after that is released and never uncounted.
//! * Machines the registry does not hear from. A host the registry shows but does not hear
//!   from is held and counted, and [`State::silent`] says since when. Once it books nothing
//!   ([`HostFacts::lease_given_up`], or the registry marked it `Releasing`) and has stayed
//!   silent for `unknown_hold`, the machine is released, since it costs money and holds
//!   nothing: drained and marked for release, which the registry checks when the actions
//!   run, then released when the mark is read back; [`State::silent_released`] names it for
//!   the shell to log. A host that shows a lease, a run, a stream or a `Held` or `Reserved`
//!   mark is never released, except that a silent host whose leases the registry gave up
//!   ([`HostFacts::lease_given_up`]) is not kept by the runs and streams it lists, which a
//!   job that can no longer report leaves behind. Hosts the scaler did not launch are
//!   never released. One the registry no longer shows is released too, but only on a
//!   complete registry view ([`Input::registry_complete`], the shell's duty to vouch for)
//!   with the machine missing on complete views `unknown_hold` apart; otherwise it is held
//!   and counted. A booting machine is given up on at `boot_timeout` on a complete view
//!   only, and only once its host has been missing on complete views for a third of
//!   `boot_timeout`, so a restart does not release a machine whose agent is about to say
//!   hello. The times a silent host was first seen silent and a machine was first seen
//!   missing are in [`Records`] with the time of the step that wrote them, so a scaler
//!   restarted more often than `unknown_hold` still reaches the hold by the time it
//!   observed; the time it was down is not counted. One the provider says it no longer has
//!   ([`Input::not_found`]) is settled.
//! * Scale down. A registered host quiet for `idle` is drained; a drained host that is
//!   still quiet is marked for release; once the registry shows the mark the token is
//!   revoked and the machine released, every step, until a read by id says gone. There is
//!   no deadline on any of this, and nothing is released that the registry did not
//!   mark while quiet, except a machine the registry has forgotten, which holds no work.
//!
//! * Machines the scaler does not release. An orphan (a listed machine no record knows, or
//!   one a launch answer named and could not confirm released) is counted and reported and
//!   never released: the design leaves that to the operator. The shell shows
//!   [`State::records`] to the operator and alerts on every `Orphan` record. The same holds
//!   for a record whose pool is no longer among [`Input::pools`]: it stays in the records and
//!   counts, but no pass reconciles or releases it, so the shell alerts on such a record
//!   and the operator releases the machine at the provider and removes the record. A
//!   launch the provider told to wait that no request waits for any more is forgotten.
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
use crate::scale::Claims;

pub use input::{
    AcquireResult, HostFacts, HostState, Input, Mark, PoolInput, ReleaseResult, Waiting,
};
pub use record::{
    Cooldown, CooldownReason, OLDEST, Phase, Record, Records, RecordsError, StoredExec,
    StoredOffer, VERSION,
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
    let before = state.records.clone();
    let mut pass = Pass::new(state, input, now);
    for pool in &input.pools {
        pass.reconcile(pool);
    }
    // Every request is matched to what covers it, in every pool, before any pool launches:
    // a request a later pool's host or machine on its way covers gets nothing from an
    // earlier one.
    let mut claims = Claims::default();
    let mut waiting: Vec<&Waiting> = input
        .waiting
        .iter()
        .filter(|request| request.reserved.is_none())
        .collect();
    for pool in &input.pools {
        waiting = pass.cover(pool, waiting, &mut claims);
        waiting = pass.undrain(pool, waiting, &mut claims);
    }
    pass.drop_unwanted(&claims);
    let uncovered = waiting.clone();
    for pool in &input.pools {
        pass.settle(pool, &claims);
        waiting = pass.launch(pool, waiting);
    }
    pass.finish(&before, &uncovered)
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
