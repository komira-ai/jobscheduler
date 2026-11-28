//! The registering side of the server: the registry, the join tokens, the hello limit
//! and the bindings, and how they are put together at start.
//!
//! Without a store setting the join tokens and the bindings are kept in files beside the
//! state file; with one, in the state store ([`crate::host_keys`]), read at start, and one row
//! at a time later where another server over the store may have written it (a binding at the
//! hello of a host the registry does not know, a token at its first use here).
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use js_agent_wire::{Heartbeat, Hello, InstanceIdentity, Taint};
use js_elastic::HostName;
use js_registry::{Binding, Credential, OsSessions, Registry};
use js_state::Revision;
use js_types::{HostId, ProviderId};
use tracing::{error, info, warn};

pub(crate) use self::cordons::restore_cordons;
use self::taints::restore_job_taints;
use crate::agent_state::{self, BindingStore, Change};
use crate::agent_tokens::{AgentTokens, PoolTokens};
use crate::clock::SharedClock;
use crate::config::{AgentSetup, CpuSlack, placer, split};
use crate::dispatch_state::DispatchState;
use crate::drain::Drain;
use crate::error::StartupError;
use crate::held_beat::{self, Wakes};
use crate::host_keys::Writes;
use crate::job_expiry::JobLimits;
use crate::job_pass::SilenceFloor;
use crate::job_sizes::Sizes;
use crate::job_store::{self, FeedTrim, ServerJobs};
use crate::join_tokens::{Admitted, JoinRefusal, JoinTokens, MintError, Minted, RevokeError};
use crate::logs::{self, LogGrants};
use crate::metrics::Meter;
use crate::poller::Poller;
use crate::rate::HelloLimiter;
use crate::sched::place::SchedPlacer;
use crate::sched::rank::static_then;
use crate::sched::taint_skips::TaintSkips;
use crate::store::ServerStore;
use crate::usage_hub::Usage;
use crate::usage_map::limits_of;
use crate::usage_setup::UsageSetup;
use crate::util::blocking;

