//! What the configuration decided for the agents that register hosts: [`AgentSetup`], the
//! clock and id source its job store is opened on ([`Config::with_job_sources`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_registry::PoolConfig;
use js_sched_placer::sizes::{ColdCpuOn, ColdMemoryOn, ColdSize};
use js_types::ProviderId;

use super::{
    Cli, ColdOn, Config, CurveSetting, DispatchTimes, LiveClients, LoadSetting, check_pool, listen,
    retired,
};
use crate::agent_pool;
use crate::error::StartupError;
use crate::job_expiry::JobLimits;
use crate::job_store::{JobSources, JobStoreSetup, ServerIds};
use crate::rate::HelloLimit;

/// What `--agent-pool-config` and the hello limit decided.
pub(crate) struct AgentSetup {
    /// The clients of the job service in force, whose token files each tick reads again.
    pub(crate) clients: LiveClients,
    /// The first pool: the one the registry is named for, whose whole-node settings and
    /// timing of restored hosts are the server's.
    pub(crate) pool: PoolConfig,
    /// The pools after the first, in the order given: a later `--agent-pool-config`, or a
    /// later joined `[[pool]]`.
    pub(crate) more: Vec<MorePool>,
    pub(crate) hello_limit: HelloLimit,
    /// `--cpu-slack` and `--pack-limit`, each or the engine's default.
    pub(crate) load: LoadSetting,
    /// `--cold-cpu-millis` and `--cold-memory-bytes`, or `server.cold_cpu_millis` and
    /// `server.cold_memory_bytes`: what a job that books nothing books while neither its class,
    /// its fallback class nor its client has a run (each absent one its default,
    /// 22 cores and 4 GiB).
    pub(crate) cold: ColdSize,
    /// `--cold-memory-bytes-on` and `--cold-cpu-millis-on`, or `server.cold_memory_bytes_on`
    /// and `server.cold_cpu_millis_on`: the cold memory and the cold CPU of the jobs of each
    /// operating system they name, in place of `cold`'s.
    pub(crate) cold_on: ColdOn,
    /// `--placement-curves`, or `server.placement_curves`: whether the engine learns profiles
    /// and books by them, with `--curve-hosts` and `--curve-max-wait-seconds` (or
    /// `server.curve_hosts` and `server.curve_max_wait_seconds`) for the mode `on`.
    pub(crate) curves: CurveSetting,
    /// `server.max_lease_wall`: the one up to which the placement engine trusts a wall time
    /// for backfill.
    pub(crate) max_lease_wall: Option<Duration>,
    /// The pool's `max_queue_wait` and `start_timeout`, which the tick's job pass enforces
    /// (their defaults for a pool that does not set them, and for `--agent-pool-config`).
    pub(crate) job_limits: JobLimits,
    /// The job store's setup when the job service is on; the agent side then opens it.
    pub(crate) jobs: Option<JobStoreSetup>,
    /// The clock and id source the job store is opened on: the system's, unless
    /// [`crate::Config::with_job_sources`] gave others.
    pub(crate) job_sources: JobSources,
    /// The machine providers of the pool's Default platform, first to last, as the bindings of
    /// its hosts name them ([`crate::sched::rank::order_of`]); `None` when the configuration
    /// names no platform for the pool, and the order is `static`, then the pool's launched
    /// hosts ([`crate::sched::rank::static_then`]).
    pub(crate) providers: Option<Vec<ProviderId>>,
    /// `server.dispatch_lease_secs` and `server.dispatch_recheck_ms`, or their defaults: what
    /// the dispatch poll is timed by.
    pub(crate) dispatch: DispatchTimes,
}

/// A pool after the first: what registers into it.
pub(crate) struct MorePool {
    pub(crate) pool: PoolConfig,
}

/// Refuses `pool`, which `place` names, as a pool after `first` and `before`: its name is
/// one of theirs (a hello could not say which it joins); its `memory_schedulable_percent` is
/// not the first pool's (the placement engine books one share of every host's memory); or it
/// has whole-node settings (the server's are the first pool's, so a later pool's would be
/// read by nothing: its class must not be `whole-node`).
pub(crate) fn check_more(
    first: &PoolConfig,
    before: &[MorePool],
    (pool, place): (&PoolConfig, &str),
) -> Result<(), StartupError> {
    let mut earlier = std::iter::once(first).chain(before.iter().map(|more| &more.pool));
    if earlier.any(|earlier| earlier.name() == pool.name()) {
        return Err(StartupError::at(
            place,
            "name: also the name of an earlier pool",
        ));
    }
    let percent = first.memory_percent().get();
    if pool.memory_percent().get() != percent {
        let reason = format!(
            "memory_schedulable_percent must be the first pool's ({percent}): the placement engine books one share of every host's memory"
        );
        return Err(StartupError::at(place, reason));
    }
    if pool.whole_node().is_some() {
        let reason = "whole-node settings are the first pool's alone: give this pool a class that is not whole-node, and no whole-node keys";
        return Err(StartupError::at(place, reason));
    }
    Ok(())
}

