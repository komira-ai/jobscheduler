//! A host's session as the store row `node/<host>` (design `multi-instance.md`, section 0 and
//! G15), when the server holds the job tables: any server over the store serves any hello and
//! any heartbeat of any host, so a beat that lands on another server than the hello's is served
//! as it is, with no second hello.
//!
//! - **A hello** the registry admitted is written as the row
//!   ([`js_jobs::JobStore::hello_node_in_pool`]): the store numbers the epoch (one more than the
//!   row's), and the welcome and the registry take that number ([`hello_row`]). A hello whose
//!   row cannot be written is refused 503 `state-write-failed`, and the registry gives up the
//!   session it admitted, so the agent's next hello is not refused `host-live` by a session
//!   nobody holds.
//! - **The pool's cap** counts every server's rows. A hello of a host that has no row is judged
//!   against it before anything of the hello is written ([`room_in_pool`]): a full pool refuses
//!   it 403 `pool-full`, with no binding written. The row's write decides it
//!   again under the pool's admission row, so of two hellos that both found room one is
//!   written; the other is the 503 of a row that could not be written, as for any row that
//!   cannot be written: its binding was committed before the row's write, so the agent's next
//!   hello is refused `pool-full` before anything of it is written.
//! - **The registry is a cache of the rows.** Before a hello is judged, and before a heartbeat
//!   whose epoch the registry does not hold is checked, the row and the host's binding are read
//!   and the session another server's hello wrote is adopted ([`refresh`]): its epoch, the
//!   digest of its secret, its pool, shape and labels. The heartbeat's secret is then checked
//!   against the digest that row holds, and a beat of an epoch the row has passed is still
//!   `409 stale-epoch`. A heartbeat of the epoch the registry holds reads nothing more.
//! - **A heartbeat is a row write** guarded by its (epoch, sequence)
//!   ([`js_jobs::JobStore::beat_node`], [`beat_row`]): the beat the row applied gives the
//!   version every write of its job listing is conditional on
//!   ([`js_jobs::JobStore::guarded`]). A beat whose sequence the row has already passed, or
//!   whose listing loses that version to a newer beat or hello of the host, changes nothing
//!   more and is answered from the rows as they stand.
//! - **The listing flags are the row's**: whether the first listing since the hello is still
//!   to be applied, whether the last listing was clean, and for an agent that sends no sequence
//!   how many applied beats in a row left out each confirmed container. The beat that applies
//!   the listing writes them at the version it wrote ([`settle`]); when a newer beat moved the
//!   row first, that beat's own write stands.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use axum::http::StatusCode;
use js_agent_wire::NodeLoad;
use js_agent_wire::{Heartbeat, Hello, Welcome};
use js_jobs::{
    Beat, JobId, JobStoreError, LoadReading, NodeHello, NodeRow, NodeState, SessionDigest,
};
use js_registry::{NodeSession, Refusal, session_digest};
use js_types::HostId;
use tracing::warn;

use super::{Refused, STATE_WRITE_FAILED};
use crate::agents::{Agents, taints};
use crate::job_store::ServerJobs;
use crate::util::blocking;

/// Puts what the store holds of `host` into the registry before its hello is judged, its
/// heartbeat checked, or a cordon, a lift or a clear of its taint is decided: the taint or
/// cordon of its binding, as [`Agents::restore_stored`] does, and, with the job tables, the
/// session its row holds ([`js_registry::Registry::adopt`]), which the registry takes when it
/// holds no newer one. The binding is read with its version, which the hello's write of the
/// binding is conditional on. Without the job tables this is [`Agents::restore_stored`].
///
/// # Errors
///
/// The binding, the row or the store's clock could not be read: the call is refused with an
/// error the agent retries, and nothing is judged on less than the store holds.
pub(crate) async fn refresh(agents: &Arc<Agents>, host: &HostId) -> io::Result<()> {
    let Some(jobs) = agents.jobs.clone() else {
        return agents.restore_stored(host, "hello").await;
    };
    let this = Arc::clone(agents);
    let named = host.clone();
    let read = blocking(move || {
        let binding = this.store.stored(&named)?;
        let row = jobs.read_node(&named).map_err(io::Error::other)?;
        let now = jobs.now().map_err(io::Error::other)?;
        io::Result::Ok((binding, row, now))
    })
    .await?;
    let (binding, row, now) = read;
    if let Some(binding) = &binding {
        taints::take_in(&agents.registry, host, binding, "store");
    }
    if let Some(row) = row {
        let record = row.record;
        let node = NodeSession {
            epoch: record.epoch,
            session: record.session.map(|digest| *digest.as_bytes()),
            pool: record.pool,
            claim: record.claim,
            labels: record.labels,
            phase: record.state.phase,
            agent_process: record.agent_process,
            agent_version: record.agent_version,
            silent_for: now.since(record.heard_at),
            binding: binding.map(|binding| binding.with_taint(None).with_cordon(None)),
        };
        agents.registry.adopt(host, node);
    }
    Ok(())
}