/// What the agent routes and the tick share. Agent handlers take the registry's lock and
/// nothing else of the server's.
pub(crate) struct Agents {
    /// The hosts of every pool ([`Registry::with_pools`]): the first pool's and the later ones'.
    pub(crate) registry: Registry,
    /// The first pool's shared join tokens.
    pub(crate) tokens: AgentTokens,
    /// The shared join tokens of each pool after the first, with its name; empty for a
    /// server of one pool.
    pub(crate) more_tokens: Vec<PoolTokens>,
    /// The one-time tokens minted for launched instances.
    pub(crate) joins: JoinTokens,
    /// The providers the mint route accepts, or none for any.
    pub(crate) mint_provider: BTreeSet<String>,
    pub(crate) limiter: HelloLimiter,
    /// The server's clock, which bounds a run by the time its lease has left.
    pub(crate) clock: SharedClock,
    pub(crate) store: BindingStore,
    /// The gate every change of the bindings and of the join tokens' uses goes through, held
    /// from the change's commit until the registry has taken it: a
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
    /// Whether the usage sink keeps its rows in the same state store as the job tables, so
    /// that the ended bookings it would list are the rows `job_state` lists already.
    pub(crate) usage_in_job_store: bool,
    /// The registry's placer, which also places the queued jobs when the job tables are
    /// held ([`Agents::place_jobs`]); `None` only for a test's agent side built without
    /// one, which places no job.
    pub(crate) placer: Option<Arc<SchedPlacer>>,
    /// The teaching of the job sizes the start began, which the server reads after it listens.
    pub(crate) sizes: Sizes,
    /// The store's rows the last pass offered hosts by, which the next read takes each row from
    /// while it stands at the version it had ([`crate::sched`]). Its lock is a leaf.
    pub(crate) rows_read: Mutex<Option<Arc<crate::sched::jobs_rows::RowsRead>>>,
    /// The host credentials and job sinks of live logs, with `sink = "object_store"`
    /// ([`crate::logs`]); `None` with `sink = "none"`. Its lock is a leaf, and no mint runs
    /// under it or under the registry's.
    pub(crate) logs: Option<Arc<LogGrants>>,
    /// The pool's limits and the server's `silence_limit`, which the job pass enforces.
    pub(crate) job_limits: JobLimits,
    /// Where the job pass counts a host's silence from ([`SilenceFloor`]).
    pub(crate) silence_floor: SilenceFloor,
    /// How far past its cores a host's CPU may be booked ([`crate::config::CpuSlack`]), which
    /// the capacity a placement is held to in its own transaction allows too.
    pub(crate) cpu_slack: CpuSlack,
    /// How long a claim of a periodic duty ([`js_jobs::JobStore::claim_duty`]) keeps the other
    /// servers off it, in milliseconds; zero claims every tick ([`Agents::set_duty_period`]).
    duty_period_ms: AtomicU64,
    /// Where the job side's events are counted ([`crate::metrics`]); set once, at start, when
    /// the server serves its metrics, and counting nothing until then.
    meter: OnceLock<Meter>,
    /// The gate of the server's pass over the queued jobs ([`Agents::place_jobs`]): one pass
    /// at a time, from its read of the open hosts to its last taint.
    pub(crate) placing: Mutex<()>,
    /// The placement poller's wake signal ([`crate::poller`]), which each submit pokes.
    pub(crate) poller: Poller,
    /// The heartbeats held open for a start ([`crate::held_beat`]), which every placement the
    /// pass commits wakes.
    pub(crate) wakes: Wakes,
    /// The dispatch poll's timings, held polls' wakes and claimer name
    /// ([`crate::dispatch_state`]); the pass wakes these beside `wakes`.
    pub(crate) dispatching: DispatchState,
    /// The drain of a stopping server ([`crate::drain`]): once it has begun, no pass places a
    /// job and no heartbeat's reply is held.
    pub(crate) draining: Drain,
    /// How long a heartbeat's reply with nothing to send is held ([`held_beat::hold_bound`]).
    pub(crate) hold_bound: Duration,
    /// How long such a reply is held when the beat lists a job as `starting`
    /// ([`held_beat::starting_hold`]).
    pub(crate) starting_hold: Duration,
    /// The jobs each host's held beats list as `starting`, and since when ([`held_beat::Starts`]).
    pub(crate) starts: held_beat::Starts,
    /// The machine providers of the pool's Default platform, first to last, as the bindings of
    /// its hosts name them: the order the pass offers the hosts in ([`crate::sched::rank`]).
    pub(crate) providers: Vec<ProviderId>,
    /// The live hosts the pass over the queued jobs has said it skips for their taint
    /// ([`TaintSkips`]), so that each taint is logged once.
    pub(crate) taint_skips: TaintSkips,
    /// The cluster's own addresses, which no job's completion callback may reach.
    pub(crate) cluster: js_callback::AddressRule,
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

/// Where a file of the agent side is kept: beside the state file at `state_file`, with
/// `suffix` added to its name.
pub(crate) fn state_path(state_file: &Path, suffix: &str) -> PathBuf {
    let mut name = state_file.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    state_file.with_file_name(name)
}

/// What [`assemble`] returns: the agent side.
pub(crate) type Assembled = Arc<Agents>;

/// The agent side: a registry on `clock` over the pool of `agents`, seeded with the
/// bindings the previous run persisted. It mints the host credentials of live logs with
/// `logs` ([`crate::logs`]).
#[cfg(test)]
pub(crate) fn assemble(
    agents: Box<AgentSetup>,
    usage: UsageSetup,
    clock: &SharedClock,
    state_file: &Path,
    logs: Option<Arc<LogGrants>>,
) -> Result<Assembled, StartupError> {
    assemble_in(agents, usage, clock, (state_file, None), logs)
}

/// [`assemble`], with the join tokens and the bindings kept in the state store `kept` when
/// there is one, instead of the files beside the state file.
pub(crate) fn assemble_in(
    agents: Box<AgentSetup>,
    usage: UsageSetup,
    clock: &SharedClock,
    (state_file, kept): (&Path, Option<&ServerStore>),
    logs: Option<Arc<LogGrants>>,
) -> Result<Assembled, StartupError> {
    let placed = placed_in(agents, usage, clock, (state_file, kept), logs);
    placed.map(|(assembled, _)| assembled)
}

/// [`assemble`], with the placer the registry was given and the sizes taught.
#[cfg(test)]
pub(crate) fn assemble_placed(
    agents: Box<AgentSetup>,
    usage: UsageSetup,
    clock: &SharedClock,
    state_file: &Path,
    logs: Option<Arc<LogGrants>>,
) -> Result<(Assembled, Arc<SchedPlacer>), StartupError> {
    let placed = placed_in(agents, usage, clock, (state_file, None), logs)?;
    let _ = placed.0.sizes.teach();
    Ok(placed)
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
    let (store, bindings) = BindingStore::in_store(kept.store(), clock.clone()).map_err(refused)?;
    Ok((joins, bindings, store))
}

/// [`assemble_in`], with the placer the registry was given.
fn placed_in(
    agents: Box<AgentSetup>,
    usage: UsageSetup,
    clock: &SharedClock,
    (state_file, kept): (&Path, Option<&ServerStore>),
    logs: Option<Arc<LogGrants>>,
) -> Result<(Assembled, Arc<SchedPlacer>), StartupError> {
    let AgentSetup {
        tokens,
        pool,
        more,
        mint_provider,
        hello_limit,
        require_host_credential,
        load,
        cold,
        cold_on,
        curves,
        backfill,
        job_limits,
        jobs,
        job_sources,
        providers,
        dispatch,
    } = *agents;
    // An orphan is ended once listed for longer than any job may run.
    let job_limits = job_limits.with_orphan_after(backfill.max_job_wall);
    let (joins, mut bindings, store) = open_hosts(clock, (state_file, kept))?;
    let joins = joins.requiring_credential(require_host_credential);
    // The usage limits hold the largest host of every pool.
    let shapes = std::iter::once(&pool).chain(more.iter().map(|more| &more.pool));
    let largest = shapes
        .map(|pool| pool.shape().resources())
        .fold((0, 0), |(cpu, memory), r| {
            (cpu.max(r.cpu_millis()), memory.max(r.memory_bytes()))
        });
    let limits = limits_of(largest.0, largest.1);
    // The scheduler picks the host of a start, and books the share of each host's
    // memory the pool's percent says (every pool's is the first's: `check_more`); the
    // registry books each host's whole booked memory.
    let (pool, memory) = split(pool);
    let placer = placer(backfill, load, (memory, cold, curves), cold_on, clock);
    let (later, more_tokens): (Vec<_>, Vec<_>) = more
        .into_iter()
        .map(|more| (split(more.pool).0, more.tokens))
        .unzip();
    let pools = (pool, later);
    let registry = Registry::with_pools(pools, clock.clone(), OsSessions, placer.clone())
        .map_err(|err| StartupError::new("--agent-pool-config", err))?;
    let interval = registry.heartbeat_interval();
    let providers = providers.unwrap_or_else(|| static_then(&registry.pool_id()));
    // The one job store of the server, when the job service is on. It is opened
    // before the bindings are put back: a host a host job tainted whose binding lost
    // the taint (a crash between the placement and the binding's write) gets it from
    // the job store's row first. Its commit failing refuses the start, as a store
    // that cannot be read does.
    let restore_flag = jobs.as_ref().map(|setup| setup.flag);
    let kept_store = kept.map(ServerStore::store);
    // The sizes' teaching is begun, not read: the server listens before its read of the ended
    // jobs (`crate::job_sizes`).
    let (jobs, job_feed, job_state, sizes) = match jobs {
        Some(setup) => {
            let opened = job_store::open((state_file, kept_store.clone()), setup, &job_sources)?;
            restore_job_taints(&opened.jobs, &store, &mut bindings, setup.flag)?;
            let sizes = crate::job_sizes::begin(&placer, &opened.jobs);
            let (jobs, feed, state) = (opened.jobs, opened.feed, opened.store);
            (Some(jobs), Some(feed), Some(state), sizes)
        }
        None => (None, None, None, Sizes::default()),
    };
    // After the job store: with the usage rows in its store, the prune's deletes are claimed.
    let (usage, in_store) = usage.build(clock, limits, (kept_store, jobs.as_ref()))?;
    for (host, binding) in &bindings {
        taints::put_back(&registry, host, binding, "start");
    }
    // A cordon a binding brought back is written into the job store, which keeps the cordons
    // from then on (`agents_cordon`), so the first tick's mirror does not lift it.
    if let Some((jobs, flag)) = jobs.as_ref().zip(restore_flag) {
        restore_cordons(jobs, &registry, flag)?;
    }
    let agents = Agents {
        registry,
        tokens,
        more_tokens,
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
        usage_in_job_store: in_store,
        placer: Some(placer.clone()),
        sizes,
        rows_read: Mutex::default(),
        logs,
        job_limits,
        silence_floor: SilenceFloor::default(),
        cpu_slack: load.cpu_slack,
        duty_period_ms: AtomicU64::new(0),
        meter: OnceLock::new(),
        placing: Mutex::default(),
        poller: Poller::default(),
        wakes: Wakes::default(),
        dispatching: DispatchState::new(dispatch),
        draining: Drain::default(),
        hold_bound: held_beat::hold_bound(interval),
        starting_hold: held_beat::starting_hold(interval),
        starts: held_beat::Starts::default(),
        providers,
        taint_skips: TaintSkips::default(),
        cluster: js_callback::AddressRule::new([]),
    };
    Ok((Arc::new(agents), placer))
}

impl Agents {
    /// Makes the tick's periodic duties (the job pass, the feed's trim) run by one server in
    /// `period`: each tick claims a duty first, and a duty another server claimed within `period`
    /// is left to it. Zero, which is the start, claims every tick.
    pub(crate) fn set_duty_period(&self, period: Duration) {
        let millis = u64::try_from(period.as_millis()).unwrap_or(u64::MAX);
        self.duty_period_ms.store(millis, Ordering::SeqCst);
    }

