//! The taint of a host job on the host's binding: written after the pass that placed the job
//! ([`Agents::keep_taints`]) and put back at start from the job store's taint rows
//! ([`restore_job_taints`]).
//!
//! The placement commit writes `taint/<host>` in the job store, and the host's binding lives
//! in another store (the `--store` state store, or the `.agents` file), so the two cannot be
//! one transaction. The job store's row holds the taint until the binding does: once the
//! binding with the taint is written, the row is deleted ([`js_jobs::JobStore::drop_taints`]),
//! so the binding is the taint's one record from then on, as it is for every other taint. A
//! crash between the two writes, or a write that fails, leaves the row, and the next start puts
//! it on the binding.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_agent_wire::Taint;
use js_jobs::HOST_JOB_TAINT;
use js_registry::Binding;
use js_types::HostId;
use tracing::{info, warn};

use super::Agents;
use crate::agent_state::BindingStore;
use crate::error::StartupError;
use crate::job_store::ServerJobs;
use crate::util::blocking;

impl Agents {
    /// Writes the bindings of `hosts`, the hosts a pass over the queued jobs tainted
    /// ([`Agents::place_jobs`]), with the taint the registry now holds for them: committed in
    /// one transaction with the store, saved to the file without it, under
    /// [`Agents::host_writes`] ([`Agents::save_bindings`]); then deletes the job store's taint
    /// rows of those of them that have a binding. Nothing for no hosts. A write that fails is
    /// logged and leaves the rows: the tick's save writes the bindings again, and the next start
    /// puts each row left on its binding and deletes it.
    pub(crate) async fn keep_taints(self: &Arc<Self>, hosts: Vec<HostId>) {
        if hosts.is_empty() {
            return;
        }
        if !self.save_bindings().await {
            warn!(
                ?hosts,
                "a host job's taint is not written to the binding; the next tick writes it"
            );
            return;
        }
        let bound = self.registry.bindings();
        let kept: Vec<HostId> = hosts
            .into_iter()
            .filter(|host| bound.contains_key(host))
            .collect();
        let this = Arc::clone(self);
        let dropped = blocking(move || {
            let dropped = this
                .jobs
                .as_ref()
                .map_or(Ok(()), |jobs| jobs.drop_taints(&kept));
            dropped.inspect_err(|error| {
                this.fence.note(error);
            })
        })
        .await;
        if let Err(error) = dropped {
            warn!(%error, "a host job's taint row is not deleted; the next start deletes it");
        }
    }
}

/// Puts each taint row of the job store on the binding of its host, when that binding holds
/// no taint: reason [`HOST_JOB_TAINT`], the row's job as the run id. A taint the binding
/// holds already stands (the first taint of a host does). The bindings are then written (in
/// one transaction with the store, to the file without it) and the rows of every host that
/// has a binding deleted, so the bindings hold the taints from here on. A row whose host has
/// no binding is logged, skipped and kept. `flag` is the job service's, which a refusal names,
/// as the job store's own refusals do.
///
/// # Errors
///
/// The taint rows could not be read or deleted, or the bindings could not be written: the
/// start is refused, and `bindings` is left as it was when the bindings could not be written.
pub(super) fn restore_job_taints(
    jobs: &ServerJobs,
    store: &BindingStore,
    bindings: &mut BTreeMap<HostId, Binding>,
    flag: &'static str,
) -> Result<(), StartupError> {
    let rows = jobs.host_taints().map_err(StartupError::because(flag))?;
    let mut merged = bindings.clone();
    let mut kept = Vec::new();
    for (host, row) in rows {
        let Some(bound) = merged.get_mut(&host) else {
            warn!(
                %host,
                job = %row.job,
                "a host job tainted a host with no binding; its taint is not restored"
            );
            continue;
        };
        if bound.taint().is_none() {
            let taint = Taint {
                reason: HOST_JOB_TAINT.to_owned(),
                run_id: Some(row.job.to_string()),
                check: None,
            };
            info!(%host, job = %row.job, "a host job's taint is put back on the host's binding");
            *bound = bound.clone().with_taint(Some(taint));
        }
        kept.push(host);
    }
    store.save(|| merged.clone()).map_err(|error| {
        StartupError::new(flag, format!("cannot write the host bindings: {error}"))
    })?;
    *bindings = merged;
    jobs.drop_taints(&kept).map_err(StartupError::because(flag))
}

#[cfg(test)]
#[path = "agents_taints_tests.rs"]
mod tests;
