//! The four invariants of the walk over a pool's machine providers (design 5.6, steps 4 and
//! 5): [`Invariant::ProviderOrder`], [`Invariant::NoSecondLaunchForOneAcquire`],
//! [`Invariant::QuotaPassOverHonoured`] and [`Invariant::FreeHostBeatsAcquire`], all after
//! every step.
//!
//! Each reads the clouds of [`Observations::clouds`]: what the recording wrapper saw at
//! each acquire ([`CloudFacts::seen`]), the calls it recorded ([`CloudFacts::calls`]) and
//! the machines the cell read of each cloud ([`CloudFacts::machines`]);
//! [`Invariant::FreeHostBeatsAcquire`] also reads the job tables
//! ([`Observations::allocation_reads`]). A step reads only what was recorded since the last
//! one, each once, except the machines, which are few and read whole. A cell without an
//! elastic pool has nothing to check.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::RefusalCode;
use js_jobs::AllocationId;
use js_sim::text::time;
use js_types::HostId;
use js_types::sched::FarmTime;

use super::{Found, Invariant};
use crate::elastic::{AcquireAnswer, AcquireSeen, CloudEvent, CloudFacts};
use crate::observe::Observations;
use crate::text;

/// What the walk's invariants have read.
#[derive(Clone, Debug, Default)]
pub(super) struct Indices {
    /// What was read of each cloud, in the order of [`Observations::clouds`].
    clouds: Vec<OneCloud>,
    /// Each acquire id a cloud answered with no answer or a deferral: that cloud's name,
    /// and when.
    unanswered: BTreeMap<String, (String, FarmTime)>,
}

/// What the walk's invariants have read of one cloud.
#[derive(Clone, Debug, Default)]
struct OneCloud {
    /// How many of its calls were read.
    calls: usize,
    /// How many of what its wrapper saw were read for [`Invariant::ProviderOrder`] and
    /// [`Invariant::NoSecondLaunchForOneAcquire`].
    seen: usize,
    /// How many of what its wrapper saw were judged for [`Invariant::FreeHostBeatsAcquire`]:
    /// those the job tables have been read past.
    judged: usize,
    /// Each offer and zone it refused for `quota`: when, and the end of the backoff.
    passed_over: BTreeMap<(String, Option<String>), (FarmTime, FarmTime)>,
}

impl Indices {
    /// Checks what each cloud recorded since the last step.
    pub(super) fn step(&mut self, observations: &Observations, found: &mut Found) {
        let clouds = &observations.clouds;
        self.clouds.resize_with(clouds.len(), OneCloud::default);
        let mut fresh: Vec<(&CloudFacts, &AcquireSeen)> = Vec::new();
        for (read, cloud) in self.clouds.iter_mut().zip(clouds) {
            read.quota(cloud, found);
            fresh.extend(cloud.seen.iter().skip(read.seen).map(|seen| (cloud, seen)));
            read.seen = cloud.seen.len();
        }
        fresh.sort_by_key(|(_, seen)| seen.at);
        for (cloud, seen) in fresh {
            order(cloud, seen, found);
            self.presented(cloud, seen, found);
        }
        one_machine_per_acquire(clouds, found);
        let tables = observations.tables_at;
        let mut booked = None;
        for (read, cloud) in self.clouds.iter_mut().zip(clouds) {
            let judged = cloud.seen.iter().skip(read.judged);
            let due = judged.take_while(|seen| tables.is_some_and(|read_at| seen.at <= read_at));
            for seen in due {
                let booked = booked.get_or_insert_with(|| Booked::of(observations));
                free_host(cloud, seen, booked, found);
                read.judged += 1;
            }
        }
    }

    /// [`Invariant::NoSecondLaunchForOneAcquire`] (b) for one acquire, `seen`, of `cloud`.
    fn presented(&mut self, cloud: &CloudFacts, seen: &AcquireSeen, found: &mut Found) {
        let earlier = self.unanswered.get(&seen.acquire);
        if let Some((first, at)) = earlier.filter(|(first, _)| *first != cloud.pool) {
            let detail = format!(
                "the acquire id {}, which provider {first} answered with no answer or a \
                 deferral at {}, was presented to provider {} at {}",
                seen.acquire,
                time(*at),
                cloud.pool,
                time(seen.at)
            );
            found.flag(Invariant::NoSecondLaunchForOneAcquire, detail);
        }
        let open = matches!(
            seen.answer,
            AcquireAnswer::Unknown | AcquireAnswer::Deferred(_)
        );
        if open {
            let first = (cloud.pool.clone(), seen.at);
            self.unanswered.entry(seen.acquire.clone()).or_insert(first);
        }
    }
}

