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
//!   reservation ([`Waiting::reserved`]: the host or the machine on its way it is set aside
//!   for is then matched to no other request, and the host is not drained as idle), when a
//!   free host that fits is not set aside for another request, or
//!   when a machine on its way (planned, launching, unresolved or booting) has an offer
//!   that fits it. Matching is greedy in queue order, one request to one host or machine,
//!   across all the pools before any pool launches. A request that fits a draining host
//!   takes it back ([`Action::Undrain`]) instead of launching.
//!   [`Offer::fit`](js_elastic::Offer::fit) judges every fit, so a request never lands on
//!   an offer of another operating system or architecture.
//! * Launch. Up to `max_step` launches per step, each to the offer pass 2 of placement picks
//!   for the request (`js_sched::acquire::pick`, design 5.6, step 5; the scaler does not
//!   choose): with the pool as its one machine provider, the cheapest available fitting
//!   offer inside the pool's `provider_limits` (`allowed_instance_types`,
//!   `max_price_cents_per_hour`), then the fewest accelerators, the least spare CPU and
//!   memory, and the offer id and zone. A provider's refusal is failed over within the
//!   request's attempt ([`js_sched::failover`]): an offer refused for `capacity` is not
//!   asked again in the attempt and its other zones are asked next, an instance type refused
//!   for `quota` is passed over in its region for [`Settings::quota_backoff`] for every
//!   request, and a request every offer was refused for waits [`Settings::acquire_retry`],
//!   or until the pool's catalog is fetched again ([`PoolInput::catalog_refreshed`]). Neither
//!   answer cools the pool or counts toward its brake. An offer with no price, or a price of zero, is never launched (zero would
//!   pass both spend caps; a request that fits only such an offer shows
//!   [`Blocked::Unpriced`]), and a provider that holds its share ([`Settings::provider_share`])
//!   launches nothing and shows [`Blocked::MaxInstances`]. A launch is made when the pool
//!   is in `Live` mode, its checks pass, its list of machines was read, it is not
//!   cooling off, no launch of it is waiting for the time its provider named, and the caps
//!   hold: `max_instances`, `max_spend_per_hour` over the stored prices of the machines
//!   held, and `max_spend_per_day` over what the machines cost in the last 24 hours (a
//!   machine that still runs keeps accruing, a launch that waits counts at its least cost).
//!   Server-wide totals ([`Budgets`]) are held beside the pool's caps: over the records of
//!   every pool of the server, and of every pool of the provider kind, the cost in the last 24
//!   hours, in the last 30 days (rolling) and since the pool's go ([`Go`]: its first launch since it
//!   was last in `Plan`, written to the records with that launch and lost when the pool is seen
//!   in `Plan`) may not
//!   pass a total with the new machine's least cost added ([`Blocked::MaxSpendTotal`]). A pool
//!   whose `live_until` has come is in `Plan` ([`Settings::mode`]). A total only keeps a launch
//!   from being made; the overrun it allows is bounded by [`overrun`]. Records are kept until
//!   their billed time has left the 30 days.
//!   The failure brake ([`Brake`], in `brake`) keeps a pool from looping on failures: each
//!   refusal (denied, shape), boot given up on and spot preemption counts, the cooldown
//!   after the n-th in a row is `boot_fail_cooldown` doubled n-1 times up to
//!   `boot_fail_cooldown_max`, and `boot_fail_trip_after` in a row trip the pool to `Plan`
//!   ([`Blocked::Tripped`]) until the step is given the pool's name in
//!   [`Input::brake_resets`]. A pool is struck at most once in a step. A machine whose host has
//!   been heard from, unbroken, for `boot_fail_reset_after` ends the count; the trip stays. It
//!   only removes launches.
//!   The acquire id is the pool and 128 random bits the caller gives in [`Input::entropy`].
//!   A launch the provider told to wait is retried when its time comes, if the pool is
//!   `Live` and every cap still holds.
//! * Hellos. The first hello of a booting machine is judged against the offer stored for
//!   its acquire by [`verify_hello`] (design 5.6.1, step 6): accelerator model by canonical
//!   name after alias resolution, accelerator count at least the offer's, the same
//!   architecture, and CPU, memory and disk each at least the offer's less
//!   [`Settings::size_tolerance_percent`]. A match registers the machine. A mismatch, in
//!   the one step that judged it, condemns the record and starts its release (the token is
//!   revoked, the shell marks the host ending after the save, and the provider is told to
//!   terminate it every step until it is gone), cools the offer down for
//!   [`Settings::offer_cooldown`] keyed by (provider, instance type, zone)
//!   ([`Records::cooled_offers`], saved with the release, so a restart keeps it), and
//!   counts a failed boot toward the pool's brake, so a wrong alias or a wrong catalog row
//!   cannot loop acquire, release, acquire. A record with no offer to compare
//!   ([`StoredOffer::nothing`]) is registered unjudged and named in [`State::unverified`];
//!   a host whose shape is not known since a restart ([`HostFacts::shape`] `None`) is
//!   registered unjudged too. A host the scaler did not launch has no booting record and
//!   is never judged.
//! * Answers. An acquire's answer is matched to its record by acquire id. `Unknown`
//!   leaves the record unresolved until a list shows the machine or `unknown_hold` has
//!   passed; a machine found after that is released and never uncounted.
//! * Machines the registry does not hear from. A host the registry shows but does not hear
//!   from is held and counted, and [`State::silent`] says since when. Once it books nothing
//!   ([`HostFacts::lease_given_up`], or the registry marked it `Releasing`) and has stayed
//!   silent for `unknown_hold`, the machine is released, since it costs money and holds
//!   nothing: drained and marked for release, which the registry checks when the actions
//!   run, then released when the mark is read back; [`State::silent_released`] names it for
//!   the shell to log. A host that shows work ([`HostFacts::busy`]) or a `Held` or
//!   `Reserved` mark is never released, except that a silent host with nothing booked on it
//!   ([`HostFacts::lease_given_up`]) is not kept by the runs and streams it lists, which a
//!   job that can no longer report leaves behind. A registered host never heard from since
//!   its hello ([`HostFacts::heard_since_hello`] false: its Welcome was lost) that books
//!   nothing is a failed boot, released at `boot_timeout` after its launch with the
//!   boot-failure cooldown, unless it was seen live and heard from on an earlier step
//!   ([`Record::seen_live`]): a host whose agent restarted and said hello again is not a
//!   lost Welcome. Hosts the scaler did not launch are never released. One the registry no
//!   longer shows is released too, but only on a complete registry view
//!   ([`Input::registry_complete`], the shell's duty to vouch for) with the machine missing
//!   on complete views `unknown_hold` apart; otherwise it is held and counted. A booting
//!   machine is given up on at `boot_timeout` on a complete view only, and only once its
//!   host has been missing on complete views for a third of `boot_timeout`, so a restart
//!   does not release a machine whose agent is about to say hello. The times a silent host
//!   was first seen silent and a machine was first seen missing are in [`Records`] with the
//!   time of the step that wrote them, so a scaler whose restarts are further apart than the save
//!   interval (the lesser of a sixth of `unknown_hold` and five minutes) still reaches the
//!   hold by the time it observed; a faster crash loop keeps the host held and counted. The
//!   time it was down is not counted.
//!   One the provider says it no longer has ([`Input::not_found`]) is settled.
//! * Scale down. A registered host quiet for `idle` is drained; a drained host that is
//!   still quiet is marked for release; once the registry shows the mark the token is
//!   revoked and the machine released, every step, until a read by id says gone. There is
//!   no deadline on any of this, and nothing is released that the registry did not
//!   mark while quiet, except a machine the registry has forgotten, which holds no work.
//!   `idle` drains a host only once it has lived `min_host_lifetime`, so a short `idle`
//!   cannot turn each billed boot into a launch for nothing.
//! * Machines that reach `max_age`. A live registered host whose machine was launched
//!   `max_age` ago is retired ([`Action::Retire`]) whether or not it is busy: the mark
//!   takes no new start and cuts no work, adds no deadline, and is never undone for a
//!   request. The release is asked, as for an idle host, only when the host is quiet, and
//!   the machine is not released while a live allocation is on it. A job ends within the
//!   server's ceiling on `max_wall`, so the host empties within it. The age is counted from
//!   [`Record::launched_at`], which is stored, so a restart neither resets it nor stops it
//!   (the cloud bills through the time the scaler was down). A host the registry does not
//!   hear from is left to the silence rules above. Each launch also carries
//!   [`Settings::lifetime`] in [`AcquirePlan::lifetime`]: `max_age` plus the ceiling plus
//!   the prepare timeout, `unknown_hold` and a margin, which a provider with a native limit enforces in the cloud.
//! * Machines in a reserved block of time. When the provider states the end of the block a
//!   machine runs in (`Held::ends_unix_seconds`, in its answer to an acquire and in each
//!   list), the scaler keeps it in [`Record::ends_at`]. Until that instant the machine is
//!   not drained as idle and not retired at `max_age`: the block is paid for to its end. A
//!   machine whose hello did not match its offer is still released at once. From the
//!   instant on, the rules above apply as for any machine, and a machine of the block that
//!   the list shows terminated or preempted, or that a notice names, is released with no
//!   strike on the brake and no cooldown, whatever phase it was in: the cloud took it back
//!   as planned. The same loss before the end is counted as it is for any machine. The end
//!   is never written to the records file; the first list after a restart states it again.
//!   The scaler does not ask a provider for a block: the offer it sends carries none.
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