    /// How long a claim of a periodic duty keeps the other servers off it.
    pub(crate) fn duty_period(&self) -> Duration {
        Duration::from_millis(self.duty_period_ms.load(Ordering::SeqCst))
    }

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
        let providers = static_then(&registry.pool_id());
        let hold_bound = held_beat::hold_bound(registry.heartbeat_interval());
        let starting_hold = held_beat::starting_hold(registry.heartbeat_interval());
        Self {
            registry,
            tokens: AgentTokens::open(&tokens, &client).unwrap(),
            more_tokens: Vec::new(),
            joins,
            mint_provider: BTreeSet::new(),
            limiter: HelloLimiter::new(limit, clock.clone()),
            clock: clock.clone(),
            store: BindingStore::new(dir.join("bindings"), Default::default()),
            host_writes: tokio::sync::Mutex::new(()),
            errors: AtomicU64::new(0),
            usage: None,
            jobs: None,
            job_feed: None,
            job_state: None,
            usage_in_job_store: false,
            placer: None,
            sizes: Sizes::default(),
            rows_read: Mutex::default(),
            logs: None,
            job_limits: JobLimits::default(),
            silence_floor: SilenceFloor::default(),
            cpu_slack: CpuSlack::default(),
            duty_period_ms: AtomicU64::new(0),
            meter: OnceLock::new(),
            placing: Mutex::default(),
            poller: Poller::default(),
            wakes: Wakes::default(),
            dispatching: DispatchState::new(Default::default()),
            draining: Drain::default(),
            hold_bound,
            starting_hold,
            starts: held_beat::Starts::default(),
            providers,
            taint_skips: TaintSkips::default(),
            cluster: js_callback::AddressRule::new([]),
        }
    }

