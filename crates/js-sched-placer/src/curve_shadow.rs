//! The decide-in-shadow line of `--placement-curves shadow`
//! (`docs/design/placement-profiles-live.md`, section 4, "Farm-wide, before `on`", step 2): beside
//! each placement round the flat rule runs, the engine judges the same round by the curve packing
//! rule `on` would use, acts on nothing it finds, and logs one line for each job the two place
//! differently:
//!
//! `a job's curve booking would place it differently`, with `job`, `class`, `flat_host` (absent
//! when the flat placement placed it nowhere), `curve_host` (absent when no host fits its curve)
//! and `slot` (the first slot over the cap on `flat_host`, 0 to 63 for the 2 s slots of the next
//! 128 s and 64 for the slot from 128 s on; absent with `flat_host`), under the log target
//! `js_sched_placer::sizes::profile`.
//!
//! The curve rule's round, as it is judged here:
//! - **The tables** are those `on` would build ([`super::CurveBook`]): every host the settings
//!   name (all when none is named), each live job charged by its class's curve at its age, the
//!   residual of the host's readings carried to every slot. Under shadow they follow the flat
//!   placements, since those are the placements made.
//! - **The flat host is kept** when the job's curve booking (its curve, or its flat number at
//!   every slot) fits its table at every slot, or when it is no curve host: no line.
//! - **Otherwise the curve's host** is one of the round's open hosts (capable, allowed, with no
//!   port of the job held) other than the flat host, the least full at its fullest booked slot with
//!   the job added (as the pack limit ranks hosts past it), the host id breaking a tie: a curve
//!   host whose table the booking fits at every slot, which the ledger has room on but for its CPU
//!   and which the scheduler refused in the round for no kind but `cpu` (the test the curve
//!   replaces); or, when the flat placement placed the job, a host that is no curve host and that
//!   the round offered and the scheduler did not refuse.
//! - A job the flat placement placed nowhere and no host fits by curve either is no line, and a
//!   job logged while it waited is not logged again until the flat placement places it or it
//!   leaves the queue.
//!
//! What this leaves out, so the line is a bound and not a replay: the scheduler's ranking among
//! the hosts both rules admit (a job whose flat host the curve fits is never logged, though the
//! pack limit's fullest slot might order the hosts otherwise), its refusals that carry no kind
//! (a lease cap, a request larger than the host), and the wait bound: under `on` a job whose curve
//! fits no host waits at most `server.curve_max_wait_seconds` and is then tested with its flat
//! number, so a line with no `curve_host` is a delay of at most that bound, not a job that never
//! starts. Nothing here changes what the flat placement places, books, refuses or tells the
//! queue status.

use std::collections::BTreeSet;

use js_sched::RefusalKind;
use js_types::HostId;
use js_types::sched::WorkerId;
use tracing::info;

use super::super::Engine;
use super::super::jobs_walker::Walker;
use super::{booking_of, scale};
use crate::map;
use crate::sizes::Curves;

/// The flat placement round of one queued job, as the shadow judges it.
pub(in crate::engine) struct ShadowRound<'a> {
    /// The hosts open to the job: capable, allowed by the walk and with no port of it held.
    open: &'a [&'a HostId],
    /// The hosts the round offered the scheduler.
    offered: &'a BTreeSet<WorkerId>,
    /// The host the flat placement placed the job on.
    placed: Option<&'a HostId>,
    /// The refusals of the round's hosts, by kind.
    refused: &'a [(WorkerId, RefusalKind)],
}

impl<'a> ShadowRound<'a> {
    /// The round over `open` that offered `offered`, placed the job on `placed` and in which
    /// the scheduler refused as `refused` says.
    pub(in crate::engine) fn new(
        open: &'a [&'a HostId],
        offered: &'a BTreeSet<WorkerId>,
        placed: Option<&'a HostId>,
        refused: &'a [(WorkerId, RefusalKind)],
    ) -> Self {
        Self {
            open,
            offered,
            placed,
            refused,
        }
    }

    /// The kinds the scheduler refused `host` for in the round.
    fn kinds(&self, host: &HostId) -> impl Iterator<Item = RefusalKind> + '_ {
        let worker = map::worker(host);
        let of_host = self.refused.iter().filter(move |(on, _)| *on == worker);
        of_host.map(|(_, kind)| *kind)
    }
}

impl Engine {
    /// Judges `round`, the flat placement of `walker`, by the curve packing rule under
    /// [`Curves::Shadow`], and logs the line of this module when the two differ; nothing under
    /// the other modes. Called before the placement, if any, is held, so the tables and the ledger
    /// are as the round found them.
    pub(in crate::engine) fn judge_in_shadow(&mut self, walker: &Walker, round: &ShadowRound<'_>) {
        if self.curves != Curves::Shadow {
            return;
        }
        let flat = walker.request.resources.cpu_millis;
        let booking = booking_of(walker.curve.as_ref().map(|own| &own.curve), flat);
        let table = round.placed.and_then(|host| self.book.tables.get(host));
        let slot = table.and_then(|table| table.first_slot_over(&booking));
        let differs = match round.placed {
            Some(_) => slot.is_some(),
            None => !self.book.shadowed.contains(&walker.job),
        };
        if !differs {
            return;
        }
        let curve_host = self.curve_host(walker, round, &booking);
        if round.placed.is_none() {
            if curve_host.is_none() {
                return;
            }
            self.book.shadowed.insert(walker.job.clone());
        }
        let slot = slot.map(|slot| u64::try_from(slot).unwrap_or(u64::MAX));
        info!(
            target: "js_sched_placer::sizes::profile",
            job = %walker.job,
            class = %walker.class.as_deref().unwrap_or_default(),
            flat_host = round.placed.map(tracing::field::display),
            curve_host = curve_host.map(tracing::field::display),
            slot,
            "a job's curve booking would place it differently"
        );
    }

    /// The host the curve rule would place `walker` on in `round`, its booking being `booking`
    /// at every slot: see the module's head.
    fn curve_host<'a>(
        &self,
        walker: &Walker,
        round: &ShadowRound<'a>,
        booking: &[u64],
    ) -> Option<&'a HostId> {
        let flat = walker.request.resources.cpu_millis;
        let fullest = |host: &HostId| -> Option<(u64, u64)> {
            match self.book.tables.get(host) {
                Some(table) => {
                    let not_cpu = round.kinds(host).any(|kind| kind != RefusalKind::Cpu);
                    let fits = !not_cpu
                        & self.room.fits_without_cpu(host, walker.booked)
                        & table.first_slot_over(booking).is_none();
                    fits.then(|| (table.fullest(booking), table.limit))
                }
                None => {
                    let worker = map::worker(host);
                    let offered = round.offered.contains(&worker);
                    let taken =
                        round.placed.is_some() & offered & round.kinds(host).next().is_none();
                    let view = taken.then(|| self.sched.load_view(&worker)).flatten();
                    view.map(|view| {
                        let booked = view.left_cpu_millis.saturating_add(flat);
                        (booked, self.limit_of(&worker))
                    })
                }
            }
        };
        let others = round.open.iter().copied();
        let others = others.filter(|host| Some(*host) != round.placed);
        let ranked = others.filter_map(|host| {
            let (booked, limit) = fullest(host)?;
            Some((scale(booked, 1_000_000, limit.max(1)), host))
        });
        ranked.min().map(|(_, host)| host)
    }
}

#[cfg(test)]
#[path = "curve_shadow_tests.rs"]
mod tests;
