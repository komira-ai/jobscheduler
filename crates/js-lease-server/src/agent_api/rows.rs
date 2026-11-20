//! The host's `node/` row in the job store (design `multi-instance.md`, G5 and G7): written by a
//! hello the registry admitted and refreshed by the beats it accepts, so every server that
//! shares the store places on the host and counts it in its pool, whichever server took its
//! hello. The registry still judges the hello and the beat; the row records what it admitted.
//!
//! A hello's row is written after the registry admitted it ([`hello_row`]), so a hello the
//! registry refuses (its host bound to another credential, say) writes nothing. The row of a
//! host the registry admits for the first time is counted against the pool's cap over every
//! server's rows ([`js_jobs::JobStore::hello_node_in_pool`]); a refusal there, or a row that
//! cannot be written, takes the registry's admission back, so a host is placed on only once its
//! row is there. A beat writes the row when what it holds changes or once per
//! [`js_registry::Registry::refresh_every`] ([`beat_row`]), so a host whose state stands still
//! does not move the store's head on every beat.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_agent_wire::{Heartbeat, Phase, Welcome};
use js_jobs::{Beat, JobStoreError, NodeHello, NodeState, SessionDigest};
use js_registry::Refusal;
use js_types::HostId;
use tracing::warn;

use super::{Refused, STATE_WRITE_FAILED, StatusCode, refused_hello};
use crate::agents::Agents;
use crate::util::blocking;

/// Why a hello's row was not written: the pool is full over every server's rows, or the store
/// could not take the write.
enum Unwritten {
    /// The pool holds its cap of hosts.
    PoolFull,
    /// The store failed.
    Store(JobStoreError),
}

/// Writes the row of `host`, whose hello the registry admitted with `welcome`, when the
/// server holds the job tables; nothing otherwise. A host the registry admitted for the first
/// time (`welcome.epoch` 1) is counted against its pool's cap; another is not new to the pool.
/// On a refusal or a failed write the registry's first admission is taken back.
fn hello_row(agents: &Agents, host: &HostId, welcome: &Welcome) -> Result<(), Unwritten> {
    let Some(jobs) = &agents.jobs else {
        return Ok(());
    };
    let Some(own) = agents.registry.own_host(host) else {
        return Ok(());
    };
    let view = own.host;
    let hello = NodeHello {
        session: agents.registry.session_digest(host).map(SessionDigest::new),
        pool: welcome.pool.clone(),
        claim: view.claim.clone(),
        shape: Some(view.shape.clone()),
        labels: view.labels.clone(),
        phase: view.phase,
    };
    let first = welcome.epoch == 1;
    let cap = agents.registry.pool_cap(&welcome.pool).filter(|_| first);
    let written = jobs.hello_node_in_pool(host, &hello, cap.unwrap_or(usize::MAX));
    let written = written.map_err(|error| match error {
        JobStoreError::PoolFull { .. } => Unwritten::PoolFull,
        error => Unwritten::Store(error),
    });
    match written {
        Ok(row) => {
            agents.note_node_epoch(host, Some(row.record.epoch));
            Ok(())
        }
        Err(unwritten) => {
            agents.registry.withdraw_first_hello(host, welcome.epoch);
            Err(unwritten)
        }
    }
}

/// `welcome` once the host's `node/` row is written ([`hello_row`]); otherwise the answer to
/// the hello: the refusal `pool-full` (the pool is full over every server's rows), or 503
/// `state-write-failed` when the store failed, which the agent retries.
pub(super) async fn written_row(
    agents: &Arc<Agents>,
    host: &HostId,
    welcome: Welcome,
) -> Result<Welcome, Refused> {
    let (agents, host) = (Arc::clone(agents), host.clone());
    blocking(move || match hello_row(&agents, &host, &welcome) {
        Ok(()) => Ok(welcome),
        Err(Unwritten::Store(error)) => {
            warn!(%error, %host, "agent hello refused: its row was not written");
            Err(Refused::new(
                StatusCode::SERVICE_UNAVAILABLE,
                STATE_WRITE_FAILED,
            ))
        }
        Err(Unwritten::PoolFull) => Err(refused_hello(host.as_str(), &Refusal::PoolFull)),
    })
    .await
}

/// Refreshes the row of `host` after a beat the registry accepted, which reported `phase`:
/// the row is written in the session of the host's hello on this server when its phase or
/// listing flag changed, or once per [`js_registry::Registry::refresh_every`]. A host with no
/// row of a hello on this server writes nothing; a row another server's hello has taken since
/// is left to that server. A failed write is logged, and the next beat writes it.
pub(super) fn beat_row(agents: &Agents, host: &HostId, phase: Phase) {
    let (Some(jobs), Some(epoch)) = (&agents.jobs, agents.node_epoch(host)) else {
        return;
    };
    let state = NodeState {
        phase,
        load: None,
        first_listing_pending: false,
        last_listing_clean: agents.listing_clean(host),
        missing_in_a_row: BTreeMap::new(),
    };
    let every = agents.registry.refresh_every();
    match jobs.refresh_node(host, epoch, &state, every) {
        Ok(None | Some(Beat::Applied(_) | Beat::Superseded(_))) => {}
        Ok(Some(Beat::StaleEpoch(_) | Beat::NoSession(_))) => agents.note_node_epoch(host, None),
        Err(error) => {
            warn!(%error, %host, "the host's row was not refreshed; the next beat writes it")
        }
    }
}

/// [`beat_row`] for a beat of a host whose jobs this server does not serve (its hello did not
/// report `per-job`), on the pool for blocking calls, so its row stays live for every server
/// as its beats come in. A beat whose host id is no host id writes nothing.
pub(super) async fn unserved_beat(agents: &Arc<Agents>, heartbeat: &Heartbeat) {
    if let Ok(host) = HostId::new(&heartbeat.host_id) {
        let (agents, phase) = (Arc::clone(agents), heartbeat.phase);
        blocking(move || beat_row(&agents, &host, phase)).await;
    }
}
