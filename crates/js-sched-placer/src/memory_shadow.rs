//! The memory shadow (`docs/design/placement-profiles-live.md`, "Memory over time, in shadow"):
//! under `--placement-curves shadow` and `on`, beside the flat memory booking that places every
//! job, what a booking that follows each class's memory curve ([`js_sizing::MemoryCurve`]) would
//! have allowed. Nothing it computes reaches a placement, a refusal, a commit or an agent.
//!
//! At each walk, each open host the room holds to a memory capacity has a table of 65 slots
//! (64 of 2 s, then one from 128 s on): `P(k) = sum_j M_j(k) + R`, where `M_j(k)` is what its
//! live job `j` is projected to hold `k` slots ahead at its age ([`js_sizing::MemoryCurve::charge`];
//! its flat memory booking when its class has no memory curve or it named its memory), and `R`
//! the residual, what the host's last reading (when it is at most [`FRESH_MS`] old) showed
//! beyond `M(0)`, carried to every slot. A queued job would fit the host by its curve when, at
//! every slot, `P(k) + B(k)` is within the host's memory capacity, `B` being its own curve's
//! booking (its flat memory booking at every slot when it has none), and the room has space for
//! it on every other axis. The scheduler's load test is not run for it.
//!
//! Each placement is then one of three: `sooner`, when at an earlier walk the job was placed
//! nowhere while its curve fitted a host the flat memory booking refused; `later`, when its curve
//! does not fit the host the flat booking chose; else `same`. A `sooner` or `later` placement logs
//! one line. Each host's reading is compared with the projection the last walk made of the
//! reading's moment, and a reading above it logs one line. The counts are kept since the
//! server started, for `/metrics` ([`crate::curves::MemoryShadowView`]).

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{JobId, JobRecord};
use js_sizing::{CURVE_SLOTS, Key, MemoryCurve, age_slot};
use js_types::HostId;
use js_types::sched::FarmTime;
use js_types::sched::usage::NodeLoad;
use tracing::info;

use super::Engine;
use super::curve_room::leaves_memory;
use super::jobs_walker::Walker;
use crate::curves::MemoryShadowView;
use crate::sizes::{Curves, JobSizes, class_name};

/// One mebibyte, the unit of the lines.
const MIB: u64 = 1 << 20;

/// A reading older than this at a walk is not taken into its residual: three windows of 5 s, the
/// age at which the scheduler takes a reading for stale.
pub(super) const FRESH_MS: u64 = 15_000;

/// A queued job of the walk under way: its class, and its class's key when it left its memory
/// at 0.
#[derive(Debug)]
struct Queued {
    key: Option<Key>,
    class: String,
}

/// A host's projected memory at each slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MemoryTable {
    /// `sum_j M_j(k)` of the host's live jobs and the walk's placements on it, at each slot.
    pub(super) held: Vec<u64>,
    /// What the host's fresh reading showed beyond `held[0]`, at every slot.
    pub(super) residual: u64,
    /// The memory the host can book.
    pub(super) limit: u64,
}

impl MemoryTable {
    /// The first slot at which `booking` does not fit with the projection.
    fn first_over(&self, booking: &[u64]) -> Option<usize> {
        let over = |(held, new): (&u64, &u64)| {
            held.saturating_add(self.residual).saturating_add(*new) > self.limit
        };
        self.held.iter().zip(booking).position(over)
    }

    /// The projection at each slot: the jobs' and the residual.
    fn projected(&self) -> Vec<u64> {
        let with = |held: &u64| held.saturating_add(self.residual);
        self.held.iter().map(with).collect()
    }
}

