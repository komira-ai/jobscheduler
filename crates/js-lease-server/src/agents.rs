//! The registering side of the server: the registry, the join tokens, the hello limit
//! and the bindings, and how they are put together at start.
//!
//! Without a store setting the join tokens and the bindings are kept in files beside the
//! state file; with one, in the state store ([`crate::host_keys`]), read once at start.
//! Every change of them that the server makes itself is written under one gate
//! ([`Agents::host_writes`]), from its commit to the registry's taking it: a hello, the
//! taint a heartbeat brings, a cordon or its lift, and the save of each tick. With the store
//! the commit comes first and the registry takes the change only once it succeeded.
//!
//! The taint of a host job is the exception: the placement commit writes it in the job store,
//! the registry takes it next, and the binding is written after ([`Agents::keep_taints`]). At
//! start a taint row of the job store that its host's binding lacks is put on the binding,
//! the bindings are written and the rows deleted before the registry is given the bindings
//! back, so a crash between the two writes loses nothing; once the binding holds the taint
//! the row is deleted, after a pass too.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use js_agent_wire::{Heartbeat, Hello, InstanceIdentity};
use js_elastic::HostName;
use js_registry::{Binding, Cordoned, Credential, OsSessions, Registry, Uncordoned};
use js_types::Clock;
use js_types::{HostCapacity, HostId, ProviderId};
use tracing::{error, info, warn};

use self::taints::restore_job_taints;
use crate::agent_state::{self, BindingStore, Change};
use crate::agent_tokens::AgentTokens;
use crate::clock::SharedClock;
use crate::config::{AgentSetup, Hosts, placer, split};
use crate::error::StartupError;
use crate::fence::Fence;
use crate::host_keys::Writes;
use crate::job_expiry::JobLimits;
use crate::job_store::{self, FeedTrim, ServerJobs};
use crate::join_tokens::{Admitted, JoinRefusal, JoinTokens, MintError, Minted, RevokeError};
use crate::logs::{self, LogGrants};
use crate::metrics::Meter;
use crate::rate::HelloLimiter;
use crate::sched::place::SchedPlacer;
use crate::store::ServerStore;
use crate::usage_hub::Usage;
use crate::usage_map::limits_of;
use crate::usage_setup::UsageSetup;
use crate::util::blocking;