impl OneCloud {
    /// [`Invariant::QuotaPassOverHonoured`] over the calls `cloud` recorded since the last
    /// step.
    fn quota(&mut self, cloud: &CloudFacts, found: &mut Found) {
        for call in cloud.calls.iter().skip(self.calls) {
            let CloudEvent::Acquire {
                offer,
                zone,
                answer,
                ..
            } = &call.event
            else {
                continue;
            };
            let key = (offer.clone(), zone.clone());
            let inside =
                |(then, until): &&(FarmTime, FarmTime)| (*then < call.at) & (call.at < *until);
            if let Some((then, _)) = self.passed_over.get(&key).filter(inside) {
                let detail = format!(
                    "{}, inside the {}s quota backoff of provider {} after its quota refusal \
                     at {}",
                    acquire_of(offer, zone.as_deref(), call.at),
                    cloud.quota_backoff.as_secs(),
                    cloud.pool,
                    time(*then)
                );
                found.flag(Invariant::QuotaPassOverHonoured, detail);
            }
            if *answer == AcquireAnswer::Refused(RefusalCode::Quota) {
                let until = call.at.saturating_add(cloud.quota_backoff);
                self.passed_over.insert(key, (call.at, until));
            }
        }
        self.calls = cloud.calls.len();
    }
}

/// How a violation names an acquire of `offer` in `zone` at `at`.
fn acquire_of(offer: &str, zone: Option<&str>, at: FarmTime) -> String {
    format!(
        "an acquire of offer {offer} in zone {} at {}",
        zone.unwrap_or("-"),
        time(at)
    )
}

/// [`Invariant::ProviderOrder`] for one acquire, `seen`, of `cloud`.
fn order(cloud: &CloudFacts, seen: &AcquireSeen, found: &mut Found) {
    for earlier in &seen.earlier {
        let room = earlier.held < earlier.max;
        if let Some((offer, zone)) = earlier.open.first().filter(|_| room) {
            let detail = format!(
                "{} from provider {}, while provider {} before it held {} of its {} machines \
                 and its offer {offer} in zone {} had no definite refusal",
                acquire_of(&seen.offer, seen.zone.as_deref(), seen.at),
                cloud.pool,
                earlier.provider,
                earlier.held,
                earlier.max,
                zone.as_deref().unwrap_or("-"),
            );
            found.flag(Invariant::ProviderOrder, detail);
        }
    }
}

/// [`Invariant::NoSecondLaunchForOneAcquire`] (a): each acquire id with more than one
/// machine over `clouds`.
fn one_machine_per_acquire(clouds: &[CloudFacts], found: &mut Found) {
    let mut by_acquire: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for cloud in clouds {
        for machine in cloud.machines.values() {
            if let Some(acquire) = &machine.acquire {
                let name = format!("m{:03} of provider {}", machine.number, cloud.pool);
                by_acquire.entry(acquire.as_str()).or_default().push(name);
            }
        }
    }
    for (acquire, machines) in by_acquire.into_iter().filter(|(_, m)| m.len() > 1) {
        let detail = format!(
            "the acquire id {acquire} has {} machines: {}",
            machines.len(),
            machines.join(", ")
        );
        found.flag(Invariant::NoSecondLaunchForOneAcquire, detail);
    }
}

/// The allocations of the job tables as the cell read them: each one's host, when it was
/// committed, and when it was freed (if a read showed it freed).
struct Booked(BTreeMap<AllocationId, (HostId, FarmTime, Option<FarmTime>)>);

impl Booked {
    /// The allocations of `observations`' reads of the job tables.
    fn of(observations: &Observations) -> Self {
        let mut booked: BTreeMap<AllocationId, (HostId, FarmTime, Option<FarmTime>)> =
            BTreeMap::new();
        for read in &observations.allocation_reads {
            let row = &read.row;
            let entry = booked.entry(row.allocation);
            let kept = entry.or_insert_with(|| (row.host.clone(), row.created_at, None));
            kept.2 = kept.2.or(row.freed_at);
        }
        Self(booked)
    }

    /// The hosts that held a job at `at`: those of an allocation committed by then and not
    /// freed before it.
    fn busy(&self, at: FarmTime) -> BTreeSet<&HostId> {
        let held = self.0.values().filter(|(_, created, freed)| {
            let ended = freed.is_some_and(|freed| freed < at);
            (*created <= at) & !ended
        });
        held.map(|(host, _, _)| host).collect()
    }
}

/// [`Invariant::FreeHostBeatsAcquire`] for one acquire, `seen`, of `cloud`, over the
/// allocations `booked`.
fn free_host(cloud: &CloudFacts, seen: &AcquireSeen, booked: &Booked, found: &mut Found) {
    let busy = booked.busy(seen.at);
    let free: Vec<&HostId> = seen
        .open_hosts
        .iter()
        .filter(|host| !busy.contains(host))
        .collect();
    if let Some(first) = free.first() {
        let detail = format!(
            "{} from provider {}, while {} registered hosts were open and held no job, {} \
             first",
            acquire_of(&seen.offer, seen.zone.as_deref(), seen.at),
            cloud.pool,
            free.len(),
            text::host(first)
        );
        found.flag(Invariant::FreeHostBeatsAcquire, detail);
    }
}
