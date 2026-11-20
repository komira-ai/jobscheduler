//! The hosts a pass over the queued jobs offers ([`Agents::offered`]), read from the job store
//! before each pass (design `multi-instance.md`, G7): every host whose `node/` row is live (heard
//! within the longest grace of the pools by the store's clock), whichever server took its hello,
//! and every host this server's registry holds live. A host is offered when its row (or, for a
//! host held here without one, the registry's record) says it serves, its last listing was
//! clean, and no row closes it: no `taint/<host>` row, no binding that holds a taint, no
//! `hostmark/<host>` row. A host this server's registry also holds is closed too by what the
//! registry holds of it (a taint, a scale-down mark, a release under way, a cordon in force, an
//! unclean listing). The store's cordons are dropped by the placer itself.
//!
//! These reads choose what the pass tries; the placement's own transaction decides on the rows
//! it reads ([`js_jobs::JobStore::place_claimed`]), so a host closed after the reads is refused
//! there.
//!
//! A `taint/<host>` row of a host this server's registry holds, which another server's pass put
//! there, is put on the registry's record of the host when the record holds no taint yet, and
//! returned with the pass's own taints, so that its binding is written and the row deleted
//! ([`Agents::keep_taints`]) as for a host job this server placed.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::{Phase, Taint};
use js_jobs::{Capacity, HOST_JOB_TAINT, JobId, NodeRow};
use js_registry::{Binding, HostState, HostView};
use js_sched::ceiling::ceiling_of;
use js_sched_placer::place::SchedPlacer;
use js_types::{HostId, Shape, Timestamp};

use super::engine::jobs::{OpenHost, open_to_jobs};
use super::rank::rank;
use crate::admit::reports_host_jobs;
use crate::agents::Agents;
use crate::config::CpuSlack;
use crate::job_store::ServerJobs;

/// What a pass offers: the open hosts, the hosts heard from (whose marks of whole-host jobs
/// stand), and the taints of other servers' placements this server's registry took on.
#[derive(Debug, Default)]
pub(super) struct Offered {
    /// The hosts that may take a job now, with what each offers.
    pub(super) open: BTreeMap<HostId, OpenHost>,
    /// Every host whose row is live, and every host the registry holds live with a hello.
    pub(super) hello: BTreeSet<HostId>,
    /// The `taint/<host>` rows of hosts this registry holds that it took on, each with the job
    /// that placed there.
    pub(super) adopted: Vec<(HostId, JobId)>,
}

/// What a host of `shape` can book: the shape the pool books for it, with the CPU slack the
/// placement engine allows on its cores, which is the most the engine places on it. The
/// capacity is the bound a placement is held to in its own transaction against the
/// allocations the store holds, so it must not be tighter than what the engine allows.
pub(super) fn capacity_of(shape: &Shape, slack: CpuSlack) -> Capacity {
    let resources = shape.resources();
    let cpu = u128::from(resources.cpu_millis()) * (1000 + u128::from(slack.permille())) / 1000;
    Capacity {
        cpu_millis: u64::try_from(cpu).unwrap_or(u64::MAX),
        memory_bytes: resources.memory_bytes(),
        disk_bytes: resources.disk_bytes(),
        accelerators: resources.gpus(),
    }
}

/// What the pass knows of one host before it is offered: its booked shape, its claim, and
/// whether it serves and its last listing was clean.
struct Seen<'a> {
    shape: &'a Shape,
    claim: Option<&'a Shape>,
    open: bool,
}

/// The row's view of a host: open when the host serves, its last listing was clean and its
/// pool is one this server serves. `None` for a row with no booked shape.
fn seen_by_row<'a>(agents: &Agents, row: &'a NodeRow) -> Option<Seen<'a>> {
    let record = &row.record;
    let served = agents.registry.pool_cap(&record.pool).is_some();
    let state = &record.state;
    Some(Seen {
        shape: record.shape.as_ref()?,
        claim: record.claim.as_ref(),
        open: served & (state.phase == Phase::Serving) & state.last_listing_clean,
    })
}