/// What the agent routes and the tick share. Agent handlers take the registry's lock and
/// nothing else of the server's.
pub(crate) struct Agents {
    pub(crate) registry: Registry,
    pub(crate) tokens: AgentTokens,
    /// The one-time tokens minted for launched instances.
    pub(crate) joins: JoinTokens,
    /// The one provider the mint route accepts, or `None` for any.
    pub(crate) mint_provider: Option<String>,
    pub(crate) limiter: HelloLimiter,
    /// The server's clock, which bounds a run by the time its lease has left.
    pub(crate) clock: SharedClock,
    store: BindingStore,
    /// The gate every change of the bindings and of the join tokens' uses goes through, held
    /// from the change's commit until the registry has taken it (or its undo is committed): a
    /// hello, the taint of a heartbeat, a cordon or its lift, and the save of each tick. So
    /// the server takes its hellos one at a time, and no save reads the registry between a
    /// commit and the change it commits. Held across the store's calls; no other lock is
    /// held while it is awaited.
    pub(crate) host_writes: tokio::sync::Mutex<()>,
    errors: AtomicU64,
    /// The usage side, unless `--usage-off`. Its lock is a leaf, taken after the registry's
    /// call has returned.
    pub(crate) usage: Option<Arc<Usage>>,
    /// The job and allocation tables, when the job service is on (`--job-listen`): the one
    /// handle of the server, which the job service holds too. With them, each heartbeat of
    /// a host that reports the `per-job` capability is also served the starts and stops of its
    /// jobs, and its job listing is committed ([`crate::per_job`]); without them, and for any
    /// other host, the reply is the lease and run reply alone.
    pub(crate) jobs: Option<Arc<ServerJobs>>,
    /// The job store's change feed, trimmed on each tick ([`Agents::job_pass`]); held with
    /// the job tables.
    pub(crate) job_feed: Option<FeedTrim>,
    /// The state store under the job tables, held with them: `GetUsage` reads the
    /// `usage/ended/` row each free of a job's allocation writes there.
    pub(crate) job_state: Option<Arc<dyn js_state::Store>>,
    /// The registry's placer, which also places the queued jobs when the job tables are
    /// held ([`Agents::place_jobs`]); `None` only for a test's agent side built without
    /// one, which places no job.
    pub(crate) placer: Option<Arc<SchedPlacer>>,
    /// The hosts whose last job listing named a job they hold no live allocation for. Its lock
    /// is a leaf.
    unclean: Mutex<BTreeSet<HostId>>,
    /// The host credentials and job sinks of live logs, with `sink = "object_store"`
    /// ([`crate::logs`]); `None` with `sink = "none"`. Its lock is a leaf, and no mint runs
    /// under it or under the registry's.
    pub(crate) logs: Option<Arc<LogGrants>>,
    /// The pool's `max_queue_wait` and `start_timeout`, which the job pass of the tick
    /// enforces ([`Agents::job_pass`]).
    pub(crate) job_limits: JobLimits,
    /// For each host, the session whose job listing was last committed: a listing of another
    /// session is the first after a hello ([`crate::job_loss`]). Its lock is a leaf.
    listed_sessions: Mutex<BTreeMap<HostId, Session>>,
    /// Where the job side's events are counted ([`crate::metrics`]); set once, at start, when
    /// the server serves its metrics, and counting nothing until then.
    meter: OnceLock<Meter>,
    /// Trips when the state store says this process is no longer its writer
    /// ([`crate::fence`]): the tick ends, heartbeats are refused and the process exits.
    pub(crate) fence: Fence,
    /// The gate of the server's pass over the queued jobs ([`Agents::place_jobs`]): one pass
    /// at a time, from its read of the open hosts to its last taint.
    pub(crate) placing: Mutex<()>,
}

/// Why a cordon or its lift was not done, or not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CordonFault {
    /// The registry does not hold the host.
    NotHeld,
    /// The commit before it failed, so nothing changed; or the save after it did.
    Unsaved,
}

impl CordonFault {
    /// The fault of a commit that failed, logged.
    fn unsaved(err: io::Error) -> Self {
        error!(error = %err, "cannot commit the cordon of a host");
        Self::Unsaved
    }
}

/// A host's session as the job side tells one from another: the epoch of its hello and when
/// the hello was taken, in milliseconds (a host removed and admitted again starts its epochs
/// over, at a later hello).
pub(crate) type Session = (u64, Option<u64>);

/// Where a file of the agent side is kept: beside the state file at `state_file`, with
/// `suffix` added to its name.
pub(crate) fn state_path(state_file: &Path, suffix: &str) -> PathBuf {
    let mut name = state_file.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    state_file.with_file_name(name)
}

/// What [`assemble`] returns: the hosts the static pool lists, and the agent side when it
/// is on.
pub(crate) type Assembled = (Vec<HostCapacity>, Option<Arc<Agents>>);

/// The hosts of the static pool, and the agent side when it is on.
///
/// Without agent flags the hosts are those `--pool-config` names. With them the agent side
/// is a registry on `clock`, seeded with the bindings the previous run persisted, which
/// refuses a hello for the id of any host the static pool lists. The agent side mints the host credentials
/// of live logs with `logs` ([`crate::logs`]); without agent flags no host reports, and
/// `logs` is dropped.
#[cfg(test)]
pub(crate) fn assemble(
    hosts: Hosts,
    usage: UsageSetup,
    clock: &SharedClock,
    state_file: &Path,
    logs: Option<Arc<LogGrants>>,
) -> Result<Assembled, StartupError> {
    assemble_in(hosts, usage, clock, (state_file, None), logs)
}

