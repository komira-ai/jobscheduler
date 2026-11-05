//! The fleet calls: `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools`, `ListProviders`
//! and `GetUsage` (design section 3.6). An operator reads the hosts with the resources
//! booked on each, the pools, the machine providers and the ended bookings; the one change
//! is a host's cordon. A client never sees an allocation as a job concept: what is booked on
//! a host is a read-only [`wire::HostBooking`] in its listing.
//!
//! The group's state is [`Ctx`]. Until [`Ctx::served`] gives it the server's handles every
//! call answers `UNIMPLEMENTED`. It reaches the server through three narrow traits, so the
//! service holds no lease table or registry of its own:
//!
//! - [`Hosts`], over the server's shared state ([`Shared`]): the listing that
//!   `GET /v1/hosts` writes ([`hosts::listing`], the table's lock taken and dropped before the
//!   registry's), and the registry's cordon of a host, saved before it is acknowledged.
//! - [`Pools`], over the status of the pools ([`PoolStatus`]): what `GET /v1/pools` writes,
//!   mapped here field by field, so nothing but the fields named below leaves the server.
//! - [`History`], over the usage sink ([`SinkHistory`]): the ended bookings under
//!   `usage/ended/`. It is absent when the server runs with `--usage-off`, and `GetUsage`
//!   then answers an empty page.
//!
//! The booked resources of a host are its live (booked or releasing) allocations in the job
//! tables, in the order they were booked, then the leases still held on it through the lease
//! routes, each listed under its lease id's text. The job tables are read after both locks
//! are dropped, on the pool for blocking calls.
//!
//! The reads change nothing: they take no write lock, write no row, bump no counter and move
//! no state on. A cordon follows the rules of the host's own route ([`cordon`]): 300 s when
//! no length is given, cut to 3600 s, zero refused; a repeat renews and never shortens; the
//! answer waits for the bindings to be written and is `UNAVAILABLE` `cordon-unsaved` when
//! they cannot be, the cordon holding on this server all the same.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{AllocationRecord, Booked, JobRecord};
use js_proto::v1 as wire;
use js_proto::v1::{
    CordonHostRequest, CordonHostResponse, GetUsageRequest, GetUsageResponse, ListHostsRequest,
    ListHostsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, UncordonHostRequest, UncordonHostResponse,
};
use js_provider::Held;
use js_registry::{HostMark, HostState};
use js_types::HostId;
use js_usage::{
    AllocationId, DrainOutcome, EndedKey, EndedPage, EndedQuery, EndedReason, EndedRow,
};
use tonic::{Request, Response, Status};

use super::JobApi;
use crate::agents::Agents;
use crate::api::Shared;
use crate::checks::State;
use crate::cordon::{self, CORDON_UNSAVED, INVALID_CORDON, UNKNOWN_HOST};
use crate::hosts::{self, HostEntry, HostList, LeasesOn, MemoryView, ShapeView, Source};
use crate::status::Status as PoolStatus;
use crate::status::{CheckView, MachineView, NamedCheckView, PoolView, PoolsView};
use crate::usage::{UsageFault, UsageSink};
use crate::util::blocking;

#[cfg(test)]
#[path = "fleet_tests.rs"]
mod tests;

/// The page of `GetUsage` asked for with no size.
const DEFAULT_PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(99);

/// The largest page of `GetUsage`; a larger one asked for is cut to it.
const MAX_PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(999);

/// The answer to a `page_token` that no answer gave.
const INVALID_PAGE_TOKEN: &str = "invalid-page-token";

/// The answer when the job tables cannot be read.
const STORE_UNAVAILABLE: &str = "store-unavailable";

/// The answer when the usage history cannot be read.
const USAGE_UNAVAILABLE: &str = "usage-unavailable";

/// The name of the check of a provider's credentials, as `GET /v1/pools` names its field.
const CREDENTIALS: &str = "credentials";

