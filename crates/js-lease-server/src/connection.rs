//! A host's connection to the servers: which server's session the host's rows are written
//! under, and that only the newest connection writes them.
//!
//! A host says hello to one server, which keeps its session in memory; its next request can
//! reach another server, which has no session for it and answers `unknown-host`, and the agent
//! says hello again there. For a moment, then, two servers hold a session for one host, and the
//! one that heard the older hello must not go on writing the host's rows (a listing committed
//! from an older beat over what the newer connection has already committed).
//!
//! The row `node/<host>` is the connection. A hello of a host that speaks the per-job messages
//! writes it, whatever it held (the newest hello wins), and the server keeps the version its
//! write got: the connection id, which is the revision of that commit, so a newer connection's
//! is always greater. Every write of a heartbeat's job listing is made through a store that
//! also checks the row still has that version ([`js_state::Checked`]), so a server whose
//! connection was replaced commits nothing for the host, and answers the beat `session-mismatch`,
//! which the agent answers with a hello.

use std::sync::Arc;

use js_agent_wire::{Hello, PER_JOB_CAPABILITY};
use js_jobs::JobStore;
use js_state::{Checked, Cond, Store, StoreError, Txn, Version};
use js_types::HostId;

use crate::agents::Agents;
use crate::job_store::ServerJobs;

/// The prefix of the connection rows.
const NODE: &str = "node/";

/// The key of the connection row of `host`.
fn key(host: &HostId) -> String {
    format!("{NODE}{host}")
}

/// Whether `hello` is of a host whose job listings this server commits.
fn lists_jobs(hello: &Hello) -> bool {
    let reported = hello.capabilities.get(PER_JOB_CAPABILITY);
    reported.map(String::as_str) == Some("true")
}

impl Agents {
    /// Takes the connection of the host that said `hello` for this server, when the host
    /// speaks the per-job messages and the server holds the job tables: the row is written
    /// (at the version read, or absent), at the registry's epoch for the record, and the version
    /// the commit gave it is kept as the connection's id. Nothing happens for another host.
    ///
    /// # Errors
    /// [`StoreError`] when the store fails, or when the row changed between the read and the
    /// write (a newer hello of the host at another server took the connection first): the hello is
    /// not welcomed, for a server that cannot write the connection must not serve the host, and
    /// the agent says hello again.
    pub(crate) fn open_connection(
        &self,
        host: &HostId,
        hello: &Hello,
        epoch: u64,
    ) -> Result<(), StoreError> {
        let Some(jobs) = self.jobs.as_ref().filter(|_| lists_jobs(hello)) else {
            return Ok(());
        };
        let key = key(host);
        let cond = jobs
            .store()
            .get(&key)?
            .map_or(Cond::Absent, |row| Cond::Version(row.version));
        let put = Txn::new().put(key.as_str(), epoch.to_string(), cond);
        let commit = jobs.store().txn(put)?.map_err(|_| {
            StoreError::Failed(format!("a newer hello of {host} took its connection first"))
        })?;
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host.clone(), Version(commit.revision.0));
        Ok(())
    }

    /// The version of the connection this server holds for `host`: `None` when it holds none.
    fn connection_of(&self, host: &HostId) -> Option<Version> {
        let held = self
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.get(host).copied()
    }

    /// The job tables as a heartbeat of `host` writes them: every write also checks that the
    /// connection this server holds for the host is still the newest, and a view of them
    /// whose writes conflict once it is not.
    ///
    /// # Errors
    /// [`Superseded`] when this server holds no connection for the host, or the row no longer
    /// has its version (a newer hello): nothing is to be written for it. [`Failed`] with the
    /// store's error when the row cannot be read.
    pub(crate) fn connected_jobs(
        &self,
        jobs: &ServerJobs,
        host: &HostId,
    ) -> Result<ServerJobs, Unconnected> {
        let held = self.connection_of(host).ok_or(Unconnected::Superseded)?;
        let key = key(host);
        let row = jobs.store().get(&key).map_err(Unconnected::Failed)?;
        if row.map(|row| row.version) != Some(held) {
            return Err(Unconnected::Superseded);
        }
        let checked: Arc<dyn Store> = Arc::new(Checked::new(
            Arc::clone(jobs.store()),
            key,
            Cond::Version(held),
        ));
        Ok(JobStore::over(jobs, Arc::new(checked)))
    }

    /// Whether the connection this server holds for `host` is no longer the newest, as the
    /// store says now: the row has another version. A row the store cannot read or does not hold
    /// is not said to be superseded (the failure that asked is reported as it was).
    pub(crate) fn connection_superseded(&self, jobs: &ServerJobs, host: &HostId) -> bool {
        let row = jobs.store().get(&key(host)).ok().flatten();
        row.is_some_and(|row| Some(row.version) != self.connection_of(host))
    }
}

/// Why a heartbeat's writes are not made through the host's connection.
#[derive(Debug)]
pub(crate) enum Unconnected {
    /// This server does not hold the newest connection of the host.
    Superseded,
    /// The connection row could not be read.
    Failed(StoreError),
}