/// [`assemble`], with the join tokens and the bindings kept in the state store `kept` when
/// there is one, instead of the files beside the state file.
pub(crate) fn assemble_in(
    hosts: Hosts,
    usage: UsageSetup,
    clock: &SharedClock,
    (state_file, kept): (&Path, Option<&ServerStore>),
    logs: Option<Arc<LogGrants>>,
) -> Result<Assembled, StartupError> {
    let placed = placed_in(hosts, usage, clock, (state_file, kept), logs);
    placed.map(|(assembled, _)| assembled)
}

/// [`assemble`], with the placer the registry was given; none for static hosts only.
#[cfg(test)]
pub(crate) fn assemble_placed(
    hosts: Hosts,
    usage: UsageSetup,
    clock: &SharedClock,
    state_file: &Path,
    logs: Option<Arc<LogGrants>>,
) -> Result<(Assembled, Option<Arc<SchedPlacer>>), StartupError> {
    placed_in(hosts, usage, clock, (state_file, None), logs)
}

/// The join tokens, the bindings read at start and their store: from the files beside the
/// state file `state_file`, or from the state store `kept`, whose refusal names its
/// setting when a value cannot be read as written.
fn open_hosts(
    clock: &SharedClock,
    (state_file, kept): (&Path, Option<&ServerStore>),
) -> Result<(JoinTokens, BTreeMap<HostId, Binding>, BindingStore), StartupError> {
    let Some(kept) = kept else {
        let path = state_path(state_file, ".agents");
        let joins = JoinTokens::open(
            state_path(state_file, ".joins"),
            clock.clone(),
            Box::new(OsSessions),
        )?;
        let bindings = agent_state::load(&path)?;
        let store = BindingStore::new(path, bindings.clone());
        return Ok((joins, bindings, store));
    };
    let refused = |reason: String| kept.refuse(reason);
    let joins = JoinTokens::in_store(kept.store(), clock.clone(), Box::new(OsSessions));
    let joins = joins.map_err(refused)?;
    let (store, bindings) = BindingStore::in_store(kept.store()).map_err(refused)?;
    Ok((joins, bindings, store))
}

/// [`assemble_in`], with the placer the registry was given; none for static hosts only.
fn placed_in(
    hosts: Hosts,
    usage: UsageSetup,
    clock: &SharedClock,
    (state_file, kept): (&Path, Option<&ServerStore>),
    logs: Option<Arc<LogGrants>>,
) -> Result<(Assembled, Option<Arc<SchedPlacer>>), StartupError> {
    match hosts {
        Hosts::Static(source) => Ok(((source.hosts()?, None), None)),
        Hosts::Layered {
            static_pool,
            agents,
        } => {
            let AgentSetup {
                tokens,
                pool,
                mint_provider,
                hello_limit,
                require_host_credential,
                cpu_slack,
                max_lease_wall,
                job_limits,
                jobs,
                job_sources,
            } = *agents;
            let (joins, mut bindings, store) = open_hosts(clock, (state_file, kept))?;
            let joins = joins.requiring_credential(require_host_credential);
            let booked = pool.shape().resources();
            let limits = limits_of(booked.cpu_millis(), booked.memory_bytes());
            // The scheduler picks the host of a start, and books the share of each host's
            // memory the pool's percent says; the registry books the whole shape.
            let (pool, memory) = split(pool);
            let placer = placer(max_lease_wall, cpu_slack, memory, clock);
            let registry = Registry::new(pool, clock.clone(), placer.clone());
            let usage = usage.build(clock, limits)?;
            // The one job store of the server, when the job service is on. It is opened
            // before the bindings are put back: a host a host job tainted whose binding lost
            // the taint (a crash between the placement and the binding's write) gets it from
            // the job store's row first. Its commit failing refuses the start, as a store
            // that cannot be read does.
            let (jobs, job_feed, job_state) = match jobs {
                Some(setup) => {
                    let opened = job_store::open(state_file, setup, &job_sources)?;
                    restore_job_taints(&opened.jobs, &store, &mut bindings, setup.flag)?;
                    (Some(opened.jobs), Some(opened.feed), Some(opened.store))
                }
                None => (None, None, None),
            };
            for (host, binding) in &bindings {
                registry.restore_tainted(host, binding.clone());
            }
            let listed = static_pool.map_or_else(|| Ok(Vec::new()), |source| source.hosts())?;
            registry.reserve(listed.iter().map(|host| host.host.clone()));
            let agents = Agents {
                registry,
                tokens,
                joins,
                mint_provider,
                limiter: HelloLimiter::new(hello_limit, clock.clone()),
                clock: clock.clone(),
                store,
                host_writes: tokio::sync::Mutex::new(()),
                errors: AtomicU64::new(0),
                usage,
                jobs,
                job_feed,
                job_state,
                placer: Some(placer.clone()),
                unclean: Mutex::default(),
                logs,
                job_limits,
                listed_sessions: Mutex::default(),
                meter: OnceLock::new(),
                fence: Fence::new(),
                placing: Mutex::default(),
            };
            Ok(((listed, Some(Arc::new(agents))), Some(placer)))
        }
    }
}