impl Config {
    /// The same configuration with the job store (when the job service is on) stamping its
    /// rows with `clock` and minting its ids from `ids`, in place of the system's clock and
    /// random source: how a simulation makes its job and allocation ids, and the times it
    /// stores, a function of its seed and its virtual clock. Without the job service there
    /// is no job store, and the sources go unused.
    pub fn with_job_sources(
        mut self,
        clock: Arc<dyn js_state::Clock>,
        ids: Arc<dyn js_jobs::IdSource>,
    ) -> Self {
        if let Some(store) = self.store.as_mut() {
            store.clock = crate::config::StoreClock(Some(Arc::clone(&clock)));
        }
        self.agents.job_sources.clock = clock;
        self.agents.job_sources.ids = ServerIds::new(ids);
        self
    }
}

/// The registering side the flags name: `--agent-pool-config` is required (a server serves
/// the hosts that run `js-agent` and register). Each further `--agent-pool-config` is one
/// more pool. `--agent-token-file`, `--require-host-credential` and a pool file's
/// `join_token_file` are accepted and not read: each given one leaves a line in `warnings`.
pub(super) fn agents(
    cli: &Cli,
    clients: &LiveClients,
    warnings: &mut Vec<String>,
) -> Result<Box<AgentSetup>, StartupError> {
    let Some((first, later)) = cli.agent_pool_config.split_first() else {
        return Err(StartupError::new(
            "--agent-pool-config",
            "required: a server serves the hosts that run js-agent and register",
        ));
    };
    let place = |path: &Path| format!("--agent-pool-config {}", path.display());
    let pool = read_pool(first, warnings)?;
    check_pool(&pool, "--agent-pool-config")?;
    let mut more = Vec::new();
    for path in later {
        let next = read_pool(path, warnings)?;
        check_pool(&next, &place(path))?;
        check_more(&pool, &more, (&next, &place(path)))?;
        more.push(MorePool { pool: next });
    }
    let load = LoadSetting {
        cpu_slack: cli.cpu_slack.unwrap_or_default(),
        pack_limit: cli.pack_limit.unwrap_or_default(),
    };
    let cold_on = cold_on_of(cli)?;
    Ok(Box::new(AgentSetup {
        clients: clients.clone(),
        pool,
        more,
        hello_limit: HelloLimit {
            burst: cli.agent_hello_burst,
            interval: cli.agent_hello_interval,
        },
        load,
        cold: ColdSize {
            cpu_millis: cli.cold_cpu_millis,
            memory_bytes: cli.cold_memory_bytes,
        },
        cold_on,
        curves: super::placement_curves::of_flags(cli)?,
        max_lease_wall: None,
        job_limits: JobLimits::default(),
        jobs: listen::job_store(cli),
        job_sources: JobSources::default(),
        providers: None,
        dispatch: DispatchTimes::default(),
    }))
}

/// `--cold-memory-bytes-on` and `--cold-cpu-millis-on`: the cold memory and the cold CPU of
/// the jobs of each operating system they name.
///
/// # Errors
///
/// An operating system one of them names twice, naming that flag.
fn cold_on_of(cli: &Cli) -> Result<ColdOn, StartupError> {
    let memory = per_os(&cli.cold_memory_bytes_on)
        .map_err(|why| StartupError::new("--cold-memory-bytes-on", why))?;
    let cpu = per_os(&cli.cold_cpu_millis_on)
        .map_err(|why| StartupError::new("--cold-cpu-millis-on", why))?;
    Ok(ColdOn {
        memory: ColdMemoryOn::new(memory),
        cpu: ColdCpuOn::new(cpu),
    })
}

/// The least cold memory of an operating system: 1 GiB, the least memory the estimator books,
/// as for `--cold-memory-bytes`.
pub(crate) const COLD_MEMORY_LEAST: u64 = 1 << 30;

/// One value of `--cold-memory-bytes-on`, `OS=BYTES`: the name of an operating system as a
/// job's `os` constraint gives it, and its cold memory in bytes ([`cold_entry`]).
///
/// # Errors
///
/// The text says what is wrong: no `=`, bytes that are not a whole number, or what
/// [`cold_entry`] refuses.
pub fn parse_cold_on(text: &str) -> Result<(String, u64), String> {
    let (os, bytes) = text
        .split_once('=')
        .ok_or_else(|| "must be OS=BYTES, such as macos=4294967296".to_owned())?;
    let bytes = bytes
        .parse::<u64>()
        .map_err(|_| format!("{bytes:?} is not a whole number of bytes"))?;
    cold_entry(os, bytes)
}

