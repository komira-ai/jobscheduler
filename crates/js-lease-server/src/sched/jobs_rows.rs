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
use std::sync::{Arc, PoisonError};

use js_jobs::{Capacity, HOST_JOB_TAINT, HostTaint, JobId, NodeRow, Written};
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

/// The store's rows a pass offers hosts by, with where the store stood when they were read.
#[derive(Debug)]
pub(crate) struct RowsRead {
    /// Where the job store's writes and the store's head stood before the reads.
    written: Written,
    /// The live `node/` rows, by host.
    live: BTreeMap<HostId, NodeRow>,
    /// The shape the engine places each live row's host as ([`engine_shape`]), for the rows
    /// that have one.
    engines: BTreeMap<HostId, Shape>,
    /// The `taint/` rows, by host.
    taints: BTreeMap<HostId, HostTaint>,
    /// The hosts a `hostmark/` row marks.
    marks: BTreeSet<HostId>,
    /// The bindings the store holds (none in file mode).
    stored: BTreeMap<HostId, Binding>,
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
    /// For a host known by its row, the shape the engine places it as ([`engine_shape`]).
    engine: Option<&'a Shape>,
}

/// The shape the engine places the host of `row` as: the pool's booking with the claim's
/// accelerator facts, as the registry tells the engine at a hello. `None` for a row with no
/// booked shape, or of a pool this server does not serve.
fn engine_shape(agents: &Agents, row: &NodeRow) -> Option<Shape> {
    let record = &row.record;
    let shape = record.shape.as_ref()?;
    let claim = record.claim.as_ref();
    agents
        .registry
        .placement_shape_in(&record.pool, shape, claim)
}

/// The row's view of a host, placed by the engine as `engine` ([`engine_shape`]): open when the
/// host serves and its last listing was clean. `None` for a row the engine places no host of.
fn seen_by_row<'a>(row: &'a NodeRow, engine: Option<&'a Shape>) -> Option<Seen<'a>> {
    let record = &row.record;
    let (shape, engine) = record.shape.as_ref().zip(engine)?;
    let state = &record.state;
    Some(Seen {
        shape,
        claim: record.claim.as_ref(),
        open: (state.phase == Phase::Serving) & state.last_listing_clean,
        engine: Some(engine),
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
    /// it ([`SchedPlacer::host_seen`]). The store's rows are read afresh, and kept for
    /// [`Agents::offered_after_beat`].
    ///
    /// # Errors
    /// A read of the store failed, said in words: the pass places nothing.
    pub(super) fn offered(
        &self,
        jobs: &ServerJobs,
        placer: &SchedPlacer,
        now: Timestamp,
    ) -> Result<Offered, String> {
        let read = self.read_rows(jobs)?;
        Ok(self.offered_from(&read, placer, now))
    }

    /// [`Agents::offered`] for a heartbeat's pass: the rows the last read kept are used again
    /// when nothing was committed to the store since they were read (its head and this job
    /// store's writes stand where they stood, [`js_jobs::JobStore::written`]), so a beat that
    /// changes nothing reads the store's head and no table. A row that ages out of the grace
    /// with no commit in between leaves at the next read: the tick's pass reads afresh.
    ///
    /// # Errors
    /// As [`Agents::offered`].
    pub(super) fn offered_after_beat(
        &self,
        jobs: &ServerJobs,
        placer: &SchedPlacer,
        now: Timestamp,
    ) -> Result<Offered, String> {
        let written = jobs.written().map_err(|error| error.to_string())?;
        let kept = self
            .rows_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let read = match kept.filter(|read| read.written == written) {
            Some(read) => read,
            None => self.read_rows(jobs)?,
        };
        Ok(self.offered_from(&read, placer, now))
    }

    /// The rows a pass offers by, read now, and kept for the next heartbeat's pass.
    fn read_rows(&self, jobs: &ServerJobs) -> Result<Arc<RowsRead>, String> {
        let written = jobs.written().map_err(|error| error.to_string())?;
        // The rows the last read decoded are taken again at the versions they had then.
        let kept = self
            .rows_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let known = kept.as_ref().map(|read| &read.live);
        let grace = self.registry.longest_grace();
        let live = jobs.live_nodes_reusing(grace, known.unwrap_or(&BTreeMap::new()));
        let live = live.map_err(|error| error.to_string())?;
        let live: BTreeMap<HostId, NodeRow> = live
            .into_iter()
            .map(|row| (row.record.host.clone(), row))
            .collect();
        let engines = live
            .iter()
            .filter_map(|(host, row)| Some((host.clone(), engine_shape(self, row)?)))
            .collect();
        let taints = jobs.host_taints().map_err(|error| error.to_string())?;
        let marks = jobs.host_marks().map_err(|error| error.to_string())?;
        let stored = self
            .store
            .scan_stored()
            .map_err(|error| error.to_string())?;
        let read = Arc::new(RowsRead {
            written,
            live,
            engines,
            taints,
            marks: marks.into_keys().collect(),
            stored: stored.unwrap_or_default(),
        });
        *self
            .rows_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&read));
        Ok(read)
    }

    /// [`Agents::offered`] over the rows `read`.
    fn offered_from(&self, read: &RowsRead, placer: &SchedPlacer, now: Timestamp) -> Offered {
        let RowsRead {
            live,
            engines,
            taints,
            marks,
            stored,
            ..
        } = read;
        // The registry's bindings are the newest of the hosts it holds.
        let held: BTreeMap<HostId, Binding> = self.registry.bindings();
        let binding_of = |host: &HostId| held.get(host).or_else(|| stored.get(host));
        let views: BTreeMap<HostId, HostView> = self
            .registry
            .hosts()
            .into_iter()
            .map(|view| (view.id.clone(), view))
            .collect();
        let mut offered = Offered::default();
        for (host, taint) in taints {
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
            // A host is offered because it has a live row or the registry holds it live, so
            // one with no row has a view.
            let seen = match row {
                Some(row) => seen_by_row(row, engines.get(host)),
                None => view.map(|view| Seen {
                    shape: &view.shape,
                    claim: view.claim.as_ref(),
                    open: true,
                    engine: None,
                }),
            };
            let Some(seen) = seen else {
                continue;
            };
            let tainted = binding_of(host).is_some_and(|bound| bound.taint().is_some());
            let closed = taints.contains_key(host) | marks.contains(host) | tainted;
            let here = view.is_none_or(|view| open_here(view, now, row.is_some()));
            if !(seen.open & !closed & here & self.listing_clean(host)) {
                continue;
            }
            // A host of a provider the Default platform does not list is no candidate here.
            let Some(ranked) = rank(&self.providers, binding_of(host)) else {
                continue;
            };
            // The engine was told of a host this registry holds live at its hello here.
            let held_live = view.is_some_and(|view| view.state == HostState::Live);
            if let Some(engine) = seen.engine.filter(|_| !held_live) {
                placer.host_seen(host, engine);
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
        offered
    }
}