mod block;
mod brake;
mod budget;
mod caps;
mod go;
mod input;
mod pass;
mod pick;
mod record;
mod scale;
mod settings;
mod state;
mod time;
mod verify;

use std::collections::BTreeSet;

use js_elastic::Demand;

use crate::caps::{check, holding};
use crate::pass::{Pass, host_named};
use crate::pick::decide;
use crate::scale::Claims;

pub use budget::{Budgets, Totals, overrun};
pub use input::{
    AcquireResult, HostFacts, HostState, Input, Mark, PoolInput, ReleaseResult, Waiting,
};
pub use record::{
    Brake, Cooldown, CooldownReason, Go, OLDEST, OfferCooldown, Phase, Record, Records,
    RecordsError, StoredExec, StoredOffer, VERSION,
};
pub use settings::{
    ACQUIRE_RETRY, BOOT_FAIL_COOLDOWN_MAX, BOOT_FAIL_RESET_AFTER, BOOT_FAIL_TRIP_AFTER, Blocked,
    Health, LaunchMode, OFFER_COOLDOWN, QUOTA_BACKOFF, SIZE_TOLERANCE_PERCENT, Settings,
    UNNAMED_PROVIDER,
};
pub use state::{AcquirePlan, Action, PlanLine, State};
pub use time::Millis;
pub use verify::{HelloDimension, Verdict, verify_hello};

