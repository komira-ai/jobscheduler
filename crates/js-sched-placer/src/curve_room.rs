//! What a host has room for by CPU curve under `--placement-curves on`
//! (`docs/design/placement-profiles-live.md`, sections 3 to 5): the host's booked CPU at each
//! slot of the next 128 s, the packing rule over it, the wait bound of a job whose curve fits no
//! host, and what `/metrics` reads of it.
//!
//! A host the settings book by curve ([`crate::curves::CurveHosts::books`]) has, for each walk,
//! a table of 65 slots: `L(k) = sum_j C_j(k) + R`, where `C_j(k)` is what its live job `j`
//! is charged `k` slots ahead at its age ([`js_sizing::Curve::charge`], its flat number when
//! its class books flat), and `R` is the residual, what the host's readings and the
//! scheduler's charge show beyond those charges ([`residual`]), carried to every slot. A job
//! fits the host when, at every slot, `L(k) + B(k)` is within the host's cores times the CPU
//! slack, `B` being the job's own booking per slot ([`booking_of`]): its curve, or its flat
//! number at every slot. This replaces the CPU axis of the store's capacity check and the
//! scheduler's CPU test on that host; memory, disk, GPUs and the rest stay as they are. A host
//! the settings do not book, and every host under [`Curves::Off`] and [`Curves::Shadow`], is
//! judged as ever. Under [`Curves::Shadow`] the engine keeps the same live jobs and tables, and
//! judges each flat placement round by them only to log the jobs the curve rule would place
//! differently ([`shadow`]); nothing it places, books, commits or refuses reads them.
//!
//! A job allocated and not yet started is age 0. The engine learns each live job's class and
//! start from the job table once a pass ([`SchedPlacer::read_live_curves`]); a job whose record
//! cannot be read is charged its flat number. A curve exists only here: the booking a job is
//! stored and committed with, and its agent's `cpu.weight`, are its flat number in every mode.
//!
//! A job of a curve class that no curve host fits for [`crate::curves::CurveSettings::max_wait`] is tested
//! with its flat number instead, from then on; the wait is kept in memory only, so a restart
//! starts it again.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{
    AllocationId, AllocationRecord, AllocationStep, Booked, Capacity, IdSource, JobId, JobRecord,
    JobStore, Needs,
};
use js_sched::{LoadView, Refusal, RefusalKind};
use js_sizing::{CURVE_SLOTS, Curve, Key};
use js_types::HostId;
use js_types::sched::{FarmTime, WorkerId};
use tracing::{info, warn};

use super::jobs::JobHosts;
use super::jobs_walker::Walker;
use super::{Engine, jobs};
use crate::curves::CurveView;
use crate::map;
use crate::place::SchedPlacer;
use crate::sizes::{Curves, JobSizes, class_name, millis};

/// A live allocation on a host, as the host's table charges it.
#[derive(Debug, Clone)]
pub(super) struct LiveJob {
    /// The allocation, so a job placed again is read again.
    allocation: AllocationId,
    /// Its host.
    host: HostId,
    /// What its allocation books, in millicores: its flat number.
    flat_millis: u64,
    /// Its class, once its record is read, when it books by its class's curve: it left its CPU
    /// at 0 ([`leaves_cpu`]). A job that named its CPU is charged its flat number, as it was
    /// booked.
    key: Option<Key>,
    /// When it started, in milliseconds since the epoch, once it has.
    started_ms: Option<u64>,
}

/// The CPU booking of a queued job that books by its class's curve.
pub(super) struct Shaped {
    /// What it books at each slot.
    pub(super) curve: Curve,
}