/// What the memory shadow keeps beside the scheduler.
#[derive(Debug, Default)]
pub(super) struct MemoryShadow {
    /// The queued jobs of the walk under way.
    queued: BTreeMap<JobId, Queued>,
    /// Each class's memory curve for a flat booking, as the walk under way read it.
    curves: BTreeMap<(Key, u64), Option<MemoryCurve>>,
    /// Each queued job placed nowhere while its curve fitted a host the flat memory booking
    /// refused, with the walk time of the first such round.
    fits_since: BTreeMap<JobId, FarmTime>,
    /// The table of each host of the walk under way, or of the last walk.
    pub(super) tables: BTreeMap<HostId, MemoryTable>,
    /// The projection of each host as the last walk ended, with that walk's time.
    projections: BTreeMap<HostId, (FarmTime, Vec<u64>)>,
    /// Each host's last reading: its time and the memory it showed in use.
    readings: BTreeMap<HostId, (FarmTime, u64)>,
    /// The counts `/metrics` reads.
    view: MemoryShadowView,
}

/// How a round's job would fare by its memory curve, judged before the round places it.
pub(super) struct Judged {
    /// Its curve, when its class has one.
    curve: Option<MemoryCurve>,
    /// Its flat memory booking.
    flat: u64,
    /// What it would book at each slot: its curve's booking, or its flat booking.
    booking: Vec<u64>,
    /// Whether its curve fits a host the flat memory booking refuses.
    refused_fits: bool,
}

/// What a job books at each slot: `curve`'s booking, or `flat` at every slot.
fn booking_of(curve: Option<&MemoryCurve>, flat: u64) -> Vec<u64> {
    let slots = 0..CURVE_SLOTS;
    slots
        .map(|slot| curve.map_or(flat, |curve| curve.booking(slot)))
        .collect()
}

/// What a running job `age_ms` old is projected to hold at each slot ahead: `curve`'s charge, or
/// `flat` at every slot.
fn charges_of(curve: Option<&MemoryCurve>, age_ms: u64, flat: u64) -> Vec<u64> {
    let slots = 0..CURVE_SLOTS;
    slots
        .map(|slot| curve.map_or(flat, |curve| curve.charge(age_ms, slot)))
        .collect()
}

/// `held` with `charges` added at each slot.
fn add(held: &mut [u64], charges: &[u64]) {
    for (held, charge) in held.iter_mut().zip(charges) {
        *held = held.saturating_add(*charge);
    }
}

impl Engine {
    /// Whether the memory shadow runs: under [`Curves::Shadow`] and [`Curves::On`].
    fn shadows_memory(&self) -> bool {
        self.curves != Curves::Off
    }

    /// The memory curve of `key`'s class for a flat booking of `flat` at `now`, read once a walk.
    fn memory_curve(&mut self, key: &Key, flat: u64, now: FarmTime) -> Option<MemoryCurve> {
        let sizes = &self.sizes;
        let cached = self.memory.curves.entry((key.clone(), flat));
        let curve = cached.or_insert_with(|| sizes.memory_curve_of(key, flat, now));
        curve.clone()
    }

    /// Begins the memory shadow's walk at `now` over `hosts`, with `queued` the jobs it walks:
    /// builds the table of each host the room holds to a memory capacity. The jobs the queue no
    /// longer holds leave its books. Nothing under [`Curves::Off`].
    pub(super) fn begin_memory_walk(
        &mut self,
        hosts: &[&HostId],
        queued: &[JobRecord],
        now: FarmTime,
    ) {
        if !self.shadows_memory() {
            return;
        }
        let ids: BTreeSet<&JobId> = queued.iter().map(|job| &job.id).collect();
        self.memory.fits_since.retain(|job, _| ids.contains(job));
        self.memory.curves.clear();
        let of = |job: &JobRecord| Queued {
            key: leaves_memory(job).then(|| JobSizes::key_of(job)),
            class: class_name(job),
        };
        self.memory.queued = queued.iter().map(|job| (job.id.clone(), of(job))).collect();
        let mut tables = BTreeMap::new();
        for host in hosts {
            if let Some(limit) = self.room.memory_capacity(host) {
                tables.insert((*host).clone(), self.memory_table_of(host, limit, now));
            }
        }
        self.memory.tables = tables;
    }