/// One step of the scaler at `now`: applies the provider's answers, the lists and the
/// hosts' hellos to `state`, then decides what to drain, release and launch.
///
/// Deterministic: it reads no clock and does no I/O.
#[must_use]
pub fn step(state: State, input: &Input, now: Millis) -> (State, Vec<Action>) {
    let before = state.records.clone();
    let mut pass = Pass::new(state, input, now);
    pass.records.forget_cooled_offers(now);
    pass.reset_brakes();
    pass.end_go();
    for pool in &input.pools {
        pass.reconcile(pool);
    }
    // Every request is matched to what covers it, in every pool, before any pool launches:
    // a request a later pool's host or machine on its way covers gets nothing from an
    // earlier one.
    let mut claims = Claims::default();
    pass.hold_reserved(&mut claims);
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
/// is live and available, pass 2 picks an offer of it (the same pick a step stages), and
/// the caps allow one more machine once the idle hosts that would be released count as
/// free. This is the registry's
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
        let machines = holding(settings, &state.records.instances, &[], &idle).len();
        let price = decide(settings, &pool.offers, demand, machines)
            .ok()
            .and_then(|offer| offer.price_cents_per_hour);
        let within = price.is_some_and(|price| {
            let all = (state.records.instances.as_slice(), [].as_slice());
            [
                check(settings, now, &state.records.instances, &[], &idle, price).is_ok(),
                budget::check(input, &state.records, settings, now, all, price).is_ok(),
            ]
            .into_iter()
            .all(|ok| ok)
        });
        [
            pool.health == Health::Available,
            settings.mode(now) == LaunchMode::Live,
            !state.records.tripped(&settings.pool),
            within,
        ]
        .into_iter()
        .all(|ok| ok)
    })
}