/// What the live curve mode keeps beside the scheduler.
#[derive(Debug, Default)]
pub(super) struct CurveBook {
    /// The live allocations of the job table on hosts, as of the last pass, with what the
    /// engine read of their jobs.
    live: BTreeMap<JobId, LiveJob>,
    /// Each queued job whose curve found no host, with the walk time of the first such round.
    waits: BTreeMap<JobId, FarmTime>,
    /// The queued jobs whose wait bound ran out: tested with their flat number from then on.
    expired: BTreeSet<JobId>,
    /// How many wait bounds ran out since the pass last took the count.
    expiries: u64,
    /// The table of each curve host of the walk under way, or of the last walk.
    tables: BTreeMap<HostId, HostTable>,
    /// The fullest booked slot of each curve host as the last walk ended.
    peaks: BTreeMap<HostId, u64>,
    /// Under [`Curves::Shadow`], the queued jobs logged as placed by their curve while the flat
    /// placement placed them nowhere: logged once while they wait, until placed or gone from the
    /// queue ([`shadow`]).
    shadowed: BTreeSet<JobId>,
}

/// A curve host's booked CPU at each slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostTable {
    /// `sum_j C_j(k)` for each slot `k`, the last being the slot from 128 s on.
    charges: Vec<u64>,
    /// What the readings show beyond the charges, at every slot.
    residual: u64,
    /// The most the host may be booked: its cores times the CPU slack.
    limit: u64,
}

impl HostTable {
    /// `L(k)`: the charges of the jobs at slot `k` and the residual.
    fn load(&self, slot: usize) -> u64 {
        let charged = self.charges.get(slot).copied().unwrap_or(0);
        charged.saturating_add(self.residual)
    }

    /// The first slot at which a job booking `booking` does not fit.
    fn first_slot_over(&self, booking: &[u64]) -> Option<usize> {
        let over =
            |(slot, new): &(usize, &u64)| self.load(*slot).saturating_add(**new) > self.limit;
        booking.iter().enumerate().find(over).map(|(slot, _)| slot)
    }

    /// The first slot at which a job booking `booking` does not fit, as a refusal.
    fn first_over(&self, booking: &[u64]) -> Option<Refusal> {
        self.first_slot_over(booking).map(|slot| Refusal {
            kind: RefusalKind::CpuCurve,
            counted: self.load(slot),
            new: booking.get(slot).copied().unwrap_or(0),
            limit: self.limit,
            measured_cpu_millis: None,
        })
    }

    /// The fullest slot of the charges with `booking` added, with no residual (a reading is
    /// no booking): what the pack limit reads.
    fn fullest(&self, booking: &[u64]) -> u64 {
        let slots = self.charges.iter().zip(booking);
        let fullest = slots.map(|(charged, new)| charged.saturating_add(*new));
        fullest.max().unwrap_or(0)
    }

    /// The fullest slot of the charges.
    fn peak(&self) -> u64 {
        self.charges.iter().copied().max().unwrap_or(0)
    }

    /// Adds a job's `charges` to the table.
    fn add(&mut self, charges: &[u64]) {
        for (held, charge) in self.charges.iter_mut().zip(charges) {
            *held = held.saturating_add(*charge);
        }
    }
}

/// What a job books at each slot: `curve`'s booking, or `flat_millis` at every slot.
fn booking_of(curve: Option<&Curve>, flat_millis: u64) -> Vec<u64> {
    let slots = 0..CURVE_SLOTS;
    slots
        .map(|slot| curve.map_or(flat_millis, |curve| curve.booking(slot)))
        .collect()
}

/// What a running job `age_ms` old is charged at each slot ahead: `curve`'s charge, or
/// `flat_millis` at every slot.
fn charges_of(curve: Option<&Curve>, age_ms: u64, flat_millis: u64) -> Vec<u64> {
    let slots = 0..CURVE_SLOTS;
    let charge = |curve: &Curve, slot| curve.charge(age_ms, slot, flat_millis);
    slots
        .map(|slot| curve.map_or(flat_millis, |curve| charge(curve, slot)))
        .collect()
}

/// The residual of a host: what its readings and its scheduler charge show beyond its jobs'
/// charges, `max(0, left - flat_sum, measured + vm - charged)`, all saturating. `left` is the
/// scheduler's charge at slot 0 (every lease at its flat cost, the readings and the VMs in),
/// `flat_sum` what the host's live jobs book, `measured` the cores its readings show demanded,
/// busy and waiting for a CPU ([`LoadView::measured_cpu_millis`]; 0 with no usable reading),
/// `vm` the VMs' CPU and `charged` the jobs' charges at slot 0. With every job charged flat the
/// host's slot 0 is the scheduler's own.
fn residual(left: u64, flat_sum: u64, (measured, vm): (u64, u64), charged: u64) -> u64 {
    let booked = left.saturating_sub(flat_sum);
    let beyond = measured.saturating_add(vm).saturating_sub(charged);
    booked.max(beyond)
}