/// The hosts as the server holds them.
#[tonic::async_trait]
pub(crate) trait Hosts: Send + Sync {
    /// Every host, as `GET /v1/hosts` lists it, and the leases booked on each.
    async fn hosts(&self) -> (HostList, LeasesOn);

    /// Cordons `host` for `length` from now, or renews its cordon (never shortening it), and
    /// saves the bindings; whether this placed the cordon (the host was not cordoned).
    async fn cordon(&self, host: &HostId, length: Duration) -> Result<bool, Refused>;

    /// Lifts the cordon of `host` and saves the bindings; whether a cordon was in force.
    async fn uncordon(&self, host: &HostId) -> Result<bool, Refused>;
}

/// Why a cordon or an uncordon was not acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The registry holds no such host: it never said hello, was forgotten, or is static.
    UnknownHost,
    /// The change holds on this server, but the bindings that keep it over a restart could
    /// not be written.
    Unsaved,
}

/// The status of the pools, as `GET /v1/pools` reads it.
pub(crate) trait Pools: Send + Sync {
    /// The status now.
    fn pools(&self) -> PoolsView;
}

/// The record of the ended bookings.
#[tonic::async_trait]
pub(crate) trait History: Send + Sync {
    /// One page of the records `query` asks for.
    async fn page(&self, query: EndedQuery) -> Result<EndedPage, UsageFault>;
}

/// A shared handle that is equal only to a clone of itself.
struct Handle<T: ?Sized>(Arc<T>);

impl<T: ?Sized> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: ?Sized> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T: ?Sized> Eq for Handle<T> {}

impl<T: ?Sized> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handle")
    }
}

/// What the group is served over.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    hosts: Handle<dyn Hosts>,
    pools: Handle<dyn Pools>,
    /// `None` with `--usage-off`.
    history: Option<Handle<dyn History>>,
}

/// The state of the fleet group: nothing until it is served ([`Ctx::served`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ctx {
    /// A value only tests set, so that a test can tell a context it gave from the default.
    #[cfg(test)]
    pub(super) mark: u32,
    served: Option<Served>,
}

impl Ctx {
    /// The default context with `mark` set, for a test that tells one context from another.
    #[cfg(test)]
    pub(super) fn marked(mark: u32) -> Ctx {
        Ctx { mark, served: None }
    }

    /// The group served over the server's `hosts` and `pools` and its usage `history`
    /// (`None` when the server runs with `--usage-off`).
    pub(crate) fn served(
        hosts: Arc<dyn Hosts>,
        pools: Arc<dyn Pools>,
        history: Option<Arc<dyn History>>,
    ) -> Ctx {
        Ctx {
            #[cfg(test)]
            mark: 0,
            served: Some(Served {
                hosts: Handle(hosts),
                pools: Handle(pools),
                history: history.map(Handle),
            }),
        }
    }
}

impl JobApi {
    /// The service with `fleet` as the fleet group's context; every other part is kept.
    #[must_use]
    pub fn with_fleet(self, fleet: Ctx) -> JobApi {
        JobApi { fleet, ..self }
    }
}

/// What the group is served over, or `UNIMPLEMENTED` before it is.
fn served(api: &JobApi) -> Result<&Served, Status> {
    let served = api.fleet.served.as_ref();
    served.ok_or_else(|| Status::unimplemented(super::UNIMPLEMENTED))
}

/// `ListHosts`: every host, by id, with the resources booked on it.
pub(super) async fn list_hosts(
    api: &JobApi,
    _request: Request<ListHostsRequest>,
) -> Result<Response<ListHostsResponse>, Status> {
    let served = served(api)?;
    let (list, leases) = served.hosts.0.hosts().await;
    let hosts = described(api, list.hosts, &leases).await?;
    Ok(Response::new(ListHostsResponse { hosts }))
}