/// [`refresh`] for a heartbeat, when the server holds the job tables and its registry does
/// not hold the epoch the beat names: the session may be one another server's hello began.
/// A heartbeat of the epoch the registry holds reads nothing here, and one whose host id does
/// not parse is left to the registry to refuse.
///
/// # Errors
///
/// As [`refresh`].
pub(super) async fn refresh_for_beat(
    agents: &Arc<Agents>,
    heartbeat: &Heartbeat,
) -> io::Result<()> {
    let Some(host) = agents
        .jobs
        .as_ref()
        .and_then(|_| HostId::new(&heartbeat.host_id).ok())
    else {
        return Ok(());
    };
    let own = agents.registry.own_host(&host);
    if own.is_some_and(|own| own.host.epoch == heartbeat.epoch) {
        return Ok(());
    }
    refresh(agents, &host).await
}

/// Refuses the hello of `host` 403 `pool-full` when `host` has no row and its pool already holds
/// the pool's cap of rows, over every server's rows ([`js_jobs::JobStore::pool_admits`]): before
/// anything of the hello is written, as every refusal of a hello. Nothing without the job
/// tables, or for a pool with no cap of its own.
///
/// # Errors
///
/// `403 pool-full`; `503 state-write-failed` when the rows could not be read, which the agent
/// retries: a hello is never admitted on less than the rows.
pub(super) async fn room_in_pool(
    agents: &Arc<Agents>,
    host: &HostId,
    hello: &Hello,
) -> Result<(), Refused> {
    let (Some(jobs), Some(cap)) = (agents.jobs.clone(), agents.registry.pool_cap(&hello.pool))
    else {
        return Ok(());
    };
    let (named, pool) = (host.clone(), hello.pool.clone());
    match blocking(move || jobs.pool_admits(&named, &pool, cap)).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(super::refused_hello(hello, &Refusal::PoolFull)),
        Err(error) => {
            warn!(%error, %host, "agent hello refused: its pool's rows could not be read");
            Err(Refused::new(
                StatusCode::SERVICE_UNAVAILABLE,
                STATE_WRITE_FAILED,
            ))
        }
    }
}

/// Writes the hello the registry admitted as `welcome` into the row of `host`, when the server
/// holds the job tables ([`record_hello`]). Nothing without the job tables.
///
/// # Errors
///
/// The row could not be written: 503 `state-write-failed`, and the registry has given up the
/// session it admitted.
pub(super) async fn hello_row(
    agents: &Arc<Agents>,
    host: &HostId,
    hello: &Hello,
    welcome: &mut Welcome,
) -> Result<(), Refused> {
    if agents.jobs.is_none() {
        return Ok(());
    }
    let this = Arc::clone(agents);
    let (named, said, mut answer) = (host.clone(), hello.clone(), welcome.clone());
    let written = blocking(move || {
        let recorded = record_hello(&this, &named, &said, &mut answer);
        recorded.map(|()| answer)
    })
    .await;
    match written {
        Ok(answer) => {
            *welcome = answer;
            Ok(())
        }
        Err(error) => {
            warn!(%error, %host, "agent hello refused: its session could not be written");
            Err(Refused::new(
                StatusCode::SERVICE_UNAVAILABLE,
                STATE_WRITE_FAILED,
            ))
        }
    }
}