/// `value * num / den`, saturating.
fn scale(value: u64, num: u64, den: u64) -> u64 {
    u64::try_from(u128::from(value) * u128::from(num) / u128::from(den)).unwrap_or(u64::MAX)
}

/// Whether the queued `job` leaves its CPU at 0, to be sized from its class's history: a job
/// that names its CPU books it flat, and one that books the whole host books none.
pub(super) fn leaves_cpu(job: &JobRecord) -> bool {
    match &job.spec.needs {
        Needs::Shape(resources) => resources.cpu_millis() == 0,
        Needs::WholeHost => false,
    }
}

/// The hosts of a round that the curve rule leaves, and the booked CPU the pack limit reads
/// on each curve host among them.
pub(super) struct Curved<'a> {
    /// The hosts to place on: the plain hosts and the curve hosts the job fits.
    pub(super) hosts: Vec<&'a HostId>,
    /// For each curve host of `hosts`, the fullest slot of its booked curve with the job added.
    pub(super) judged: BTreeMap<WorkerId, u64>,
}

/// The curve hosts of a round the job fits, with the refusals of those it does not.
struct Fitted<'a> {
    hosts: Vec<(&'a HostId, u64)>,
    refused: Vec<(&'a HostId, Refusal)>,
}

impl Engine {
    /// Whether `host` books by curve now: the mode is on and the settings name it.
    fn books_by_curve(&self, host: &HostId) -> bool {
        (self.curves == Curves::On) & self.curve.hosts.books(host)
    }

    /// Whether `host` has a table in a walk: the mode is on or shadow and the settings name it.
    /// Under shadow the table is only judged by ([`shadow`]), never placed by.
    fn judges_by_curve(&self, host: &HostId) -> bool {
        (self.curves != Curves::Off) & self.curve.hosts.books(host)
    }

    /// Whether `host` has room for `booked` but for its CPU, which the curve judges, when it is
    /// a curve host of the walk under [`Curves::On`]; as the ledger says otherwise.
    pub(super) fn room_fits(&self, host: &HostId, booked: Booked) -> bool {
        if (self.curves == Curves::On) & self.book.tables.contains_key(host) {
            self.room.fits_without_cpu(host, booked)
        } else {
            self.room.fits(host, booked)
        }
    }

    /// The capacity the store holds a placement on `host` to, which `capacity` is: the CPU
    /// lifted off a host that books by curve (rule 1 judged it), the rest as it is.
    pub(super) fn commit_capacity(
        &self,
        host: &HostId,
        capacity: Option<&Capacity>,
    ) -> Option<Capacity> {
        let lifted = |capacity: &Capacity| Capacity {
            cpu_millis: u64::MAX,
            ..*capacity
        };
        capacity.map(|capacity| {
            if self.books_by_curve(host) {
                lifted(capacity)
            } else {
                *capacity
            }
        })
    }

    /// What `/metrics` reads of the curve mode; `None` unless it is on.
    pub(super) fn curve_view_of(&self) -> Option<CurveView> {
        let (curve_classes, flat_classes) = self.sizes.curve_counts();
        let view = CurveView {
            curve_classes,
            flat_classes,
            host_peaks: self.book.peaks.clone(),
        };
        (self.curves == Curves::On).then_some(view)
    }

    /// How many wait bounds ran out since the last call.
    pub(crate) fn take_curve_expiries(&mut self) -> u64 {
        std::mem::take(&mut self.book.expiries)
    }

    /// Brings the live curve jobs to `live`, the live allocations of the job table: an entry
    /// whose allocation is gone (or placed again) is dropped, each share allocation not held is
    /// taken up, and the jobs whose start is not known are answered, to be read. Nothing under
    /// [`Curves::Off`].
    pub(super) fn sync_live_curves(&mut self, live: &[AllocationRecord]) -> Vec<JobId> {
        if self.curves == Curves::Off {
            return Vec::new();
        }
        let shares = live
            .iter()
            .filter(|allocation| allocation.step != AllocationStep::Freed);
        let shares: BTreeMap<&JobId, (&AllocationRecord, u64)> = shares
            .filter_map(|allocation| match allocation.booked {
                Booked::Shape { cpu_millis, .. } => {
                    Some((&allocation.job, (allocation, cpu_millis)))
                }
                Booked::WholeHost => None,
            })
            .collect();
        self.book.live.retain(|job, held| {
            let same = |(allocation, _): &(&AllocationRecord, u64)| {
                allocation.allocation_id == held.allocation
            };
            shares.get(job).is_some_and(same)
        });
        for (job, (allocation, flat_millis)) in shares {
            self.book
                .live
                .entry(job.clone())
                .or_insert_with(|| LiveJob {
                    allocation: allocation.allocation_id,
                    host: allocation.host.clone(),
                    flat_millis,
                    key: None,
                    started_ms: None,
                });
        }
        let unstarted = self
            .book
            .live
            .iter()
            .filter(|(_, held)| held.started_ms.is_none());
        unstarted.map(|(job, _)| job.clone()).collect()
    }

    /// Records what the record of a live job says: its class and when it started.
    fn read_live_curve(&mut self, job: &JobRecord) {
        let started = job.started_at.map(|at| millis(at.as_duration()));
        let held = self.book.live.get_mut(&job.id);
        held.into_iter().for_each(|held| {
            held.key = leaves_cpu(job).then(|| JobSizes::key_of(job));
            held.started_ms = started;
        });
    }

    /// Builds the table of each of `hosts` that books by curve, as the walk at `now` finds it,
    /// and forgets the table of every other host; under [`Curves::Shadow`] the tables of the hosts
    /// `on` would book by curve, to judge by. The jobs the queue no longer holds leave the wait
    /// bound's books and the shadow's.
    pub(super) fn begin_curve_walk<'a>(
        &mut self,
        hosts: impl Iterator<Item = &'a HostId>,
        queued: &[JobRecord],
        now: FarmTime,
    ) {
        let ids: BTreeSet<&JobId> = queued.iter().map(|job| &job.id).collect();
        self.book.waits.retain(|job, _| ids.contains(job));
        self.book.expired.retain(|job| ids.contains(job));
        self.book.shadowed.retain(|job| ids.contains(job));
        let curved = hosts.filter(|host| self.judges_by_curve(host));
        let tables = curved.map(|host| (host.clone(), self.table_of(host, now)));
        self.book.tables = tables.collect();
    }

    /// The most `worker` may be booked: its cores times the CPU slack (0 for a worker the
    /// scheduler does not know).
    fn limit_of(&self, worker: &WorkerId) -> u64 {
        let slack = u64::from(self.cpu_slack_permille);
        let cores = self
            .sched
            .capacity(worker)
            .map_or(0, |capacity| capacity.cpu_millis);
        scale(cores, 1000 + slack, 1000)
    }

    /// The table of `host` as the walk at `now` finds it.
    fn table_of(&self, host: &HostId, now: FarmTime) -> HostTable {
        let worker = map::worker(host);
        let mut table = HostTable {
            charges: vec![0; CURVE_SLOTS],
            residual: 0,
            limit: self.limit_of(&worker),
        };
        let mut flat_sum = 0u64;
        for held in self.book.live.values().filter(|held| held.host == *host) {
            let age = held
                .started_ms
                .map_or(0, |at| now.as_millis().saturating_sub(at));
            let curve = held.key.as_ref();
            let curve = curve.and_then(|key| self.sizes.curve_of(key, held.flat_millis, now));
            table.add(&charges_of(curve.as_ref(), age, held.flat_millis));
            flat_sum = flat_sum.saturating_add(held.flat_millis);
        }
        let view = self.sched.load_view(&worker);
        table.residual = view.map_or(0, |view| {
            residual_of(&view, flat_sum, table.charges.first().copied().unwrap_or(0))
        });
        table
    }

    /// `walker` with the curve its class books, when it books one: its job left its CPU at 0 (it
    /// is `job`'s sized form), the mode is on (or shadow, which only judges by it), the class
    /// passes its four checks and the job's wait bound has not run out; and with its class's name
    /// unless the mode is off.
    pub(super) fn shaped(&self, walker: Walker, job: &JobRecord, now: FarmTime) -> Walker {
        let own = leaves_cpu(job) & !self.book.expired.contains(&walker.job);
        let flat = walker.request.resources.cpu_millis;
        let key = JobSizes::key_of(job);
        let curve = own.then(|| self.sizes.curve_of(&key, flat, now)).flatten();
        let curve = curve.map(|curve| Shaped { curve });
        let class = (self.curves != Curves::Off).then(|| class_name(job));
        Walker {
            curve,
            class,
            ..walker
        }
    }

    /// The hosts of `free` the curve rule leaves `walker` under [`Curves::On`] (every host of
    /// `free` otherwise): those that are no curve host of the walk, and the curve hosts it fits
    /// at every slot. A curve host it does not fit is refused as
    /// `cpu-curve`, with the first slot over the cap. A walker with a curve that fits none of
    /// the curve hosts starts its wait, and past the wait bound is tested again with its flat
    /// number at every slot, from then on ([`crate::curves::CurveSettings::max_wait`]).
    pub(super) fn by_curve<'a>(
        &mut self,
        walker: &Walker,
        free: Vec<&'a HostId>,
        now: FarmTime,
    ) -> Curved<'a> {
        let on = self.curves == Curves::On;
        let (curved, mut hosts): (Vec<&HostId>, Vec<&HostId>) = free
            .into_iter()
            .partition(|host| on & self.book.tables.contains_key(*host));
        if curved.is_empty() {
            return Curved {
                hosts,
                judged: BTreeMap::new(),
            };
        }
        let flat = walker.request.resources.cpu_millis;
        let own = walker.curve.as_ref();
        let mut fitted = self.fit_tables(&curved, &booking_of(own.map(|own| &own.curve), flat));
        // A round in which the curve fits a host ends the wait: the bound counts the time no
        // host fitted the curve, not the time since the first such round.
        if own.is_some() & !fitted.hosts.is_empty() {
            self.book.waits.remove(&walker.job);
        }
        if own.is_some() & fitted.hosts.is_empty() && self.wait_over(walker, now) {
            fitted = self.fit_tables(&curved, &booking_of(None, flat));
        }
        for (host, refusal) in fitted.refused {
            self.book_refusal(&walker.job, map::worker(host), refusal);
        }
        hosts.extend(fitted.hosts.iter().map(|(host, _)| *host));
        let judged = fitted
            .hosts
            .into_iter()
            .map(|(host, fullest)| (map::worker(host), fullest));
        Curved {
            hosts,
            judged: judged.collect(),
        }
    }

    /// The hosts of `curved` whose table `booking` fits at every slot, each with the fullest slot
    /// of its charges with `booking` added, and the refusal of each it does not fit.
    fn fit_tables<'a>(&self, curved: &[&'a HostId], booking: &[u64]) -> Fitted<'a> {
        let mut fitted = Fitted {
            hosts: Vec::new(),
            refused: Vec::new(),
        };
        let tables = curved
            .iter()
            .filter_map(|host| self.book.tables.get(*host).map(|table| (*host, table)));
        for (host, table) in tables {
            match table.first_over(booking) {
                Some(refusal) => fitted.refused.push((host, refusal)),
                None => fitted.hosts.push((host, table.fullest(booking))),
            }
        }
        fitted
    }

    /// Notes that `walker`'s curve found no curve host at `now`, and whether it has found none
    /// for the wait bound: then the job is tested with its flat number from now on, counted once
    /// and logged once.
    fn wait_over(&mut self, walker: &Walker, now: FarmTime) -> bool {
        let since = *self.book.waits.entry(walker.job.clone()).or_insert(now);
        let waited = now.as_millis().saturating_sub(since.as_millis());
        let over = u128::from(waited) >= self.curve.max_wait.as_millis();
        if over {
            self.book.expired.insert(walker.job.clone());
            self.book.expiries = self.book.expiries.saturating_add(1);
            info!(
                target: "js_sched_placer::sizes::profile",
                job = %walker.job,
                class = %walker.class.as_deref().unwrap_or_default(),
                waited_ms = waited,
                "a job's curve found no host within the wait bound; it is tested with its flat number"
            );
        }
        over
    }

    /// Tells `judged`, the fullest booked slot of each curve host the job may be placed on, to
    /// the scheduler for `operation`: its pack limit reads that figure as the host's booked CPU.
    pub(super) fn judge_cpu(
        &mut self,
        operation: js_types::sched::OperationId,
        judged: BTreeMap<WorkerId, u64>,
    ) {
        if !judged.is_empty() {
            self.sched.set_cpu_judged(operation, judged);
        }
    }

    /// Adds `walker`, placed on `host`, to its table, charged at age 0 (under [`Curves::Shadow`]
    /// too, where the table follows the flat placements), and forgets its wait and whether the
    /// shadow logged it while it waited.
    pub(super) fn placed_by_curve(&mut self, walker: &Walker, host: &HostId) {
        self.book.waits.remove(&walker.job);
        self.book.expired.remove(&walker.job);
        self.book.shadowed.remove(&walker.job);
        let flat = walker.request.resources.cpu_millis;
        let curve = walker.curve.as_ref().map(|own| &own.curve);
        let charges = charges_of(curve, 0, flat);
        self.book
            .tables
            .get_mut(host)
            .into_iter()
            .for_each(|table| table.add(&charges));
    }

    /// Keeps the fullest booked slot of each curve host as the walk ended, for `/metrics`.
    pub(super) fn end_curve_walk(&mut self) {
        let peaks = self.book.tables.iter();
        self.book.peaks = peaks
            .map(|(host, table)| (host.clone(), table.peak()))
            .collect();
    }
}