/// `CordonHost`: takes a registered host out of placement for a time, or extends its cordon.
pub(super) async fn cordon_host(
    api: &JobApi,
    request: Request<CordonHostRequest>,
) -> Result<Response<CordonHostResponse>, Status> {
    let served = served(api)?;
    let request = request.into_inner();
    let length = cordon::length(request.cordon_secs.map(u64::from));
    let length = length.ok_or_else(|| Status::invalid_argument(INVALID_CORDON))?;
    let host = known(&request.host_id)?;
    let placed = served.hosts.0.cordon(&host, length).await;
    let placed = placed.map_err(refusal)?;
    let host = one_host(api, served, &host).await?;
    Ok(Response::new(CordonHostResponse {
        host: Some(host),
        already_cordoned: !placed,
    }))
}

/// `UncordonHost`: lifts a host's cordon; a host that is not cordoned changes nothing.
pub(super) async fn uncordon_host(
    api: &JobApi,
    request: Request<UncordonHostRequest>,
) -> Result<Response<UncordonHostResponse>, Status> {
    let served = served(api)?;
    let host = known(&request.into_inner().host_id)?;
    let lifted = served.hosts.0.uncordon(&host).await.map_err(refusal)?;
    let host = one_host(api, served, &host).await?;
    Ok(Response::new(UncordonHostResponse {
        host: Some(host),
        lifted,
    }))
}

/// `ListPools`: the elastic pools, by name; none for a server without one.
pub(super) async fn list_pools(
    api: &JobApi,
    _request: Request<ListPoolsRequest>,
) -> Result<Response<ListPoolsResponse>, Status> {
    let view = served(api)?.pools.0.pools();
    let pools = view.pools.iter().map(pool).collect();
    Ok(Response::new(ListPoolsResponse { pools }))
}

/// `ListProviders`: the machine provider of each elastic pool, by the pool's name.
pub(super) async fn list_providers(
    api: &JobApi,
    _request: Request<ListProvidersRequest>,
) -> Result<Response<ListProvidersResponse>, Status> {
    let view = served(api)?.pools.0.pools();
    let providers = view.pools.iter().map(provider).collect();
    Ok(Response::new(ListProvidersResponse { providers }))
}

/// `GetUsage`: the ended bookings, oldest end first, a page at a time.
pub(super) async fn get_usage(
    api: &JobApi,
    request: Request<GetUsageRequest>,
) -> Result<Response<GetUsageResponse>, Status> {
    let served = served(api)?;
    let request = request.into_inner();
    let after = match request.page_token.as_str() {
        "" => None,
        token => Some(EndedKey::parse(token).ok_or_else(invalid_token)?),
    };
    let empty = Ok(Response::new(GetUsageResponse::default()));
    let Some(history) = &served.history else {
        return empty;
    };
    // A filter no record can match (an id no allocation has) matches none.
    let host = filter(&request.host_id, |text| HostId::new(text).ok());
    let id = filter(&request.job_id, |text| AllocationId::parse(text).ok());
    let (Some(host), Some(id)) = (host, id) else {
        return empty;
    };
    let query = EndedQuery {
        after,
        from_ms: request.from_ms.unwrap_or(0),
        before_ms: request.to_ms,
        host,
        id,
        limit: page_size(request.page_size),
    };
    let page = history.0.page(query).await;
    let page = page.map_err(|_| Status::unavailable(USAGE_UNAVAILABLE))?;
    Ok(Response::new(GetUsageResponse {
        records: page.rows.into_iter().map(record).collect(),
        next_page_token: page.next.map(|next| next.text()).unwrap_or_default(),
    }))
}

fn invalid_token() -> Status {
    Status::invalid_argument(INVALID_PAGE_TOKEN)
}

/// The filter that `text` asks for: `Some(None)` when it is empty (every record),
/// `Some(Some(value))` for the value `parse` reads, and `None` when it reads none.
fn filter<T>(text: &str, parse: impl Fn(&str) -> Option<T>) -> Option<Option<T>> {
    match text {
        "" => Some(None),
        text => parse(text).map(Some),
    }
}

