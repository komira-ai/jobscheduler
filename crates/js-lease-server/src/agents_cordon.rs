//! A host's cordon, as the server keeps it: a request of the host's own route or the
//! operator's ([`Agents::cordon`], [`Agents::uncordon`]), the scaler's ([`Agents::cordon_now`]),
//! the mirror of each tick ([`Agents::mirror_cordons`]) and the cordons put back at start
//! ([`restore_cordons`]).
//!
//! With the job service on, the job store's row `cordon/<host>` is the cordon's record
//! ([`js_jobs::JobStore::cordon_host`]): each request is decided in the store's transaction by
//! the rule of [`js_types::cordon`], every placement transaction refuses a host whose row is in
//! force, and the row lapses by the store's clock with nothing renewing it. So every server over
//! the store keeps a cordon placed through any of them: each pass reads the rows
//! ([`js_jobs::JobStore::cordons`]), and each tick puts them on the registry, which the host
//! listing, the scaler and the probes read. A cordon placed through another server keeps this
//! server's passes off the host at once; one lifted through another server opens the host to
//! this server's passes at its next tick, when its registry lets the cordon go. Without the
//! job service (a server that places no job) the registry decides by the same rule, and holds
//! the holder in memory only.
//!
//! Either way the host's binding is written with the cordon's end (not its holder) once the
//! request is decided, before the registry takes it, so a restart keeps the cordon: with the
//! store it is committed, and without it the file is saved after the registry took it. A write
//! that fails is answered as unsaved, and the same call repeated is the retry. With the job
//! service the row is written first: a cordon then holds on every server all the same, and a
//! lift whose binding was not written is lifted in the store, and so on every server's passes,
//! while this server's registry keeps the cordon until its next tick (a repeat of the lift says
//! it lifted nothing, and the tick's save writes the binding). A request that lost a race with
//! another server's change of the same cordon is answered as unsaved too, and asked again.
//!
//! The binding's write is conditional on the version of the host's binding row, read first
//! ([`Agents::keep`]), so a cordon never overwrites a binding another server wrote (a taint it
//! stored is taken into the registry first); a write that loses its version is answered as unsaved
//! too. A host the registry does not hold is taken from the rows first ([`Agents::take_from_rows`]),
//! so a host another server serves is served; a host no row knows is `NotHeld`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_jobs::ScaleMark;
use js_registry::{Cordoned, HostMark, NotHeld, Registry, Uncordoned};
use js_types::cordon::{self, Asker, Cordon, Cordoning, Uncordoning};
use js_types::{Clock, CordonHolder, HostId};
use tracing::{error, warn};

use super::{Agents, CordonFault};
use crate::error::StartupError;
use crate::job_store::ServerJobs;
use crate::util::blocking;

/// The fault of a job store's cordon write that failed (the store's, or another request of the
/// same host's cordon committed first), logged: nothing was written.
fn store_fault(err: js_jobs::JobStoreError) -> CordonFault {
    error!(error = %err, "cannot write the cordon of a host to the job store");
    CordonFault::Unsaved
}

impl From<NotHeld> for CordonFault {
    fn from(_: NotHeld) -> Self {
        Self::NotHeld
    }
}

impl Agents {
    /// The cordon in force on `host`, with its time left, as the registry holds it.
    ///
    /// # Errors
    /// [`CordonFault::NotHeld`] for a host the registry does not hold.
    fn held_cordon(&self, host: &HostId) -> Result<Option<Cordon<Duration>>, CordonFault> {
        let own = self.registry.own_host(host).ok_or(NotHeld)?;
        Ok(own.cordon_until.map(|until| Cordon {
            until: Duration::from_millis(until.millis().saturating_sub(own.now.millis())),
            holder: own.cordon_holder,
        }))
    }

    /// Takes `host` from the store's rows into the registry when the registry does not hold it
    /// and the server holds the job tables: the node row and the binding of a host another
    /// server serves ([`crate::agent_api::rows::refresh`], the one the hello reads them with),
    /// so that a request about the host is decided on what the store holds, and a host held by
    /// another server is not `404`. A host no row and no binding knows stays unheld, and a
    /// server without the job tables decides on its registry alone. The caller holds
    /// [`Agents::host_writes`].
    ///
    /// # Errors
    /// [`CordonFault::Unsaved`] when the rows could not be read: nothing is decided on less
    /// than the store holds.
    pub(crate) async fn take_from_rows(self: &Arc<Self>, host: &HostId) -> Result<(), CordonFault> {
        if self.jobs.is_none() || self.registry.own_host(host).is_some() {
            return Ok(());
        }
        let read = crate::agent_api::rows::refresh(self, host).await;
        read.map_err(CordonFault::unsaved)
    }