/// [`residual`] of the host the scheduler's `view` shows.
fn residual_of(view: &LoadView, flat_sum: u64, charged: u64) -> u64 {
    let measured = view.measured_cpu_millis.filter(|_| !view.fallback);
    let measured = measured.map_or(0, u64::from);
    residual(
        view.left_cpu_millis,
        flat_sum,
        (measured, view.vm_cpu_millis),
        charged,
    )
}

impl SchedPlacer {
    /// Reads from `jobs` the class and the start of each live job of `live` the engine has not
    /// read a start of, under the curve mode: a read that fails is logged and the job is
    /// charged its flat number.
    pub(super) fn read_live_curves<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        live: &[AllocationRecord],
    ) {
        let unread = self.engine().sync_live_curves(live);
        for id in unread {
            match jobs.get_job(&id) {
                Ok(found) => {
                    let mut engine = self.engine();
                    found.iter().for_each(|job| engine.read_live_curve(job));
                }
                Err(error) => {
                    warn!(job = %id, %error, "a live job was not read; it is charged its flat number")
                }
            }
        }
    }

    /// Tells `metrics` of each wait bound that ran out since the last pass.
    pub(super) fn tell_expiries(&self, metrics: &dyn jobs::PassMetrics) {
        let expired = self.engine().take_curve_expiries();
        (0..expired).for_each(|_| metrics.curve_wait_expired());
    }

    /// The capacity the store holds a placement of `job` on `host` to: that of the host's offer
    /// in `hosts`, its CPU lifted when the host books by curve.
    pub(super) fn commit_capacity_of(
        &self,
        hosts: &JobHosts<'_>,
        host: &HostId,
    ) -> Option<Capacity> {
        let capacity = jobs::capacity_of(hosts, host);
        self.engine().commit_capacity(host, capacity)
    }
}

#[path = "curve_shadow.rs"]
pub(super) mod shadow;

#[cfg(test)]
#[path = "curve_room_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "curve_pass_tests.rs"]
mod pass_tests;