/// The page size `asked` gives: [`DEFAULT_PAGE`] for 0, cut to [`MAX_PAGE`].
fn page_size(asked: u32) -> NonZeroUsize {
    let asked = usize::try_from(asked).unwrap_or(usize::MAX);
    NonZeroUsize::new(asked).map_or(DEFAULT_PAGE, |size| size.min(MAX_PAGE))
}

/// The host named `id`, or `NOT_FOUND` `unknown-host` for an id no host can have.
fn known(id: &str) -> Result<HostId, Status> {
    HostId::new(id).map_err(|_| Status::not_found(UNKNOWN_HOST))
}

/// The answer to a refused cordon or uncordon.
fn refusal(refused: Refused) -> Status {
    match refused {
        Refused::UnknownHost => Status::not_found(UNKNOWN_HOST),
        Refused::Unsaved => Status::unavailable(CORDON_UNSAVED),
    }
}

/// The host `id` as `ListHosts` lists it now, or `NOT_FOUND` `unknown-host` when the listing
/// no longer holds it.
async fn one_host(api: &JobApi, served: &Served, id: &HostId) -> Result<wire::Host, Status> {
    let (list, leases) = served.hosts.0.hosts().await;
    let entries = list.hosts.into_iter().filter(|entry| entry.id == *id);
    let mut hosts = described(api, entries.collect(), &leases).await?;
    hosts.pop().ok_or_else(|| Status::not_found(UNKNOWN_HOST))
}

/// The live allocations on a host, each with its job's record when the job still has one.
type Live = Vec<(AllocationRecord, Option<JobRecord>)>;

/// `entries` as the service tells them, each with what is booked on it: its live
/// allocations, read from the job tables on the pool for blocking calls, then `leases`.
async fn described(
    api: &JobApi,
    entries: Vec<HostEntry>,
    leases: &LeasesOn,
) -> Result<Vec<wire::Host>, Status> {
    let ids: Vec<HostId> = entries.iter().map(|entry| entry.id.clone()).collect();
    let jobs = Arc::clone(&api.jobs);
    let read = move || ids.iter().map(|id| jobs.live_on(id)).collect();
    let live: Result<Vec<Live>, _> = blocking(read).await;
    let live = live.map_err(|_| Status::unavailable(STORE_UNAVAILABLE))?;
    let hosts = entries.into_iter().zip(live);
    Ok(hosts
        .map(|(entry, live)| host(entry, live, leases))
        .collect())
}

/// One host as the service tells it, with `live` and its `leases` booked on it.
fn host(entry: HostEntry, mut live: Live, leases: &LeasesOn) -> wire::Host {
    let shape = shape(&entry.shape);
    live.sort_by_key(|(allocation, _)| allocation.created_at);
    let jobs = live.iter();
    let jobs = jobs.map(|(allocation, job)| job_booking(allocation, job.as_ref(), &shape));
    let mut booked: Vec<wire::HostBooking> = jobs.collect();
    let held = leases.get(&entry.id).into_iter().flatten();
    booked.extend(held.map(lease_booking));
    wire::Host {
        id: entry.id.to_string(),
        source: i32::from(source(entry.source)),
        state: i32::from(entry.state.map_or(wire::HostState::Unspecified, state)),
        shape: Some(shape),
        memory: Some(memory(&entry.memory)),
        capabilities: entry.capabilities,
        labels: entry.labels,
        booked,
        mark: i32::from(entry.mark.map_or(wire::HostMark::Unspecified, mark)),
        cordon_expires_at_ms: entry.cordon.map(|cordon| cordon.expires_at_ms),
        taint_reason: entry.taint.map(|taint| taint.reason).unwrap_or_default(),
        agent_version: entry.agent_version.unwrap_or_default(),
    }
}