    /// Decides the cordon request of `by` for `lease` on `host`: in the job store when the
    /// server holds one (a blocking call), else on the cordon the registry holds.
    fn decide_cordon(
        &self,
        host: &HostId,
        lease: Duration,
        by: &Asker,
    ) -> Result<Cordoning<Duration>, CordonFault> {
        let held = self.held_cordon(host)?;
        let Some(jobs) = &self.jobs else {
            return Ok(cordon::cordon(held.as_ref(), lease, by));
        };
        let wrote = jobs.cordon_host(host, lease, by).map_err(store_fault)?;
        // The time left is the store's, read on its clock: put on the registry's clock it can
        // come out shorter than the end the registry holds by the time the call took. A request
        // that found a cordon in force never shortens it, so the registry keeps the later end.
        let held = held.filter(|_| wrote.already);
        let until = held.map_or(wrote.cordon.until, |held| {
            held.until.max(wrote.cordon.until)
        });
        let cordon = Cordon {
            until,
            holder: wrote.cordon.holder,
        };
        Ok(Cordoning {
            cordon,
            already: wrote.already,
            held_by_other: wrote.held_by_other,
        })
    }

    /// Decides the lift of `host`'s cordon by `holder` (any holder's for none), as
    /// [`Agents::decide_cordon`] decides a cordon: what the lift did, and the cordon in force
    /// after it.
    fn decide_uncordon(
        &self,
        host: &HostId,
        holder: Option<&CordonHolder>,
    ) -> Result<(Uncordoning, Option<Cordon<Duration>>), CordonFault> {
        let held = self.held_cordon(host)?;
        let Some(jobs) = &self.jobs else {
            let done = cordon::uncordon(held.as_ref(), holder);
            return Ok((done, held.filter(|_| !done.lifted)));
        };
        let wrote = jobs.uncordon_host(host, holder).map_err(store_fault)?;
        let done = Uncordoning {
            lifted: wrote.lifted,
            held_by_other: wrote.held_by_other,
        };
        Ok((done, wrote.cordon))
    }

    /// Writes the binding of `host` with the end of `cordon` (or with none), as
    /// [`Agents::keep`] writes it. The end is held on this server's clock, as the registry
    /// holds it, and stored on the store's ([`crate::agent_state`]): the store's clock is read
    /// first, and a store that cannot read it writes nothing.
    async fn keep_cordon(
        self: &Arc<Self>,
        host: &HostId,
        cordon: Option<&Cordon<Duration>>,
    ) -> Result<(), CordonFault> {
        let this = Arc::clone(self);
        let measured = blocking(move || this.store.measure()).await;
        measured.map_err(CordonFault::unsaved)?;
        let until = cordon.map(|cordon| self.clock.now().plus(cordon.until).millis());
        let kept = self.keep(host, |bound| bound.with_cordon(until)).await;
        kept.map_err(CordonFault::unsaved)
    }

    /// Cordons `host` for `lease` as `by` asks (the rule of [`js_types::cordon`]), and answers
    /// the host as it is after, with whether the call placed the cordon and whether the
    /// cordon in force is another holder's, left as it was.
    ///
    /// # Errors
    ///
    /// [`CordonFault::NotHeld`] for a host the registry does not hold and the rows do not know
    /// ([`Agents::take_from_rows`]); [`CordonFault::Unsaved`] when the rows could not be read,
    /// the job store's write or the binding's commit failed, or
    /// in file mode when the save after it did (the module docs say what holds then).
    pub(crate) async fn cordon(
        self: &Arc<Self>,
        host: &HostId,
        lease: Duration,
        by: Asker,
    ) -> Result<Cordoned, CordonFault> {
        let gate = self.host_writes.lock().await;
        self.take_from_rows(host).await?;
        let (this, id) = (Arc::clone(self), host.clone());
        let decided = blocking(move || this.decide_cordon(&id, lease, &by)).await?;
        if !decided.held_by_other {
            self.keep_cordon(host, Some(&decided.cordon)).await?;
        }
        let own = self.registry.set_cordon(host, Some(&decided.cordon));
        drop(gate);
        let own = own?;
        self.saved().await?;
        Ok(Cordoned {
            host: own,
            placed: !decided.already,
            held_by_other: decided.held_by_other,
        })
    }