    /// The table of `host`, which can book `limit`, as the walk at `now` finds it.
    fn memory_table_of(&mut self, host: &HostId, limit: u64, now: FarmTime) -> MemoryTable {
        let here = self.book.live.values().filter(|held| held.host == *host);
        let jobs: Vec<(Option<Key>, u64, u64)> = here
            .map(|held| {
                let age = held
                    .started_ms
                    .map_or(0, |at| now.as_millis().saturating_sub(at));
                (held.memory_key.clone(), held.memory_bytes, age)
            })
            .collect();
        let mut held = vec![0; CURVE_SLOTS];
        for (key, flat, age) in jobs {
            let curve = key.and_then(|key| self.memory_curve(&key, flat, now));
            add(&mut held, &charges_of(curve.as_ref(), age, flat));
        }
        let fresh = self
            .memory
            .readings
            .get(host)
            .filter(|(at, _)| now.as_millis().saturating_sub(at.as_millis()) <= FRESH_MS);
        let residual = fresh.map_or(0, |(_, used)| used.saturating_sub(held[0]));
        MemoryTable {
            held,
            residual,
            limit,
        }
    }

    /// [`Engine::try_job`] for `walker`, with the memory shadow's judgement of the round before
    /// it and the placement's verdict after it. The round places exactly as it would alone.
    pub(super) fn shadowed_try(
        &mut self,
        walker: &Walker,
        (room, capable): (&BTreeSet<HostId>, &BTreeSet<&HostId>),
        now: FarmTime,
        settled: &mut bool,
    ) -> Option<HostId> {
        // A walk with no host held to a memory capacity judges nothing: the queue's rounds cost
        // the shadow nothing then.
        let judging = self.shadows_memory() && !self.memory.tables.is_empty();
        let judged = judging.then(|| self.judge_memory(walker, (room, capable), now));
        let host = self.try_job(walker, (room, capable), now, settled);
        if let Some(judged) = judged {
            self.memory_decided(walker, judged, host.as_ref(), now);
        }
        host
    }

    /// How `walker` would fare by its memory curve on the hosts of `room` in `capable` where no
    /// allocation holds a host port it declares.
    fn judge_memory(
        &mut self,
        walker: &Walker,
        (room, capable): (&BTreeSet<HostId>, &BTreeSet<&HostId>),
        now: FarmTime,
    ) -> Judged {
        let flat = walker.charge.memory_bytes;
        let key = self.memory.queued.get(&walker.job);
        let key = key.and_then(|queued| queued.key.clone());
        let curve = key.and_then(|key| self.memory_curve(&key, flat, now));
        let booking = booking_of(curve.as_ref(), flat);
        let hosts = room.iter().filter(|host| capable.contains(host));
        let hosts = hosts.filter(|host| !self.port_held_on(host, &walker.host_ports));
        let refused_fits = hosts
            .filter_map(|host| self.memory.tables.get(host).map(|table| (host, table)))
            .filter(|(host, _)| self.room.fits_without_memory(host, walker.booked))
            .filter(|(host, _)| self.room.memory_refusal(host, walker.booked).is_some())
            .any(|(_, table)| table.first_over(&booking).is_none());
        Judged {
            curve,
            flat,
            booking,
            refused_fits,
        }
    }