/// A count of GPUs as the messages carry it: saturated at `u32::MAX`.
fn gpus_of(gpus: u64) -> u32 {
    u32::try_from(gpus).unwrap_or(u32::MAX)
}

/// What `allocation` books, for the job `job` (its declared host ports), on a host whose
/// whole shape is `whole` (what a whole-host allocation books).
fn job_booking(
    allocation: &AllocationRecord,
    job: Option<&JobRecord>,
    whole: &wire::Shape,
) -> wire::HostBooking {
    let ports = job.map(|job| job.spec.ports.iter().map(|port| u32::from(port.host)));
    let (cpu_millis, memory_bytes, disk_bytes, gpus) = match allocation.booked {
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes,
            accelerators,
        } => (cpu_millis, memory_bytes, disk_bytes, gpus_of(accelerators)),
        Booked::WholeHost => (
            whole.cpu_millis,
            whole.memory_bytes,
            whole.disk_bytes.unwrap_or(0),
            whole.gpus,
        ),
    };
    wire::HostBooking {
        job_id: allocation.job.to_string(),
        cpu_millis,
        memory_bytes,
        disk_bytes,
        gpus,
        host_ports: ports.into_iter().flatten().collect(),
    }
}

/// What a lease held through the lease routes books, under the lease id's text.
fn lease_booking(held: &Held) -> wire::HostBooking {
    let resources = &held.resources;
    wire::HostBooking {
        job_id: held.lease.to_string(),
        cpu_millis: resources.cpu_millis(),
        memory_bytes: resources.memory_bytes(),
        disk_bytes: resources.disk_bytes(),
        gpus: gpus_of(resources.gpus()),
        host_ports: Vec::new(),
    }
}

fn source(source: Source) -> wire::HostSource {
    match source {
        Source::Static => wire::HostSource::Static,
        Source::Registered => wire::HostSource::Registered,
    }
}

fn state(state: HostState) -> wire::HostState {
    match state {
        HostState::Live => wire::HostState::Live,
        HostState::Lost => wire::HostState::Lost,
        HostState::Awaiting => wire::HostState::Awaiting,
    }
}

fn mark(mark: HostMark) -> wire::HostMark {
    match mark {
        HostMark::Reserved => wire::HostMark::Reserved,
        HostMark::Held => wire::HostMark::Held,
        HostMark::Draining => wire::HostMark::Draining,
        HostMark::Releasing => wire::HostMark::Releasing,
    }
}

fn shape(shape: &ShapeView) -> wire::Shape {
    wire::Shape {
        cpu_millis: shape.cpu_millis,
        memory_bytes: shape.memory_bytes,
        gpus: gpus_of(shape.gpus),
        gpu_class: shape.gpu_class.clone().unwrap_or_default(),
        disk_bytes: Some(shape.disk_bytes).filter(|bytes| *bytes > 0),
    }
}

fn memory(memory: &MemoryView) -> wire::Memory {
    wire::Memory {
        configured_bytes: memory.configured_bytes,
        schedulable_percent: u32::from(memory.schedulable_percent),
        schedulable_bytes: memory.bookable_bytes,
    }
}

fn availability(state: State) -> wire::Availability {
    match state {
        State::Checking => wire::Availability::Checking,
        State::Available => wire::Availability::Available,
        State::Unavailable => wire::Availability::Unavailable,
    }
}

/// A pool as `ListPools` tells it, from the fields of its view that the message has.
fn pool(view: &PoolView) -> wire::Pool {
    wire::Pool {
        name: view.name.clone(),
        kind: view.kind.clone(),
        state: i32::from(availability(view.state)),
        reasons: view.reasons.clone(),
        memory: Some(memory(&view.memory)),
        instances: u32::try_from(view.instances.n).unwrap_or(u32::MAX),
        last_tick_ms: view.last_tick_ms,
    }
}