/// The least cold CPU of an operating system: a quarter core, the least CPU the estimator
/// books, as for `--cold-cpu-millis`.
pub(crate) const COLD_CPU_LEAST: u64 = 250;

/// The most cold CPU of an operating system: the most millicores a host can report, as for
/// `--cold-cpu-millis`.
pub(crate) const COLD_CPU_MOST: u64 = u32::MAX as u64;

/// One value of `--cold-cpu-millis-on`, `OS=MILLIS`: the name of an operating system as a
/// job's `os` constraint gives it, and its cold CPU in millicores ([`cold_cpu_entry`]).
///
/// # Errors
///
/// The text says what is wrong: no `=`, millicores that are not a whole number, or what
/// [`cold_cpu_entry`] refuses.
pub fn parse_cold_cpu_on(text: &str) -> Result<(String, u64), String> {
    let (os, millis) = text
        .split_once('=')
        .ok_or_else(|| "must be OS=MILLIS, such as macos=1000".to_owned())?;
    let millis = millis
        .parse::<u64>()
        .map_err(|_| format!("{millis:?} is not a whole number of millicores"))?;
    cold_cpu_entry(os, millis)
}

/// The cold CPU `millis` of the operating system `os`, checked: a name that is not empty and
/// millicores from 250 (a quarter core) to 4294967295.
///
/// # Errors
///
/// The text says what is wrong.
pub(crate) fn cold_cpu_entry(os: &str, millis: u64) -> Result<(String, u64), String> {
    if os.is_empty() {
        return Err("the operating system is empty".to_owned());
    }
    if !(COLD_CPU_LEAST..=COLD_CPU_MOST).contains(&millis) {
        return Err(format!(
            "{os}: {millis} is not from 250 (a quarter core) to 4294967295"
        ));
    }
    Ok((os.to_owned(), millis))
}

/// The cold memory `bytes` of the operating system `os`, checked: a name that is not empty and
/// bytes from 1073741824 (1 GiB).
///
/// # Errors
///
/// The text says what is wrong.
pub(crate) fn cold_entry(os: &str, bytes: u64) -> Result<(String, u64), String> {
    if os.is_empty() {
        return Err("the operating system is empty".to_owned());
    }
    if bytes < COLD_MEMORY_LEAST {
        return Err(format!(
            "{os}: {bytes} is below the least, 1073741824 (1 GiB)"
        ));
    }
    Ok((os.to_owned(), bytes))
}

/// The value of each operating system `entries` names, as one of the repeatable per-system
/// flags gives them (`--cold-memory-bytes-on`, `--cold-cpu-millis-on`).
///
/// # Errors
///
/// An operating system named twice.
pub(crate) fn per_os(entries: &[(String, u64)]) -> Result<BTreeMap<String, u64>, String> {
    let mut on = BTreeMap::new();
    for (os, value) in entries {
        if on.insert(os.clone(), *value).is_some() {
            return Err(format!("{os} is given twice"));
        }
    }
    Ok(on)
}

/// The pool the file at `path` describes. A file that names `join_token_file` is not asked for
/// the file: it leaves a line in `warnings` instead.
fn read_pool(path: &Path, warnings: &mut Vec<String>) -> Result<PoolConfig, StartupError> {
    let file = agent_pool::read(path)?;
    if file.names_join_token_file {
        let item = format!("{}: join_token_file", path.display());
        warnings.push(retired::ignored(&item));
    }
    Ok(file.pool)
}

#[cfg(test)]
mod tests {
    use js_testkit::dir::TestDir;

    use super::*;

    const POOL: &str = "name = \"mac\"\nclass = \"mac-host\"\nmax_machines = 4\n\
                        [shape]\ncpu_millis = 4000\nmemory_bytes = 8589934592\n";

    /// A pool file that names `join_token_file` is read whatever the path is (one that does
    /// not exist still starts) and leaves exactly one line naming the file and the key; one that
    /// does not name it leaves none. It catches a read of the retired key that refuses a start
    /// for a missing file, one that says nothing, and one that says it twice.
    #[test]
    fn a_pool_file_naming_join_token_file_leaves_one_line_and_is_not_read() {
        let dir = TestDir::under(std::env::temp_dir(), "agent_setup_pool_files");
        let plain = dir.path().join("plain.toml");
        let named = dir.path().join("named.toml");
        std::fs::write(&plain, POOL).unwrap();
        let text = format!("join_token_file = \"/nowhere/at/all\"\n{POOL}");
        std::fs::write(&named, text).unwrap();
        let mut warnings = Vec::new();
        let plain_pool = read_pool(&plain, &mut warnings).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        let named_pool = read_pool(&named, &mut warnings).unwrap();
        assert_eq!(named_pool, plain_pool);
        let item = format!("{}: join_token_file", named.display());
        assert_eq!(warnings, [retired::ignored(&item)]);
    }
}