    /// Records the round of `walker` that `judged` judged and that placed it on `host` (`None`:
    /// nowhere) at `now`: a job placed nowhere while its curve fitted a host the flat memory
    /// booking refused starts its time; a placement on a host with a table is counted, logged
    /// when it differs, and projected on that host from age 0.
    fn memory_decided(
        &mut self,
        walker: &Walker,
        judged: Judged,
        host: Option<&HostId>,
        now: FarmTime,
    ) {
        let Some(host) = host else {
            if judged.refused_fits {
                self.memory
                    .fits_since
                    .entry(walker.job.clone())
                    .or_insert(now);
            }
            return;
        };
        let since = self.memory.fits_since.remove(&walker.job);
        let Some(table) = self.memory.tables.get_mut(host) else {
            return;
        };
        let over = table.first_over(&judged.booking);
        add(
            &mut table.held,
            &charges_of(judged.curve.as_ref(), 0, judged.flat),
        );
        let class = self.memory.queued.get(&walker.job);
        let class = class.map_or("", |queued| queued.class.as_str());
        let view = &mut self.memory.view;
        match (over, since) {
            (Some(slot), _) => {
                view.later = view.later.saturating_add(1);
                info!(
                    target: "js_sched_placer::sizes::profile",
                    job = %walker.job,
                    %class,
                    %host,
                    slot,
                    "placement curves: a job's memory curve would not have placed it on its host"
                );
            }
            (None, Some(since)) => {
                let sooner_ms = now.as_millis().saturating_sub(since.as_millis());
                view.sooner = view.sooner.saturating_add(1);
                view.sooner_ms = view.sooner_ms.saturating_add(sooner_ms);
                info!(
                    target: "js_sched_placer::sizes::profile",
                    job = %walker.job,
                    %class,
                    %host,
                    sooner_ms,
                    "placement curves: a job's memory curve would have placed it sooner"
                );
            }
            (None, None) => view.same = view.same.saturating_add(1),
        }
    }

    /// Ends the memory shadow's walk at `now`: keeps each host's projection, for the readings
    /// that follow, and what its jobs are projected to hold now, for `/metrics`. A host the walk
    /// has no table of leaves the view and its readings.
    pub(super) fn end_memory_walk(&mut self, now: FarmTime) {
        let tables = &self.memory.tables;
        self.memory
            .view
            .hosts
            .retain(|host, _| tables.contains_key(host));
        self.memory
            .readings
            .retain(|host, _| tables.contains_key(host));
        let tables = self.memory.tables.iter();
        let projections = tables.map(|(host, table)| (host.clone(), (now, table.projected())));
        self.memory.projections = projections.collect();
        for (host, table) in &self.memory.tables {
            let seen = self.memory.view.hosts.entry(host.clone()).or_default();
            seen.projected_bytes = table.held[0];
        }
    }

    /// Takes `load`, a reading of `host` heard at `now`, into the memory shadow: it is kept for
    /// the next walk's residual and compared with the projection the last walk made of `now`,
    /// and a reading above it is logged. Nothing under [`Curves::Off`], or without a reading.
    pub(super) fn memory_heard(&mut self, host: &HostId, load: Option<&NodeLoad>, now: FarmTime) {
        let Some(load) = load.filter(|_| self.shadows_memory()) else {
            return;
        };
        let used = load.memory_working_set_bytes;
        self.memory.readings.insert(host.clone(), (now, used));
        let seen = self.memory.view.hosts.entry(host.clone()).or_default();
        seen.measured_bytes = Some(used);
        let Some((made, projected)) = self.memory.projections.get(host) else {
            return;
        };
        let ahead_ms = now.as_millis().saturating_sub(made.as_millis());
        let expected = projected[age_slot(ahead_ms)];
        seen.checks = seen.checks.saturating_add(1);
        if used > expected {
            seen.too_low = seen.too_low.saturating_add(1);
            seen.short_bytes = seen.short_bytes.saturating_add(used - expected);
            info!(
                target: "js_sched_placer::sizes::profile",
                %host,
                projected_mib = expected / MIB,
                measured_mib = used / MIB,
                ahead_ms,
                "placement curves: a host held more memory than its jobs' memory curves projected"
            );
        }
    }

    /// What `/metrics` reads of the memory shadow; `None` under [`Curves::Off`].
    pub(super) fn memory_shadow_view_of(&self) -> Option<MemoryShadowView> {
        self.shadows_memory().then(|| self.memory.view.clone())
    }
}

#[cfg(test)]
#[path = "memory_shadow_tests.rs"]
mod tests;