/// The provider of a pool as `ListProviders` tells it: its credentials check and its own
/// checks, the machines it launched, and a state of its own: checking until the pool's
/// first pass, then available while every one of its checks holds.
fn provider(view: &PoolView) -> wire::Provider {
    let credentials = view.credentials.iter().map(credentials_check);
    let own = view.provider_checks.iter().map(named_check);
    let checks: Vec<wire::ProviderCheck> = credentials.chain(own).collect();
    let state = match (view.state, checks.iter().all(|check| check.ok)) {
        (State::Checking, _) => wire::Availability::Checking,
        (_, true) => wire::Availability::Available,
        (_, false) => wire::Availability::Unavailable,
    };
    wire::Provider {
        name: view.name.clone(),
        kind: view.kind.clone(),
        state: i32::from(state),
        checks,
        machines: view.machines.iter().map(machine).collect(),
    }
}

fn credentials_check(check: &CheckView) -> wire::ProviderCheck {
    wire::ProviderCheck {
        name: CREDENTIALS.to_owned(),
        ok: check.ok,
        reason: check.reason.clone().unwrap_or_default(),
        note: String::new(),
        checked_at_ms: check.checked_at_ms,
    }
}

fn named_check(check: &NamedCheckView) -> wire::ProviderCheck {
    wire::ProviderCheck {
        name: check.name.to_owned(),
        ok: check.ok,
        reason: check.reason.clone().unwrap_or_default(),
        note: check.note.clone().unwrap_or_default(),
        checked_at_ms: check.checked_at_ms,
    }
}

fn machine(view: &MachineView) -> wire::Machine {
    // The phase as `GET /v1/pools` writes it (`launching`, `registered`, ...).
    let phase = serde_json::to_value(view.state).ok();
    let phase = phase.and_then(|phase| phase.as_str().map(str::to_owned));
    wire::Machine {
        host: view.host.clone(),
        state: phase.unwrap_or_default(),
        offer: view.offer.clone(),
        zone: view.zone.clone().unwrap_or_default(),
        price_cents_per_hour: Some(view.price_cents_per_hour),
        provider_id: view.provider_id.clone().unwrap_or_default(),
    }
}

/// How each reason an ended booking has is told: the job's state when the booking ended
/// and the reason its result gives. A booking ended for a reason the row cannot tell apart
/// (a lease given back or expired) has neither.
const ENDINGS: [(EndedReason, wire::JobState, wire::ResultReason); 6] = [
    (
        EndedReason::Released,
        wire::JobState::Unspecified,
        wire::ResultReason::Unspecified,
    ),
    (
        EndedReason::Expired,
        wire::JobState::Unspecified,
        wire::ResultReason::Unspecified,
    ),
    (
        EndedReason::Lost,
        wire::JobState::Lost,
        wire::ResultReason::Unspecified,
    ),
    (
        EndedReason::CancelledWhileWaiting,
        wire::JobState::Cancelled,
        wire::ResultReason::Cancelled,
    ),
    (
        EndedReason::FailedToStart,
        wire::JobState::Queued,
        wire::ResultReason::Unspecified,
    ),
    (
        EndedReason::StoppedAtWallLimit,
        wire::JobState::Failed,
        wire::ResultReason::WallLimit,
    ),
];

/// How a whole-host booking got its host, by what became of its mark.
const DRAINS: [(DrainOutcome, wire::WholeHostOutcome); 2] = [
    (DrainOutcome::NoMark, wire::WholeHostOutcome::QuietHost),
    (DrainOutcome::MarkHeld, wire::WholeHostOutcome::Drained),
];

