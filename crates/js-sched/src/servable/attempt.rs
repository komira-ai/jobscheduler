//! One request to place in a round: what it is, which workers are out, and what it will be
//! charged.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_types::sched::usage::LeaseCost;
use js_types::sched::{FarmTime, LeaseId, WorkerId};

use crate::admission::Profile;
use crate::backfill;
use crate::input::Request;

/// One request to place: what it is, which workers are out, and what it will be charged.
pub(crate) struct Attempt<'a> {
    pub(crate) request: &'a Request,
    /// Workers held for a whole-machine lease or a blocked head ahead of it.
    pub(crate) reserved: &'a BTreeSet<WorkerId>,
    /// The start time of each worker in `reserved` (farm milliseconds; `None`: not known):
    /// the request is placed there only as backfill that ends by it.
    pub(crate) starts: &'a BTreeMap<WorkerId, Option<u64>>,
    /// How long the request counts for backfill ([`crate::Backfill`]); `None` never fits.
    pub(crate) backfill: Option<u64>,
    /// Workers barred to it ([`crate::Bar`] other than a back-off): never granted to it.
    pub(crate) barred: &'a BTreeSet<WorkerId>,
    /// Workers its earlier runs ended on as the farm's fault.
    pub(crate) avoid: &'a BTreeSet<WorkerId>,
    /// A key floor raised its memory: a worker it would own whole is out.
    pub(crate) not_whole: bool,
    /// The only workers it may go on, when it is held to some
    /// ([`crate::Scheduler::place_only_on`]).
    pub(crate) only: Option<&'a BTreeSet<WorkerId>>,
    /// The workers whose CPU the caller judges for it, each with the booked CPU in
    /// millicores the pack limit reads there ([`crate::Scheduler::set_cpu_judged`]).
    pub(crate) judged: Option<&'a BTreeMap<WorkerId, u64>>,
    /// What it is charged if placed now (before the clamp to a worker's capacity).
    pub(crate) cost: LeaseCost,
    /// How it is charged over time, if it is (see [`Admission::profile_of`]).
    pub(crate) profile: Option<Arc<Profile>>,
    /// The lease it gets if placed now.
    pub(crate) lease: LeaseId,
    /// `L` of the lease's key where it is charged by a curve, else 1: the minutes the
    /// score's fit looks at.
    pub(crate) fit_minutes: u8,
}

impl Attempt<'_> {
    /// The booked CPU the caller gives for `worker`, if it judges the request's CPU there.
    pub(super) fn judged_on(&self, worker: &WorkerId) -> Option<u64> {
        self.judged.and_then(|judged| judged.get(worker)).copied()
    }

    /// Whether the request may go on `worker` at `now`: one of the workers it is held to,
    /// if it is held to some, not barred to it, and either no one holds it or it fits
    /// there as backfill ([`backfill::fits`]).
    pub(super) fn may_use(&self, worker: &WorkerId, now: FarmTime) -> bool {
        let allowed = self.only.is_none_or(|only| only.contains(worker));
        allowed && !self.barred.contains(worker) && self.may_share(worker, now)
    }

    /// Whether the request may go on `worker` at `now` as far as holds go: a worker no
    /// one holds, or a held one where it fits as backfill ([`backfill::fits`]).
    fn may_share(&self, worker: &WorkerId, now: FarmTime) -> bool {
        !self.reserved.contains(worker)
            || self
                .starts
                .get(worker)
                .is_some_and(|start| backfill::fits(now.as_millis(), self.backfill, *start))
    }
}