/// Writes the hello the registry admitted as `welcome` into the row of `host`
/// ([`js_jobs::JobStore::hello_node_in_pool`], under the pool's cap when it has one): the digest
/// of the welcome's secret, what the hello announced (its agent version included), and the
/// shape the registry books. The
/// store numbers the epoch, and the registry and `welcome` take that number. Nothing without
/// the job tables. It blocks on the store.
///
/// # Errors
///
/// The row could not be written, or the pool filled since [`room_in_pool`] read it
/// ([`JobStoreError::PoolFull`]): the registry has given up the session it admitted
/// ([`js_registry::Registry::abandon`]), so no beat of it is taken and the next hello is not
/// refused `host-live` for it. The binding the hello committed before the row stands.
pub(crate) fn record_hello(
    agents: &Agents,
    host: &HostId,
    hello: &Hello,
    welcome: &mut Welcome,
) -> Result<(), JobStoreError> {
    let Some(jobs) = &agents.jobs else {
        return Ok(());
    };
    let own = agents.registry.own_host(host);
    let row = NodeHello {
        session: Some(SessionDigest::new(session_digest(&welcome.session))),
        pool: hello.pool.clone(),
        claim: own.as_ref().and_then(|own| own.host.claim.clone()),
        shape: own.map(|own| own.host.shape),
        labels: hello.labels.clone(),
        phase: hello.phase,
        agent_process: hello.agent_process.clone(),
        agent_version: Some(hello.agent_version.clone()),
    };
    let cap = agents.registry.pool_cap(&hello.pool).unwrap_or(usize::MAX);
    match jobs.hello_node_in_pool(host, &row, cap) {
        Ok(row) => {
            agents
                .registry
                .renumber(host, welcome.epoch, row.record.epoch);
            welcome.epoch = row.record.epoch;
            Ok(())
        }
        Err(error) => {
            agents.registry.abandon(host, welcome.epoch);
            Err(error)
        }
    }
}

/// Applies `heartbeat` to its host's row, when the server holds the job tables
/// ([`js_jobs::JobStore::beat_node`]): its phase and its load, with the listing flags kept for
/// the listing to settle ([`settle`]). The row as written, whose version the listing's writes
/// are conditional on; `None` without the job tables, and for a beat whose sequence the row has
/// already passed, which writes nothing and is answered from the rows.
///
/// # Errors
///
/// `409 stale-epoch` for a beat of an epoch a later hello passed, `409 unknown-host` for an
/// epoch the row holds no hello of (the agent says hello), and `503 state-write-failed` when
/// the row could not be read or written.
pub(super) async fn beat_row(
    agents: &Arc<Agents>,
    heartbeat: &Heartbeat,
) -> Result<Option<NodeRow>, Refused> {
    let Some(jobs) = agents.jobs.clone() else {
        return Ok(None);
    };
    // The registry accepted the beat, so its host id parses.
    let host = HostId::new(&heartbeat.host_id).map_err(Refusal::BadHeartbeatHostId)?;
    let (epoch, seq, phase) = (heartbeat.epoch, heartbeat.seq, heartbeat.phase);
    let load = heartbeat.load.as_ref().map(reading);
    let named = host.clone();
    let beat = blocking(move || {
        jobs.beat_node(&named, epoch, seq, |record| NodeState {
            phase,
            load,
            ..record.state.clone()
        })
    })
    .await;
    match beat {
        Ok(Beat::Applied(row)) => Ok(Some(row)),
        Ok(Beat::Superseded(_)) => Ok(None),
        Ok(Beat::StaleEpoch(_)) => Err(Refused::of(&Refusal::StaleEpoch)),
        Ok(Beat::NoSession(_)) => Err(Refused::of(&Refusal::UnknownHost)),
        Err(error) => {
            warn!(%error, %host, "a heartbeat could not be written to its host's row");
            Err(Refused::new(
                StatusCode::SERVICE_UNAVAILABLE,
                STATE_WRITE_FAILED,
            ))
        }
    }
}

/// The scalar part of `load` the row keeps ([`LoadReading`]).
fn reading(load: &NodeLoad) -> LoadReading {
    LoadReading {
        seq: load.seq,
        mono_usec: load.mono_usec,
        cpu_usage_usec: load.node_cpu_usec(),
        working_set_bytes: load.working_set_bytes,
        active_file_bytes: load.active_file_bytes,
        actions_cpu_limited: load.actions_cpu_limited,
    }
}

/// Records in `row`, at the version the beat wrote, what its applied listing left: the first
/// listing since the hello is applied, whether the listing was `clean`, and the confirmed
/// containers `missing` left out by applied beats in a row. When a newer beat or hello moved
/// the row first, nothing is written, and that one's own write stands. A write that fails is
/// logged: the flag stays as it was, and the next beat's listing (the agent's whole account)
/// is judged as the first again.
pub(super) fn settle(jobs: &ServerJobs, row: &NodeRow, clean: bool, missing: BTreeMap<JobId, u32>) {
    let state = NodeState {
        first_listing_pending: false,
        last_listing_clean: clean,
        missing_in_a_row: missing,
        ..row.record.state.clone()
    };
    if let Err(error) = jobs.settle_node(row, state) {
        let host = &row.record.host;
        warn!(%error, %host, "a heartbeat's listing flags could not be written to its host's row");
    }
}

#[cfg(test)]
#[path = "../agent_api_rows_tests.rs"]
mod tests;