/// Whether the registry's record of a host, as `view` shows it at `now`, leaves it open: no
/// taint, no scale-down mark, no release under way, no cordon in force; and, for a host that
/// has no live row (`live` false), the registry holds it live and serving
/// ([`open_to_jobs`]).
fn open_here(view: &HostView, now: Timestamp, live: bool) -> bool {
    if !live {
        return open_to_jobs(view, now);
    }
    let cordoned = view.cordon_until.is_some_and(|until| until > now);
    view.taint.is_none() & view.mark.is_none() & !view.releasing & !cordoned
}

impl Agents {
    /// The hosts the next pass offers, read from the job store and this server's registry at
    /// `now` (the module says how), with each offered host the placer does not know yet told to
    /// it ([`SchedPlacer::host_seen`]).
    ///
    /// # Errors
    /// A read of the store failed, said in words: the pass places nothing.
    pub(super) fn offered(
        &self,
        jobs: &ServerJobs,
        placer: &SchedPlacer,
        now: Timestamp,
    ) -> Result<Offered, String> {
        let live = jobs.live_nodes(self.registry.longest_grace());
        let live = live.map_err(|error| error.to_string())?;
        let live: BTreeMap<HostId, NodeRow> = live
            .into_iter()
            .map(|row| (row.record.host.clone(), row))
            .collect();
        let taints = jobs.host_taints().map_err(|error| error.to_string())?;
        let marks = jobs.host_marks().map_err(|error| error.to_string())?;
        let stored = self
            .store
            .scan_stored()
            .map_err(|error| error.to_string())?;
        // The registry's bindings are the newest of the hosts it holds.
        let mut bindings: BTreeMap<HostId, Binding> = stored.unwrap_or_default();
        bindings.extend(self.registry.bindings());
        let views: BTreeMap<HostId, HostView> = self
            .registry
            .hosts()
            .into_iter()
            .map(|view| (view.id.clone(), view))
            .collect();
        let mut offered = Offered::default();
        for (host, taint) in &taints {
            let taint_of = |job: &JobId| Taint {
                reason: HOST_JOB_TAINT.to_owned(),
                run_id: Some(job.to_string()),
                check: None,
            };
            let held = views.contains_key(host);
            if held && self.registry.taint_host(host, taint_of(&taint.job)) {
                offered.adopted.push((host.clone(), taint.job.clone()));
            }
        }
        let held_live = views
            .values()
            .filter(|view| (view.state == HostState::Live) & view.claim.is_some())
            .map(|view| view.id.clone());
        offered.hello = live.keys().cloned().chain(held_live).collect();
        for host in &offered.hello {
            let view = views.get(host);
            let row = live.get(host);
            let seen = match (row, view) {
                (Some(row), _) => seen_by_row(self, row),
                (None, Some(view)) => Some(Seen {
                    shape: &view.shape,
                    claim: view.claim.as_ref(),
                    open: true,
                }),
                (None, None) => None,
            };
            let Some(seen) = seen else {
                continue;
            };
            let tainted = bindings
                .get(host)
                .is_some_and(|bound| bound.taint().is_some());
            let closed = taints.contains_key(host) | marks.contains_key(host) | tainted;
            let here = view.is_none_or(|view| open_here(view, now, row.is_some()));
            if !(seen.open & !closed & here & self.listing_clean(host)) {
                continue;
            }
            // A host of a provider the Default platform does not list is no candidate here.
            let Some(ranked) = rank(&self.providers, bindings.get(host)) else {
                continue;
            };
            if row.is_some() {
                placer.host_seen(host, seen.shape);
            }
            let capabilities = seen.claim.map(Shape::capabilities);
            let offer = OpenHost {
                ceiling: capabilities.map(ceiling_of).unwrap_or_default(),
                host_jobs: capabilities.is_some_and(reports_host_jobs),
                rank: Some(ranked),
                capacity: Some(capacity_of(seen.shape, self.cpu_slack)),
            };
            offered.open.insert(host.clone(), offer);
        }
        Ok(offered)
    }
}