    /// This agent side with its join tokens and bindings kept in `store` (empty when it is
    /// read here), as a server with a store setting keeps them, for a test of their commits.
    #[cfg(test)]
    pub(crate) fn kept_in(self, store: Arc<dyn js_state::Store>) -> Self {
        let random = Box::new(js_registry::SequentialSessions::new());
        let joins = JoinTokens::in_store(Arc::clone(&store), self.clock.clone(), random);
        let (bound, _) = BindingStore::in_store(store, self.clock.clone()).unwrap();
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

    /// This agent side placing with `placer` (`--placement js-sched`), for a test of the
    /// placement pass over the queued jobs. The registry must already hold it.
    #[cfg(test)]
    pub(crate) fn with_placer(self, placer: Arc<SchedPlacer>) -> Self {
        Self {
            placer: Some(placer),
            ..self
        }
    }

    /// Whether the capacity of `host` may be counted by placement: its last job listing was
    /// clean (design section 6.2, section 8 row 37), as its row holds it. True without the job
    /// tables and for a host with no row; false for one whose row cannot be read.
    #[cfg(test)]
    pub(crate) fn listing_clean(&self, host: &HostId) -> bool {
        let Some(jobs) = &self.jobs else {
            return true;
        };
        let row = jobs.read_node(host);
        row.is_ok_and(|row| row.is_none_or(|row| row.record.state.last_listing_clean))
    }

    /// Lets the hello of `host` register with the one-time token or host credential `who`
    /// (the digest of what the request carried), consuming the token on its first use, and
    /// commits `binding` (the host's binding, with the store) with the use in one
    /// transaction. With `issue`, that first use also retires the token for a new host
    /// credential (see [`JoinTokens::admit_writing`]). The check and the write run on the
    /// pool for blocking calls. The revision of the commit comes with the answer: the version of
    /// the binding it wrote ([`JoinTokens::admit_writing`]).
    pub(crate) async fn admit_join(
        self: &Arc<Self>,
        who: Credential,
        (host, instance): (String, Option<InstanceIdentity>),
        issue: bool,
        binding: Writes,
    ) -> Result<(Admitted, Option<Revision>), JoinRefusal> {
        let this = Arc::clone(self);
        blocking(move || {
            let instance = instance.as_ref();
            this.joins
                .admit_writing(&who, &host, instance, issue, binding)
        })
        .await
    }

    /// Reads the binding row of `host` before its hello is judged (or its binding is written),
    /// holds it with its version as what the store holds ([`BindingStore::stored`]), and puts
    /// what it holds into the registry ([`taints::take_in`]): the taint or cordon of a host the
    /// registry does not know (the start of this server put back what the store held then, and
    /// this does the same for what the store holds now, so a host that comes from another
    /// server over the same store keeps the taint that server stored after this one started,
    /// and takes no job here that its agent would refuse), and the taint another server
    /// stored for a host the registry knows and holds none for, which a write made on what the
    /// registry holds would otherwise overwrite. `when` names the caller in the log line of
    /// what is put back. Nothing in file mode.
    ///
    /// # Errors
    ///
    /// The store could not be read: the hello is refused with an error the agent retries,
    /// and nothing is judged on less than the store holds.
    pub(crate) async fn restore_stored(
        self: &Arc<Self>,
        host: &HostId,
        when: &'static str,
    ) -> io::Result<()> {
        let this = Arc::clone(self);
        let named = host.clone();
        let stored = blocking(move || this.store.stored(&named)).await?;
        if let Some(binding) = stored {
            taints::take_in(&self.registry, host, &binding, when);
        }
        Ok(())
    }

    /// The change of the binding of `host` that the registry makes when it admits `hello`
    /// with `credential`; `certified` says the hello presented a host certificate. It is the
    /// binding the registry holds for the host, which it keeps, or a new one to `credential`
    /// and the hello's instance. Its writes are empty in file mode, and when the store holds
    /// that binding already, as for a host the registry knows (unless the stored binding still
    /// holds a cordon that lapsed since the last tick); otherwise the write is conditional on
    /// the version of the row [`Agents::restore_stored`] read: the hello is judged on that row,
    /// and a row another server wrote since refuses the write.
    pub(crate) fn hello_change(
        &self,
        host: &HostId,
        hello: &Hello,
        (credential, certified): (&Credential, bool),
    ) -> Change {
        let held = self.registry.bindings().remove(host);
        let fresh = || Binding::new(credential.clone(), hello.instance.clone());
        // A hello with a host certificate moves a host from the credential it was bound to
        // onto the certificate's, keeping its taint and cordon (the registry decides the same,
        // [`Registry::hello_as_certified_host`], and has judged this hello before the change
        // is made: a live host that is not the same process is refused there, so a hello that
        // gets here moves the binding the registry moves).
        let moved = |bound: &Binding| {
            let taint = bound.taint().cloned();
            fresh()
                .with_taint(taint)
                .with_cordon(bound.cordon_until_ms())
        };
        let bound = match held {
            Some(bound) if certified && bound.credential() != credential => moved(&bound),
            Some(bound) => bound,
            None => fresh(),
        };
        self.store.change(host, bound)
    }

    /// Commits `writes` (bindings, with the store) in one transaction, on the pool for
    /// blocking calls; no store call when there are none, as in file mode. The revision of the
    /// commit is the version of the binding it wrote, when it wrote one.
    ///
    /// # Errors
    ///
    /// The store failed or refused the transaction (a binding another server wrote since it
    /// was read): nothing was written.
    pub(crate) async fn commit_hosts(
        self: &Arc<Self>,
        writes: Writes,
    ) -> io::Result<Option<Revision>> {
        let this = Arc::clone(self);
        blocking(move || this.store.commit(writes)).await
    }

    /// Holds the binding of `change` as committed at `revision`.
    pub(crate) fn applied(&self, change: &Change, revision: Option<Revision>) {
        self.store.applied(change, revision);
    }

    /// Commits the binding of `host` as `make` changes it, when the registry holds one, and
    /// holds it as committed. The row is read first ([`Agents::restore_stored`]), so the write
    /// is made at the version of the row as it stands and over a taint another server stored
    /// the registry has taken; a row written in between refuses the write. The caller holds
    /// [`Agents::host_writes`] and has the registry take the change next. Nothing is written in
    /// file mode.
    async fn keep(
        self: &Arc<Self>,
        host: &HostId,
        make: impl FnOnce(Binding) -> Binding,
    ) -> io::Result<()> {
        self.restore_stored(host, "write").await?;
        let held = self.registry.bindings().remove(host);
        let change = held.map(|bound| self.store.change(host, make(bound)));
        let writes = change.as_ref().map(|change| change.writes.clone());
        let revision = self.commit_hosts(writes.unwrap_or_default()).await?;
        let applied = |change: &Change| self.store.applied(change, revision);
        change.iter().for_each(applied);
        Ok(())
    }

    /// Makes the taint `heartbeat` would record, or the clearing of the host's taint it
    /// reports or implies, durable before the registry records it
    /// ([`Registry::check_heartbeat`]): the host's binding with the taint (none for a clear)
    /// is committed under [`Agents::host_writes`], whose guard is returned for the caller to
    /// hold until the registry has recorded the beat. Nothing when the beat would record no
    /// taint and clear none, or the registry refuses it (the beat is then refused the same).
    /// A clear a beat implies is its saying the agent holds no taint (`tainted` false) while
    /// the host's taint is one the agent made: it is committed as a reported clear is, and
    /// logged with `by="agent-holds-none"` ([`Agents::log_agent_holds_none`]), where a
    /// reported clear is logged with `by="agent"`.
    ///
    /// A beat that reports cleared a taint the registry does not hold, on a host the registry
    /// holds no taint for, ends the taint the store holds when it is that one, or the one a
    /// refused start made (reason `host-tainted`, which the agent never reports)
    /// ([`Agents::end_stored_taint`]): another server over the same store (the two of a
    /// start-first update) was told the taint and stored it, and the agent, told that its
    /// clear was taken, never reports it again.
    ///
    /// # Errors
    ///
    /// The store failed or refused the commit: the taint is not recorded, and the caller
    /// answers the heartbeat with an error so that the agent reports it again. The commit is
    /// conditional on the taint the registry holds ([`Agents::keep_taint`]), so a taint another
    /// server stored or ended since the registry took the host would fail every later beat the
    /// same way; before the error is returned the registry takes the taint the store holds
    /// ([`Agents::resync_taint`]), and the agent's repeat is judged on that.
    pub(crate) async fn durable_taint<'a>(
        self: &'a Arc<Self>,
        heartbeat: &Heartbeat,
        who: &Credential,
    ) -> io::Result<Option<tokio::sync::MutexGuard<'a, ()>>> {
        let Ok(check) = self.registry.check_heartbeat(heartbeat, who) else {
            return Ok(None);
        };
        // A clear the registry does not take, on a host it holds untainted. The host is read
        // only for such a beat, and outside `host_writes`: a taint stored in between is kept by
        // the store's conditional write, which expects the taint it read.
        let stray = heartbeat.taint_cleared.as_ref();
        let stray = stray.filter(|_| !check.cleared & check.taint.is_none());
        let untainted = |host: &HostId| {
            let own = self.registry.own_host(host);
            own.is_some_and(|own| own.host.taint.is_none())
        };
        let stray = stray.filter(|_| untainted(&check.host));
        if let Some(cleared) = stray {
            let gate = self.host_writes.lock().await;
            self.end_stored_taint(&check.host, cleared).await?;
            return Ok(Some(gate));
        }
        if check.taint.is_none() && !check.cleared {
            return Ok(None);
        }
        let gate = self.host_writes.lock().await;
        if let Err(error) = self.keep_taint(&check.host, check.taint.clone()).await {
            self.resync_taint(&check.host).await;
            return Err(error);
        }
        if let Some(cleared) = heartbeat.taint_cleared.as_ref().filter(|_| check.cleared) {
            taints::log_cleared(&check.host, cleared, "agent");
        }
        if check.cleared && heartbeat.taint_cleared.is_none() {
            self.log_agent_holds_none(&check.host);
        }
        if let Some(taint) = &check.taint {
            taints::log_stored(&check.host, taint);
        }
        Ok(Some(gate))
    }

    /// Ends the taint the store holds for `host` when it is `cleared` or a refused start's
    /// ([`BindingStore::end_taint`], whose conditional write never erases a taint another
    /// server stored in between). The registry holds no taint for the host, so nothing changes
    /// in memory. The caller holds [`Agents::host_writes`].
    async fn end_stored_taint(self: &Arc<Self>, host: &HostId, cleared: &Taint) -> io::Result<()> {
        let this = Arc::clone(self);
        let reason = cleared.reason.clone();
        let (named, cleared) = (host.clone(), cleared.clone());
        let ended = blocking(move || this.store.end_taint(&named, &cleared)).await?;
        if ended.is_some() {
            info!(%host, reason, "the agent's clear ended the taint another server stored");
        }
        Ok(())
    }

    /// Commits the binding of `host` with `taint` (none for a clear), when the registry holds
    /// one, as [`Agents::keep`] does, but only if the store still holds the taint the registry
    /// holds ([`BindingStore::commit_taint`]): a taint another server reported or cleared is
    /// refused, not overwritten. The caller holds [`Agents::host_writes`].
    async fn keep_taint(self: &Arc<Self>, host: &HostId, taint: Option<Taint>) -> io::Result<()> {
        let held = self.registry.bindings().remove(host);
        let expected = held.as_ref().and_then(|bound| bound.taint().cloned());
        let change = held.map(|bound| self.store.change(host, bound.with_taint(taint)));
        let this = Arc::clone(self);
        let (change, revision) = blocking(move || {
            let commit = |change| this.store.commit_taint(change, expected.as_ref());
            let revision = change.as_ref().map_or(Ok(None), commit)?;
            Ok::<_, io::Error>((change, revision))
        })
        .await?;
        let applied = |change: &Change| self.store.applied(change, revision);
        change.iter().for_each(applied);
        Ok(())
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

    /// The shared join tokens of every pool, the first pool's first.
    fn every_tokens(&self) -> impl Iterator<Item = &AgentTokens> {
        let more = self.more_tokens.iter().map(|more| &more.tokens);
        std::iter::once(&self.tokens).chain(more)
    }

    /// The pool a shared join token `given` belongs to, with the credential a host admitted
    /// with it is bound to: the first pool's name for one of its tokens, a later pool's for
    /// one of that pool's.
    pub(crate) fn identify_pooled(&self, given: &str) -> Option<(Credential, String)> {
        let first = self.tokens.identify(given);
        let first = first.map(|credential| (credential, self.registry.pool_id().to_string()));
        let later = || {
            self.more_tokens.iter().find_map(|more| {
                let credential = more.tokens.identify(given)?;
                Some((credential, more.pool.clone()))
            })
        };
        first.or_else(later)
    }

    /// How many times the agent side could not read or write one of its files since the
    /// server started: the join token file that could not be read again, the file of
    /// bindings (or the store) that could not be written, and the credential times the tick
    /// could not commit.
    pub(crate) fn errors(&self) -> u64 {
        self.errors.load(Ordering::SeqCst)
    }

    /// Counts the job side's events with `meter` from now on. Set once: a second meter is ignored.
    pub(crate) fn set_meter(&self, meter: Meter) {
        let _ = self.meter.set(meter);
    }

    /// Where the job side's events are counted: [`Meter::off`] until [`Agents::set_meter`].
    pub(crate) fn meter(&self) -> &Meter {
        self.meter.get().unwrap_or(Meter::off())
    }

    /// What every tick does for the agent side: read the join tokens again and drop the
    /// hello limits of those that left the file once they are refilled (a limit that is still
    /// spent stays, whoever it is of, so a certificate host's is not reset), then read the job service's client token
    /// files again ([`AgentTokens::reload_clients`]), put the job store's cordons on the
    /// registry ([`Agents::mirror_cordons`]) and its scale-down marks
    /// ([`Agents::mirror_host_marks`]), and make the file of bindings current
    /// (a write that failed earlier is tried again here; so is the removal of a host the
    /// registry has forgotten). A token file that cannot be read keeps the tokens in
    /// force, and their limits; a client token file that cannot be read, or holds no valid
    /// token, keeps every client's token in force, with one warning line per tick. The usage
    /// side, when there is one, is ticked next, and the host credentials of live logs last
    /// ([`LogGrants::tick`]: the departed hosts' are forgotten, the renewals and the retries
    /// queued and started, off this task).
    pub(crate) async fn refresh(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let read = blocking(move || this.every_tokens().map(AgentTokens::reload).collect()).await;
        let read: Vec<Result<(), StartupError>> = read;
        for err in read.iter().filter_map(|read| read.as_ref().err()) {
            self.errors.fetch_add(1, Ordering::SeqCst);
            warn!(error = %err, "cannot read the agent token file again; the tokens in force stay");
        }
        // A set that could not be read keeps its tokens, and their limits.
        let credentials = self.every_tokens().flat_map(AgentTokens::credentials);
        self.limiter.retain(&credentials.collect::<Vec<_>>());
        let this = Arc::clone(self);
        let reload = move || {
            let beside: Vec<&AgentTokens> =
                this.more_tokens.iter().map(|more| &more.tokens).collect();
            this.tokens.reload_clients(&beside)
        };
        match blocking(reload).await {
            Ok(true) => info!("the client tokens changed: the new tokens are in force"),
            Ok(false) => {}
            Err(error) => {
                warn!(%error, "cannot read the client token files again; the client tokens in force stay");
            }
        }
        self.mirror_cordons().await;
        self.mirror_host_marks().await;
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
    /// Whether a server may still serve `host` by its session row (`node/<host>`): the row was
    /// heard, by the store's clock, within the grace and lost retention of its pool
    /// ([`js_registry::Registry::kept_for`]), the window after which a registry forgets a
    /// silent host. Then its binding is not deleted because this server's registry let go of it
    /// (it heard the host last when it took its session from the row; another server has
    /// heard it since). A row that cannot be read, or of a pool this server does not have,
    /// counts as served: a binding is never deleted on less than the store holds. False
    /// without the job tables and for a host with no row.
    fn served(&self, host: &HostId) -> bool {
        let Some(jobs) = &self.jobs else {
            return false;
        };
        let heard = |row: js_jobs::NodeRow| {
            let kept = self.registry.kept_for(&row.record.pool);
            let now = jobs.now().ok();
            let silent = now.map(|now| now.since(row.record.heard_at));
            kept.zip(silent).is_none_or(|(kept, silent)| silent <= kept)
        };
        jobs.read_node(host)
            .map_or(true, |row| row.is_some_and(heard))
    }

    pub(crate) async fn save_bindings(self: &Arc<Self>) -> bool {
        let gate = self.host_writes.lock().await;
        let this = Arc::clone(self);
        let saved = blocking(move || {
            let served = |host: &HostId| this.served(host);
            this.store.save(|| this.registry.bindings(), &served)
        })
        .await;
        // A row another server wrote since this one read it was left as it is: the taint it
        // holds is the registry's from here on, and the binding is written over it by the next
        // save, or not at all when the two agree.
        let lost = saved.iter().flatten();
        lost.for_each(|(host, binding)| taints::take_in(&self.registry, host, binding, "save"));
        drop(gate);
        if let Err(err) = &saved {
            self.errors.fetch_add(1, Ordering::SeqCst);
            error!(error = %err, "cannot write the host bindings");
        }
        saved.is_ok_and(|lost| lost.is_empty())
    }
}

#[path = "agents_cordon.rs"]
mod cordons;

#[path = "agents_taints.rs"]
pub(crate) mod taints;

#[path = "agents_retire.rs"]
pub(crate) mod retire;

#[cfg(test)]
#[path = "agents_mint_tests.rs"]
mod mint_tests;

#[cfg(test)]
#[path = "agents_estimate_tests.rs"]
mod estimate_tests;

#[cfg(test)]
#[path = "agents_pools_tests.rs"]
mod pools_tests;