/// An ended booking as `GetUsage` tells it.
fn record(row: EndedRow) -> wire::UsageRecord {
    let unknown = (wire::JobState::Unspecified, wire::ResultReason::Unspecified);
    let ending = ENDINGS
        .iter()
        .find(|(reason, ..)| *reason == row.ended_reason);
    let (end_state, ended_reason) = ending.map_or(unknown, |(_, state, reason)| (*state, *reason));
    let run_ms = row.run_ms.unwrap_or(0);
    let id = row.id.to_string();
    wire::UsageRecord {
        job_id: id.clone(),
        host_id: row
            .host
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        queue_wait_ms: row.queued_ms.unwrap_or(0),
        run_ms,
        end_state: i32::from(end_state),
        ended_reason: i32::from(ended_reason),
        ended_at_ms: row.actual_end,
        booked: row.booked.map(|booked| wire::HostBooking {
            job_id: id,
            cpu_millis: booked.cpu_milli,
            memory_bytes: booked.memory_bytes,
            ..wire::HostBooking::default()
        }),
        // A mean in thousandths of a core over milliseconds is CPU time in microseconds.
        used: row.used.map(|used| wire::ResourceUsage {
            cpu_usec: used.cpu_mean_milli.saturating_mul(run_ms),
            peak_memory_bytes: used.memory_bytes,
            ..wire::ResourceUsage::default()
        }),
        whole_host: i32::from(whole_host(&row)),
        mark_give_ups: row.give_up_outcome.unwrap_or(0),
        estimated_run_ms: estimated_run(&row),
    }
}

/// How a whole-host booking got its host; unspecified for any other booking, and for one
/// whose mark was dropped or whose story was not kept.
fn whole_host(row: &EndedRow) -> wire::WholeHostOutcome {
    let whole = row.booked.is_some_and(|booked| booked.whole_host);
    let drain = DRAINS.iter().find(|(drain, _)| *drain == row.drain_outcome);
    let drain = drain.map_or(wire::WholeHostOutcome::Unspecified, |(_, outcome)| *outcome);
    match whole {
        true => drain,
        false => wire::WholeHostOutcome::Unspecified,
    }
}

/// The run time the estimate gave: its end less the booking's start; `None` without an
/// estimate, or without a known start.
fn estimated_run(row: &EndedRow) -> Option<u64> {
    let start = row.run_ms.and_then(|run| row.actual_end.checked_sub(run));
    let estimate = row.estimated_end.zip(start);
    estimate.and_then(|(end, start)| end.checked_sub(start))
}

#[tonic::async_trait]
impl Hosts for Shared {
    async fn hosts(&self) -> (HostList, LeasesOn) {
        hosts::listing(self).await
    }

    async fn cordon(&self, host: &HostId, length: Duration) -> Result<bool, Refused> {
        let agents = self.agents().ok_or(Refused::UnknownHost)?;
        let cordoned = agents.registry.cordon(host, length);
        let placed = cordoned.map_err(|_| Refused::UnknownHost)?.placed;
        saved(agents).await.map(|()| placed)
    }

    async fn uncordon(&self, host: &HostId) -> Result<bool, Refused> {
        let agents = self.agents().ok_or(Refused::UnknownHost)?;
        let uncordoned = agents.registry.uncordon(host);
        let lifted = uncordoned.map_err(|_| Refused::UnknownHost)?.lifted;
        saved(agents).await.map(|()| lifted)
    }
}

/// Writes what the registry has bound, as the host's own route does before it answers.
async fn saved(agents: &Arc<Agents>) -> Result<(), Refused> {
    match agents.save_bindings().await {
        true => Ok(()),
        false => Err(Refused::Unsaved),
    }
}

impl Pools for PoolStatus {
    fn pools(&self) -> PoolsView {
        self.view()
    }
}

/// The record of the ended bookings that the usage sink keeps.
pub(crate) struct SinkHistory(pub(crate) Arc<dyn UsageSink>);

#[tonic::async_trait]
impl History for SinkHistory {
    async fn page(&self, query: EndedQuery) -> Result<EndedPage, UsageFault> {
        let sink = Arc::clone(&self.0);
        blocking(move || sink.ended_rows_after(&query)).await
    }
}
