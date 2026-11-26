//! A scrape: the snapshot read from the state `GET /v1/hosts` reads and from the job tables,
//! rendered with the recorder's counts, and the router of the metrics listener.

use std::num::NonZeroUsize;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use js_jobs::{AllocationRecord, Booked, JobStoreError, MAX_PAGE};
use js_sched_placer::curves::CurveView;
use tracing::warn;

use super::Recorder;
use super::render::{HostSeen, JobsSeen, Snapshot, Split, render};
use crate::api::{Peer, Shared, not_found, peer_may_read_status};
use crate::hosts::{HostEntry, HostList, listing};
use crate::job_store::ServerJobs;
use crate::util::blocking;

/// The content type of the Prometheus text format.
pub(crate) const TEXT_FORMAT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// What a scrape reads: the server's state, and the events counted since it started.
struct Scrape {
    shared: Arc<Shared>,
    recorder: Arc<Recorder>,
}

/// The router of the metrics listener: `GET /metrics` over `shared` and `recorder`, to a
/// private peer only. Any other request is answered as an unknown route.
pub(crate) fn router(shared: Arc<Shared>, recorder: Arc<Recorder>) -> Router {
    let scrape = Arc::new(Scrape { shared, recorder });
    Router::new()
        .route("/metrics", get(metrics))
        .fallback(|| async { not_found() })
        .with_state(scrape)
}

async fn metrics(State(scrape): State<Arc<Scrape>>, peer: Peer) -> Response {
    if !peer_may_read_status(&peer) {
        return not_found();
    }
    match snapshot(&scrape.shared, MAX_PAGE).await {
        Ok(seen) => (
            [(CONTENT_TYPE, TEXT_FORMAT)],
            render(&seen, &scrape.recorder),
        )
            .into_response(),
        Err(error) => {
            warn!(%error, "a scrape could not read the job tables");
            let text = "the job tables could not be read\n";
            (StatusCode::SERVICE_UNAVAILABLE, text).into_response()
        }
    }
}

/// What a scrape reads of `shared` now, reading the job tables `page` jobs at a time.
///
/// # Errors
/// The [`JobStoreError`] of a failed read of the job tables.
pub(crate) async fn snapshot(
    shared: &Shared,
    page: NonZeroUsize,
) -> Result<Snapshot, JobStoreError> {
    let list = listing(shared);
    let agents = shared.agents();
    let jobs = match agents.jobs.clone() {
        Some(jobs) => Some(blocking(move || read_jobs(&jobs, page)).await?),
        None => None,
    };
    let curves = match agents.placer.clone() {
        Some(placer) => blocking(move || placer.curve_view()).await,
        None => None,
    };
    Ok(combine(list, jobs, curves))
}

/// Every job of `jobs` counted by state at the store's clock now (by which each job's submit
/// was stamped, so its age is one clock's), `page` at a time, and the live allocations.
fn read_jobs(
    jobs: &ServerJobs,
    page: NonZeroUsize,
) -> Result<(JobsSeen, Vec<AllocationRecord>), JobStoreError> {
    let now = jobs.now()?;
    let mut seen = JobsSeen::default();
    let mut resume = None;
    loop {
        let read = jobs.list_jobs(None, resume.as_ref(), page)?;
        for job in &read.jobs {
            seen.count(job.state, now.since(job.submitted_at));
        }
        resume = read.resume;
        if resume.is_none() {
            break;
        }
    }
    Ok((seen, jobs.live_allocations()?))
}

/// The snapshot of the hosts of `list`, each booked with the live allocations of `jobs`, of
/// the jobs, and of the placer's curve view `curves`.
fn combine(
    list: HostList,
    jobs: Option<(JobsSeen, Vec<AllocationRecord>)>,
    curves: Option<CurveView>,
) -> Snapshot {
    let (jobs, live) = jobs.map_or((None, Vec::new()), |(seen, live)| (Some(seen), live));
    let hosts = list.hosts.into_iter();
    let hosts = hosts.map(|entry| host_seen(entry, &live)).collect();
    Snapshot {
        jobs,
        hosts,
        curves,
    }
}

/// What `entry` has booked, of CPU and memory: its jobs' allocations, one that books the
/// whole host counted as its whole shape.
fn host_seen(entry: HostEntry, live: &[AllocationRecord]) -> HostSeen {
    let whole = (entry.shape.cpu_millis, entry.shape.memory_bytes);
    let allocated = live.iter().filter(|allocation| allocation.host == entry.id);
    let allocated = allocated.map(|allocation| match allocation.booked {
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            ..
        } => (cpu_millis, memory_bytes),
        Booked::WholeHost => whole,
    });
    let add = |(cpu, memory): (u64, u64), (more_cpu, more_memory): (u64, u64)| {
        (
            cpu.saturating_add(more_cpu),
            memory.saturating_add(more_memory),
        )
    };
    let (cpu, memory) = allocated.fold((0, 0), add);
    HostSeen {
        host: entry.id,
        cpu: Split::of(whole.0, cpu),
        memory: Split::of(whole.1, memory),
        mark: entry.mark,
    }
}