impl Agents {
    /// The agent side over `registry`, with its files in `dir`, for a test that drives
    /// the routes without a socket.
    #[cfg(test)]
    pub(crate) fn over(registry: Registry, clock: &SharedClock, dir: &Path) -> Self {
        use crate::config::{Clients, LiveClients, parse_token};
        use crate::rate::HelloLimit;
        let client = Clients::only(&parse_token(&[b'c'; 40]).unwrap(), "--token-file");
        let client = LiveClients::new(client);
        let tokens = dir.join("agent-tokens");
        std::fs::write(&tokens, [b'j'; 40]).unwrap();
        std::fs::set_permissions(&tokens, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let joins = JoinTokens::open(
            dir.join("joins"),
            clock.clone(),
            Box::new(js_registry::SequentialSessions::new()),
        )
        .unwrap();
        let limit = HelloLimit {
            burst: 5,
            interval: std::time::Duration::from_secs(1),
        };
        Self {
            registry,
            tokens: AgentTokens::open(&tokens, &client).unwrap(),
            joins,
            mint_provider: None,
            limiter: HelloLimiter::new(limit, clock.clone()),
            clock: clock.clone(),
            store: BindingStore::new(dir.join("bindings"), Default::default()),
            host_writes: tokio::sync::Mutex::new(()),
            errors: AtomicU64::new(0),
            usage: None,
            jobs: None,
            job_feed: None,
            job_state: None,
            placer: None,
            unclean: Mutex::default(),
            logs: None,
            job_limits: JobLimits::default(),
            listed_sessions: Mutex::default(),
            meter: OnceLock::new(),
            fence: Fence::new(),
            placing: Mutex::default(),
        }
    }

    /// This agent side with its join tokens and bindings kept in `store` (empty when it is
    /// read here), as a server with a store setting keeps them, for a test of their commits.
    #[cfg(test)]
    pub(crate) fn kept_in(self, store: Arc<dyn js_state::Store>) -> Self {
        let random = Box::new(js_registry::SequentialSessions::new());
        let joins = JoinTokens::in_store(Arc::clone(&store), self.clock.clone(), random);
        let (bound, _) = BindingStore::in_store(store).unwrap();
        Self {
            joins: joins.unwrap(),
            store: bound,
            ..self
        }
    }

    /// This agent side holding the job tables `jobs`, for a test of the per-job heartbeat.
    #[cfg(test)]
    pub(crate) fn with_jobs(self, jobs: Arc<ServerJobs>) -> Self {
        Self {
            jobs: Some(jobs),
            ..self
        }
    }

    /// This agent side trimming the job store's feed through `feed` on each tick, for a test
    /// of the job pass.
    #[cfg(test)]
    pub(crate) fn with_job_feed(self, feed: FeedTrim) -> Self {
        Self {
            job_feed: Some(feed),
            ..self
        }
    }

    /// This agent side minting the host credentials of live logs with `logs`, for a test of
    /// the heartbeat's log grant and the start's sink.
    #[cfg(test)]
    pub(crate) fn with_logs(self, logs: Arc<LogGrants>) -> Self {
        Self {
            logs: Some(logs),
            ..self
        }
    }

    /// This agent side holding its jobs to `limits`, for a test of the job pass.
    #[cfg(test)]
    pub(crate) fn with_job_limits(self, limits: JobLimits) -> Self {
        Self {
            job_limits: limits,
            ..self
        }
    }

    /// Whether a job listing of `host` in `session` is the first since the host's hello: no
    /// listing of that session has been committed ([`Agents::listed`]).
    pub(crate) fn first_listing(&self, host: &HostId, session: Session) -> bool {
        let listed = self
            .listed_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        listed.get(host) != Some(&session)
    }

    /// Records that a job listing of `host` in `session` was committed.
    pub(crate) fn listed(&self, host: &HostId, session: Session) {
        let mut listed = self
            .listed_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        listed.insert(host.clone(), session);
    }

    /// This agent side placing with `placer` (`--placement js-sched`), for a test of the
    /// heartbeat's pass over the queued jobs. The registry must already hold it.
    #[cfg(test)]
    pub(crate) fn with_placer(self, placer: Arc<SchedPlacer>) -> Self {
        Self {
            placer: Some(placer),
            ..self
        }
    }

    /// Records whether the last job listing of `host` was clean: it named only jobs the host
    /// holds a live allocation for.
    pub(crate) fn note_listing(&self, host: &HostId, clean: bool) {
        let mut unclean = self.unclean.lock().unwrap_or_else(PoisonError::into_inner);
        if clean {
            unclean.remove(host);
        } else {
            unclean.insert(host.clone());
        }
    }

    /// Whether the capacity of `host` may be counted by placement: its last job listing was
    /// clean (design section 6.2, section 8 row 37). A host that lists a job it holds no live
    /// allocation for is told to stop it and is not placed on until a listing is clean.
    pub(crate) fn listing_clean(&self, host: &HostId) -> bool {
        let unclean = self.unclean.lock().unwrap_or_else(PoisonError::into_inner);
        !unclean.contains(host)
    }

    /// Lets the hello of `host` register with the one-time token or host credential `who`
    /// (the digest of what the request carried), consuming the token on its first use, and
    /// commits `binding` (the host's binding, with the store) with the use in one
    /// transaction. With `issue`, that first use also retires the token for a new host
    /// credential (see [`JoinTokens::admit_writing`]). The check and the write run on the
    /// pool for blocking calls.
    pub(crate) async fn admit_join(
        self: &Arc<Self>,
        who: Credential,
        (host, instance): (String, Option<InstanceIdentity>),
        issue: bool,
        binding: Writes,
    ) -> Result<Admitted, JoinRefusal> {
        let this = Arc::clone(self);
        blocking(move || {
            let instance = instance.as_ref();
            this.joins
                .admit_writing(&who, &host, instance, issue, binding)
        })
        .await
    }

    /// Gives back the use that [`Agents::admit_join`] made for `host`, and the credential it
    /// issued, after the registry refused its hello, with `binding` (the undo of the host's
    /// binding, with the store) in the same transaction. True when that was written.
    pub(crate) async fn release_join(
        self: &Arc<Self>,
        who: Credential,
        host: String,
        binding: Writes,
    ) -> bool {
        let this = Arc::clone(self);
        blocking(move || this.joins.release_writing(&who, &host, binding)).await
    }

    /// The change of the binding of `host` that the registry makes when it admits `hello`
    /// with `credential`: the binding the registry holds for the host, which it keeps, or a
    /// new one to `credential` and the hello's instance. Its writes are empty in file mode,
    /// and when the store holds that binding already, as for a host the registry knows
    /// (unless the stored binding still holds a cordon that lapsed since the last tick).
    pub(crate) fn hello_change(
        &self,
        host: &HostId,
        hello: &Hello,
        credential: &Credential,
    ) -> Change {
        let held = self.registry.bindings().remove(host);
        let fresh = || Binding::new(credential.clone(), hello.instance.clone());
        self.store.change(host, held.unwrap_or_else(fresh))
    }

    /// Commits `writes` (bindings, with the store) in one transaction, on the pool for
    /// blocking calls; no store call when there are none, as in file mode.
    ///
    /// # Errors
    ///
    /// The store failed or refused the transaction: nothing was written.
    pub(crate) async fn commit_hosts(self: &Arc<Self>, writes: Writes) -> io::Result<()> {
        let this = Arc::clone(self);
        blocking(move || this.store.commit(writes)).await
    }

    /// Holds the binding of `change` as committed.
    pub(crate) fn applied(&self, change: &Change) {
        self.store.applied(change);
    }

    /// Holds the binding from before `change`, once its undo is committed.
    pub(crate) fn undone(&self, change: &Change) {
        self.store.undone(change);
    }

    /// Commits the binding of `host` as `make` changes it, when the registry holds one, and
    /// holds it as committed. The caller holds [`Agents::host_writes`] and has the registry
    /// take the change next. Nothing is written in file mode.
    async fn keep(
        self: &Arc<Self>,
        host: &HostId,
        make: impl FnOnce(Binding) -> Binding,
    ) -> io::Result<()> {
        let held = self.registry.bindings().remove(host);
        let change = held.map(|bound| self.store.change(host, make(bound)));
        let writes = change.as_ref().map(|change| change.writes.clone());
        self.commit_hosts(writes.unwrap_or_default()).await?;
        change.iter().for_each(|change| self.store.applied(change));
        Ok(())
    }

    /// Makes the taint `heartbeat` would record durable before the registry records it
    /// ([`Registry::check_heartbeat`]): the host's binding with the taint is committed under
    /// [`Agents::host_writes`], whose guard is returned for the caller to hold until the
    /// registry has recorded the beat. Nothing when the beat would record no taint, or the
    /// registry refuses it (the beat is then refused the same).
    ///
    /// # Errors
    ///
    /// The store failed or refused the commit: the taint is not recorded, and the caller
    /// answers the heartbeat with an error so that the agent reports it again.
    pub(crate) async fn durable_taint<'a>(
        self: &'a Arc<Self>,
        heartbeat: &Heartbeat,
        who: &Credential,
    ) -> io::Result<Option<tokio::sync::MutexGuard<'a, ()>>> {
        let Ok(check) = self.registry.check_heartbeat(heartbeat, who) else {
            return Ok(None);
        };
        let Some(taint) = check.taint else {
            return Ok(None);
        };
        let gate = self.host_writes.lock().await;
        self.keep(&check.host, |bound| bound.with_taint(Some(taint)))
            .await?;
        Ok(Some(gate))
    }

    /// Cordons `host` for `lease` (see [`Registry::cordon`]). With the store the cordon is
    /// committed first and the registry takes it only then; the end committed is read from
    /// the registry's clock a moment before the registry reads it, so the registry's end can
    /// be that moment later, which the next tick commits. In file mode the registry takes it
    /// first and the file is saved after.
    ///
    /// # Errors
    ///
    /// [`CordonFault::NotHeld`] for a host the registry does not hold;
    /// [`CordonFault::Unsaved`] when the commit failed (nothing changed), or in file mode when
    /// the save after it did (the cordon holds on this server).
    pub(crate) async fn cordon(
        self: &Arc<Self>,
        host: &HostId,
        lease: Duration,
    ) -> Result<Cordoned, CordonFault> {
        let gate = self.host_writes.lock().await;
        let wanted = self.clock.now().plus(lease).millis();
        let until = |bound: Binding| {
            let until = bound
                .cordon_until_ms()
                .map_or(wanted, |held| held.max(wanted));
            bound.with_cordon(Some(until))
        };
        self.keep(host, until).await.map_err(CordonFault::unsaved)?;
        let placed = self.registry.cordon(host, lease);
        drop(gate);
        let placed = placed.map_err(|_| CordonFault::NotHeld)?;
        self.saved().await?;
        Ok(placed)
    }

    /// Lifts the cordon of `host` (see [`Registry::uncordon`]), committed first with the
    /// store, as [`Agents::cordon`] places one.
    ///
    /// # Errors
    ///
    /// As [`Agents::cordon`].
    pub(crate) async fn uncordon(
        self: &Arc<Self>,
        host: &HostId,
    ) -> Result<Uncordoned, CordonFault> {
        let gate = self.host_writes.lock().await;
        let lifted = |bound: Binding| bound.with_cordon(None);
        self.keep(host, lifted)
            .await
            .map_err(CordonFault::unsaved)?;
        let lifted = self.registry.uncordon(host);
        drop(gate);
        let lifted = lifted.map_err(|_| CordonFault::NotHeld)?;
        self.saved().await?;
        Ok(lifted)
    }

    /// The save of the file after a cordon or its lift, as a fault when it failed. Nothing
    /// with the store, which committed the change before the registry took it.
    async fn saved(self: &Arc<Self>) -> Result<(), CordonFault> {
        if !self.store.in_file() {
            return Ok(());
        }
        match self.save_bindings().await {
            true => Ok(()),
            false => Err(CordonFault::Unsaved),
        }
    }

    /// Whether the one-time token `who` may heartbeat for `host`; a heartbeat it serves
    /// renews the token's credential. Runs on the pool for blocking calls, since a renewal
    /// writes the file (with the store it is left for the tick).
    pub(crate) async fn serve_join(
        self: &Arc<Self>,
        who: Credential,
        host: String,
    ) -> Result<(), JoinRefusal> {
        let this = Arc::clone(self);
        blocking(move || this.joins.serves(&who, &host)).await
    }

    /// Mints the one-time join token for the host `name` of the pool `pool`: the in-process
    /// form of `POST /v1/agent/join-tokens`, for the scaler that launches the machine. The
    /// token is bound to the instance `<pool>:<name>`, which is the line the launched
    /// agent's `--instance-file` holds, and lasts `ttl` from now (at most
    /// [`crate::join_tokens::MAX_TTL`]; a longer one is the caller's mistake, the route
    /// refuses it). A second mint for the same pool and name replaces the first unused
    /// token, so a caller mints once per launch and never again for a machine that may
    /// have booted. A blocking call, as it writes the token file or the store: run it on the
    /// pool for blocking calls.
    ///
    /// # Errors
    ///
    /// [`MintError`] as for the route.
    pub(crate) fn mint_for(
        &self,
        pool: &ProviderId,
        name: &HostName,
        ttl: Duration,
    ) -> Result<Minted, MintError> {
        self.joins.mint(Self::instance_of(pool, name), ttl)
    }

    /// Revokes every token and host credential bound to `<pool>:<name>`, used or not:
    /// the in-process form of `DELETE /v1/agent/join-tokens`. Returns how many rows were
    /// dropped; a name with none drops none and succeeds, so a repeat is a no-op. A blocking
    /// call, as [`Agents::mint_for`].
    ///
    /// # Errors
    ///
    /// [`RevokeError::Unsaved`] when the file or the store could not be written; nothing was
    /// dropped.
    pub(crate) fn revoke_for(
        &self,
        pool: &ProviderId,
        name: &HostName,
    ) -> Result<usize, RevokeError> {
        self.joins.revoke(&Self::instance_of(pool, name))
    }

    /// The instance a launched host names in its hello: the pool as the provider and the
    /// host's name as the instance id.
    fn instance_of(pool: &ProviderId, name: &HostName) -> InstanceIdentity {
        InstanceIdentity {
            provider: pool.to_string(),
            instance_id: name.to_string(),
        }
    }

    /// How many times the agent side could not read or write one of its files since the
    /// server started: the join token file that could not be read again, the file of
    /// bindings (or the store) that could not be written, and the credential times the tick
    /// could not commit.
    pub(crate) fn errors(&self) -> u64 {
        self.errors.load(Ordering::SeqCst)
    }

    /// Counts the job side's events with `meter` from now on. Set once: a second meter is
    /// ignored.
    pub(crate) fn set_meter(&self, meter: Meter) {
        let _ = self.meter.set(meter);
    }

    /// Where the job side's events are counted: [`Meter::off`] until [`Agents::set_meter`].
    pub(crate) fn meter(&self) -> &Meter {
        self.meter.get().unwrap_or(Meter::off())
    }

    /// What every tick does for the agent side: read the join tokens again and drop the
    /// hello limits of those that left the file, then read the job service's client token
    /// files again ([`AgentTokens::reload_clients`]), and make the file of bindings current
    /// (a write that failed earlier is tried again here; so is the removal of a host the
    /// registry has forgotten). A token file that cannot be read keeps the tokens in
    /// force, and their limits; a client token file that cannot be read, or holds no valid
    /// token, keeps every client's token in force, with one warning line per tick. The usage
    /// side, when there is one, is ticked next, and the host credentials of live logs last
    /// ([`LogGrants::tick`]: the departed hosts' are forgotten, the renewals and the retries
    /// queued and started, off this task).
    pub(crate) async fn refresh(self: &Arc<Self>) {
        let this = Arc::clone(self);
        match blocking(move || this.tokens.reload()).await {
            Ok(()) => self.limiter.retain(&self.tokens.credentials()),
            Err(err) => {
                self.errors.fetch_add(1, Ordering::SeqCst);
                warn!(error = %err, "cannot read the agent token file again; the tokens in force stay");
            }
        }
        let this = Arc::clone(self);
        match blocking(move || this.tokens.reload_clients()).await {
            Ok(true) => info!("the client tokens changed: the new tokens are in force"),
            Ok(false) => {}
            Err(error) => {
                warn!(%error, "cannot read the client token files again; the client tokens in force stay");
            }
        }
        self.save_bindings().await;
        // A due time that cannot be committed (only a store's commit fails here) is counted
        // with no arm of its own, so every test binary runs the same code of the tick.
        let this = Arc::clone(self);
        let flushed = blocking(move || this.joins.flush()).await;
        self.errors.fetch_add(u64::from(!flushed), Ordering::SeqCst);
        if let Some(usage) = &self.usage {
            usage.tick().await;
        }
        // A closure over the option, so every test binary runs the same code of the tick.
        let tick = |grants: &Arc<LogGrants>| grants.tick(&logs::uploading_hosts(&self.registry));
        self.logs.iter().for_each(tick);
    }

    /// Makes the file of bindings, or the store, hold what the registry has bound, on the
    /// pool for blocking calls, under [`Agents::host_writes`]: the write is synced to disk,
    /// or committed in one transaction. In file mode a host that was admitted stays
    /// admitted when this fails; the lease on it would then survive a restart without its
    /// binding, until the next tick writes it. With the store the server's own changes are
    /// committed before the registry holds them, so this writes only what the registry
    /// changed by itself (a host it forgot, a cordon that lapsed) or took without a commit.
    /// True when the file or the store holds what the registry has bound (a taint report is
    /// acknowledged only then).
    pub(crate) async fn save_bindings(self: &Arc<Self>) -> bool {
        let gate = self.host_writes.lock().await;
        let this = Arc::clone(self);
        let saved = blocking(move || this.store.save(|| this.registry.bindings())).await;
        drop(gate);
        if let Err(err) = &saved {
            self.errors.fetch_add(1, Ordering::SeqCst);
            error!(error = %err, "cannot write the host bindings");
        }
        saved.is_ok()
    }
}

#[path = "agents_taints.rs"]
mod taints;

#[cfg(test)]
#[path = "agents_mint_tests.rs"]
mod mint_tests;

#[cfg(test)]
#[path = "agents_estimate_tests.rs"]
mod estimate_tests;