    /// Lifts the cordon of `host` if `holder` holds it, or any cordon for none (the rule of
    /// [`js_types::cordon`]), as [`Agents::cordon`] places one.
    ///
    /// # Errors
    ///
    /// As [`Agents::cordon`].
    pub(crate) async fn uncordon(
        self: &Arc<Self>,
        host: &HostId,
        holder: Option<CordonHolder>,
    ) -> Result<Uncordoned, CordonFault> {
        let gate = self.host_writes.lock().await;
        self.take_from_rows(host).await?;
        let (this, id) = (Arc::clone(self), host.clone());
        let decided = blocking(move || this.decide_uncordon(&id, holder.as_ref())).await;
        let (done, after) = decided?;
        if done.lifted {
            self.keep_cordon(host, None).await?;
        }
        let own = self.registry.set_cordon(host, after.as_ref());
        drop(gate);
        let own = own?;
        self.saved().await?;
        Ok(Uncordoned {
            host: own,
            lifted: done.lifted,
            held_by_other: done.held_by_other,
        })
    }

    /// Cordons `host` for `lease`, unnamed, from the pool for blocking calls (the scaler's
    /// cordon of a machine its provider reclaims): decided as [`Agents::cordon`] decides it and
    /// taken by the registry; the tick's save writes the binding.
    ///
    /// # Errors
    ///
    /// As [`Agents::cordon`], without the binding's write.
    pub(crate) fn cordon_now(&self, host: &HostId, lease: Duration) -> Result<(), CordonFault> {
        let _gate = self.host_writes.blocking_lock();
        let decided = self.decide_cordon(host, lease, &Asker::Unnamed)?;
        self.registry.set_cordon(host, Some(&decided.cordon))?;
        Ok(())
    }

    /// Puts the job store's cordons on the registry ([`Registry::mirror_cordons`]), when the
    /// server holds the job store: a cordon placed, extended or lifted through another server is
    /// then what this server lists and what its scaler and probes read. The tick runs it before
    /// its save, which writes the bindings it changed. A read that fails is logged and counted,
    /// and the registry keeps the cordons it holds until the next tick.
    pub(crate) async fn mirror_cordons(self: &Arc<Self>) {
        let Some(jobs) = self.jobs.clone() else {
            return;
        };
        let gate = self.host_writes.lock().await;
        match blocking(move || jobs.cordons()).await {
            Ok(cordons) => self.registry.mirror_cordons(&cordons),
            Err(error) => {
                self.errors.fetch_add(1, Ordering::SeqCst);
                warn!(%error, "cannot read the job store's cordons; the registry keeps its own until the next tick");
            }
        }
        drop(gate);
    }
}

impl Agents {
    /// Puts the job store's scale-down marks, the rows `hostmark/<host>`, on the registry
    /// ([`Registry::mirror_marks`]), when the server holds the job store: the registry's marks
    /// are then the cache of the rows, so a mark put through another server, or by the scaler
    /// here before its row write failed, is what this registry holds after the tick. A read
    /// that fails is logged and counted, and the registry keeps the marks it holds until the
    /// next tick.
    pub(crate) async fn mirror_host_marks(self: &Arc<Self>) {
        let Some(jobs) = self.jobs.clone() else {
            return;
        };
        match blocking(move || jobs.host_marks()).await {
            Ok(rows) => {
                let named = rows.into_iter().map(|(host, (mark, _))| {
                    let mark = match mark {
                        ScaleMark::Draining => HostMark::Draining,
                        ScaleMark::Releasing => HostMark::Releasing,
                    };
                    (host, mark)
                });
                self.registry.mirror_marks(&named.collect());
            }
            Err(error) => {
                self.errors.fetch_add(1, Ordering::SeqCst);
                warn!(%error, "cannot read the job store's host marks; the registry keeps its own until the next tick");
            }
        }
    }
}

/// Writes into the job store `jobs` each cordon the registry holds that the store has no row
/// for: the cordons a binding brought back at start, which an earlier release kept in the
/// bindings alone. It is done once for the store ([`js_jobs::JobStore::restore_cordons`]): from
/// then on the store is the cordons' record, and a cordon lifted through another server, or
/// lifted here before a binding's write failed, is not written back by a later start. A row
/// the store holds already is left. A write that fails refuses the start, naming `flag`, as the
/// job store's other reads at start do.
pub(crate) fn restore_cordons(
    jobs: &ServerJobs,
    registry: &Registry,
    flag: &'static str,
) -> Result<(), StartupError> {
    let restored = jobs.restore_cordons(&registry.cordons());
    restored.map(drop).map_err(StartupError::because(flag))
}
