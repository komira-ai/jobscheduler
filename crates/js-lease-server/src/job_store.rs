//! The job and allocation tables as the server holds them: one [`JobStore`] over the state
//! store chosen at start.
//!
//! The server opens it only when the job service is on (`--job-listen`, or
//! `server.job_listen`), once, and the agent side holds that one handle
//! ([`crate::agents::Agents::jobs`]): the job service ([`crate::job_api::from_server`]) and
//! each heartbeat of an agent that speaks the per-job messages ([`crate::per_job`]) are served
//! from it, and nothing opens a second one. Without it no store is opened, no file is made,
//! and every heartbeat reply is the lease and run reply alone.
//!
//! # The backend
//!
//! With a state store (`--store`, `server.store`) the tables are rows of that store, the one
//! the host bindings and the join tokens are in, so Postgres is the one place the server's
//! state is. A file `<state file>.jobs` of an earlier start without the setting is not read
//! or changed: the first start over the store begins with an empty job store, and the file
//! stays where it was as a backup.
//!
//! Without a state store the backend is a `js-state` SQLite file beside the state file,
//! `<state file>.jobs`: the single-node development backend. The file is the server user's
//! and owner-only (mode 0600), as the agent side's files beside it are: it holds the jobs'
//! specs, which may carry secrets. A file already there is kept and made owner-only.
//! SQLite's own files beside it (`-wal`, `-shm`) take the file's mode, and the store's lock
//! (`.lock`) is owner-only too.
//!
//! # Other servers at the open
//!
//! Over a Postgres store other servers may write the tables while this one opens them: the
//! old instance of a start-first update keeps serving until the new one is up. The open's
//! commits (the index backfill and the boot's raise) are conditional on the versions they
//! read, so a write of another server in between makes one lose
//! ([`JobStoreError::Stale`]). That is a lost race, not a fault: the open reads again,
//! decides again and commits again, after a pause that doubles from [`FIRST_RETRY`] to at
//! most [`MOST_RETRY`], for as long as it keeps losing. Every other error refuses the start.

use std::fs::{OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{IdSource, JobStore, JobStoreError, MintError, RandomIds, Retention};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Store, SystemClock};
use tracing::{info, warn};

use crate::agents::state_path;
use crate::config::FeedRetention;
use crate::error::StartupError;

mod feed_trim;

pub(crate) use feed_trim::FeedTrim;

/// The job store the server runs on: the tables over whichever state store the service
/// opened, with allocation ids from the source of its [`JobSources`] (the system's random
/// source, unless the in-process seam gives another).
pub(crate) type ServerJobs = JobStore<Arc<dyn Store>, ServerIds>;

/// Where the server's job store mints its ids: the system's random source
/// ([`ServerIds::random`]) in a service, or a source the caller gives
/// (`Config::with_job_sources`, the in-process seam), so a simulation's ids are a function
/// of its seed.
#[derive(Clone)]
pub struct ServerIds(Arc<dyn IdSource>);

impl ServerIds {
    /// Ids from `source`.
    #[must_use]
    pub fn new(source: Arc<dyn IdSource>) -> ServerIds {
        ServerIds(source)
    }

    /// Ids from the system's random source ([`RandomIds`]).
    #[must_use]
    pub fn random() -> ServerIds {
        ServerIds(Arc::new(RandomIds))
    }
}

impl IdSource for ServerIds {
    fn mint(&self) -> Result<[u8; 16], MintError> {
        self.0.mint()
    }
}

/// How the open waits before it tries again after losing a commit to another server: the
/// thread sleeps, unless a test gives a pause that records the wait instead.
pub(crate) type Pause = Arc<dyn Fn(Duration) + Send + Sync>;

/// The pause before the open's first try again after a lost commit.
pub(crate) const FIRST_RETRY: Duration = Duration::from_millis(10);

/// The longest pause between the open's tries: each pause doubles the one before, up to this.
pub(crate) const MOST_RETRY: Duration = Duration::from_secs(1);

/// The clock the job store stamps its rows with, the source it mints ids from, and how its
/// open waits before trying again: the system's clock, random source and sleep unless the
/// in-process seam or a test gives others.
pub(crate) struct JobSources {
    /// The store's clock.
    pub(crate) clock: Arc<dyn Clock>,
    /// The store's ids.
    pub(crate) ids: ServerIds,
    /// The wait before the open tries again after losing a commit to another server.
    pub(crate) pause: Pause,
}

impl Default for JobSources {
    fn default() -> JobSources {
        JobSources {
            clock: Arc::new(SystemClock::default()),
            ids: ServerIds::random(),
            pause: Arc::new(std::thread::sleep),
        }
    }
}

/// What the job store is opened with: the flag or key of the job service's address (which
/// turned it on, and names it in a refusal), how long an ended job is kept
/// (`--job-retention`, `server.job_retention`), how many changes its feed keeps
/// (`--job-feed-retention`, `server.job_feed_retention`) and how long a freed allocation's
/// row is kept (`--job-allocation-retention`, `server.job_allocation_retention`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct JobStoreSetup {
    /// `--job-listen` or `server.job_listen`.
    pub(crate) flag: &'static str,
    /// The window an ended job's record and result are kept for.
    pub(crate) retention: Retention,
    /// How many of the newest changes of the store's feed the tick keeps.
    pub(crate) feed_retention: FeedRetention,
    /// The window a freed allocation's row is kept for after its free.
    pub(crate) allocation_retention: Retention,
}

/// The job store as [`open`] returns it: the tables, and the state store under them, which
/// the tick trims the feed of ([`FeedTrim`]) and `GetUsage` reads the usage rows of
/// ([`crate::job_api::fleet::StoreHistory`]).
pub(crate) struct Opened {
    /// The job and allocation tables.
    pub(crate) jobs: Arc<ServerJobs>,
    /// The tick's handle for trimming the store's feed.
    pub(crate) feed: FeedTrim,
    /// The state store under the tables, which holds the `usage/ended/` row written beside
    /// each free of an allocation.
    pub(crate) store: Arc<dyn Store>,
}

/// The suffix of the job store's file, after the state file's name.
pub(crate) const SUFFIX: &str = ".jobs";

/// Opens the job store in the state store `kept` when there is one, else in a file beside
/// the state file `state_file`, as `setup` says, on the clock and id source of `sources`, and
/// raises its
/// deadlines for this boot in one transaction ([`JobStore::raise_at_boot`]) before the server
/// ticks or serves; the same transaction replaces the file contents a row of an ended job
/// still holds (one an earlier build ended) by their digests. A commit of the open that
/// another server's write beat is tried again, from its reads, until it holds (see "Other
/// servers at the open" above). A file that
/// cannot be made owner-only or opened as a state store (a directory in its place, another
/// process holding it, a file that is not a state store), and any other error of the open (a
/// row that cannot be read, a store that fails), is a refused start naming the flag of the
/// job service's address.
pub(crate) fn open(
    (state_file, kept): (&Path, Option<Arc<dyn Store>>),
    setup: JobStoreSetup,
    sources: &JobSources,
) -> Result<Opened, StartupError> {
    let (store, home) = match kept {
        Some(store) => (store, "the state store".to_owned()),
        None => {
            let path = state_path(state_file, SUFFIX);
            let store = open_file(&path, setup, sources)?;
            (store, path.display().to_string())
        }
    };
    let feed = FeedTrim::new(Arc::clone(&store), setup.feed_retention);
    let (jobs, raised) =
        booted_until_won(&store, sources).map_err(StartupError::because(setup.flag))?;
    info!(home, raised, "the job store is open");
    Ok(Opened {
        jobs: Arc::new(
            jobs.with_retention(setup.retention)
                .with_allocation_retention(setup.allocation_retention),
        ),
        feed,
        store,
    })
}

/// [`booted`] again, after a pause that doubles from [`FIRST_RETRY`] up to [`MOST_RETRY`],
/// for as long as it answers [`JobStoreError::Stale`]: another server committed between one
/// of its reads and its commit, and nothing was written. Any other answer is returned.
fn booted_until_won(
    store: &Arc<dyn Store>,
    sources: &JobSources,
) -> Result<(ServerJobs, usize), JobStoreError> {
    let mut wait = FIRST_RETRY;
    let mut lost: u64 = 0;
    loop {
        match booted(store, sources) {
            Err(JobStoreError::Stale { id }) => {
                lost = lost.saturating_add(1);
                warn!(
                    %id,
                    lost,
                    wait_ms = wait.as_millis(),
                    "another server changed a job while the job store opened; trying again"
                );
                (sources.pause)(wait);
                wait = wait.saturating_mul(2).min(MOST_RETRY);
            }
            answer => return answer,
        }
    }
}

/// The job store over `store`, with its indexes backfilled ([`JobStore::new`]) and its
/// deadlines raised for a boot at the store's reading now ([`JobStore::raise_at_boot`]),
/// with how many rows the raise wrote. Before the first tick: an outage longer than a queue
/// wait, a start timeout or a retention window fails, ends or deletes nothing at that tick
/// (design 6.7, step 7). An ended job's row that still holds its files' contents is
/// digested in the same commit.
fn booted(
    store: &Arc<dyn Store>,
    sources: &JobSources,
) -> Result<(ServerJobs, usize), JobStoreError> {
    let boot = store.now();
    let jobs = JobStore::new(Arc::new(Arc::clone(store)), sources.ids.clone())?;
    let raised = jobs.raise_at_boot(boot)?;
    Ok((jobs, raised))
}

/// Opens the SQLite file at `path`, made owner-only first, as the job store's backend.
fn open_file(
    path: &Path,
    setup: JobStoreSetup,
    sources: &JobSources,
) -> Result<Arc<dyn Store>, StartupError> {
    if let Err(err) = owner_only(path) {
        let reason = format!("{}: {err}", path.display());
        return Err(StartupError::new(setup.flag, reason));
    }
    let clock = Arc::clone(&sources.clock);
    let store = SqliteStore::open(path, clock, Options::default())
        .map_err(StartupError::because(setup.flag))?;
    Ok(Arc::new(store))
}

/// Makes the file at `path` (creating it empty when it is missing) owner-only.
fn owner_only(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use js_jobs::interfere::{INJECTED, Interfere};
    use js_state::fake::Fake;
    use js_state::{Cond, Txn};
    use js_testkit::dir::TestDir;

    use super::*;
    use crate::agent_api::job_rig::tables;

    fn scratch(name: &str) -> TestDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/job_store");
        TestDir::under(root, name)
    }

    fn setup(retention_secs: u64) -> JobStoreSetup {
        JobStoreSetup {
            flag: "--job-listen",
            retention: Retention::new(Duration::from_secs(retention_secs)).unwrap(),
            feed_retention: FeedRetention::DEFAULT,
            allocation_retention: Retention::new(Duration::from_secs(2 * retention_secs)).unwrap(),
        }
    }

    /// Asserts that the store is opened beside the state file under its suffix, with the
    /// retention it was given, and that a second open of the same file while the first is
    /// held is refused naming the flag, with the store's own reason. It catches a store
    /// opened with the default retention whatever the setting says, and one opened twice.
    #[test]
    fn the_store_is_opened_once_beside_the_state_file_with_its_retention() {
        let dir = scratch("opened");
        let state = dir.path().join("leases.toml");
        let jobs = open((&state, None), setup(259_200), &JobSources::default())
            .unwrap()
            .jobs;
        assert!(dir.path().join("leases.toml.jobs").is_file());
        assert_eq!(jobs.retention(), setup(259_200).retention);
        assert_eq!(
            jobs.allocation_retention(),
            setup(259_200).allocation_retention
        );
        let second = open((&state, None), setup(259_200), &JobSources::default())
            .err()
            .unwrap();
        assert_eq!(second.flag(), "--job-listen");
        assert!(second.reason().contains("leases.toml.jobs"), "{second}");
    }

    /// Asserts that opening the store is the boot: a queued job written before it is given a
    /// deadline floor of at least the open plus the grace, and a second open (a restart) keeps
    /// a floor of at least the first. It catches a store opened without the raise, so the
    /// first tick after an outage would fail every queued job that waited through it.
    #[test]
    fn opening_the_store_raises_the_deadlines_for_the_boot() {
        let dir = scratch("boot");
        let state = dir.path().join("leases.toml");
        let jobs = open((&state, None), setup(60), &JobSources::default())
            .unwrap()
            .jobs;
        let id = js_jobs::JobId::parse("job-q").unwrap();
        let new = js_jobs::NewJob {
            id: Some(id.clone()),
            spec: crate::agent_api::job_rig::spec(),
            priority: 0,
            client: None,
        };
        let queued = jobs.insert_queued(new).unwrap().record;
        assert_eq!(queued.deadline_floor, None);
        drop(jobs);
        let floor = |jobs: &ServerJobs| jobs.get_job(&id).unwrap().unwrap().deadline_floor;
        let reopened = open((&state, None), setup(60), &JobSources::default())
            .unwrap()
            .jobs;
        let first = floor(&reopened).unwrap();
        assert!(first >= queued.submitted_at.saturating_add(js_jobs::BOOT_GRACE));
        drop(reopened);
        let again = open((&state, None), setup(60), &JobSources::default())
            .unwrap()
            .jobs;
        assert!(floor(&again).unwrap() >= first);
    }

    /// Asserts that a directory where the store's file goes (which no user, root included,
    /// can open as a file) is refused naming the flag, with the path in the reason.
    #[test]
    fn a_directory_in_the_files_place_is_refused_naming_the_flag() {
        let dir = scratch("directory");
        let state = dir.path().join("leases.toml");
        std::fs::create_dir(dir.path().join("leases.toml.jobs")).unwrap();
        let refused = open((&state, None), setup(60), &JobSources::default())
            .err()
            .unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert!(refused.reason().contains("leases.toml.jobs"), "{refused}");
    }

    /// The index space of the jobs that wait or run, as the job store keys it.
    const ACTIVE_INDEX: &str = "idx/job/active/";

    /// The job sources of a test: the system's clock and ids, and a pause that records each
    /// wait instead of sleeping, with the record.
    fn recording() -> (JobSources, Arc<Mutex<Vec<Duration>>>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&waits);
        let pause: Pause = Arc::new(move |wait| record.lock().unwrap().push(wait));
        let sources = JobSources {
            pause,
            ..JobSources::default()
        };
        (sources, waits)
    }

    /// Makes another server write the row `key` (its value again, so only its version
    /// moves) just before each of the next `commits` commits of `store`, so each of them,
    /// conditional on the version read before, loses.
    fn contend(store: &Arc<Interfere<Fake>>, key: &'static str, commits: usize) {
        if commits == 0 {
            return;
        }
        let again = Arc::clone(store);
        store.before_next(move || {
            let other = again.inner();
            let row = other.get(key).unwrap().unwrap();
            let rewrite = Txn::new().put(key, row.value, Cond::Any);
            other.txn(rewrite).unwrap().unwrap();
            contend(&again, key, commits - 1);
        });
    }

    /// Job tables over a fake store holding the queued job `job-q`, which a boot gives a
    /// deadline floor to, so the boot's raise commits.
    fn with_a_queued_job() -> Arc<Interfere<Fake>> {
        let (store, jobs) = tables();
        let new = js_jobs::NewJob {
            id: Some(js_jobs::JobId::parse("job-q").unwrap()),
            spec: crate::agent_api::job_rig::spec(),
            priority: 0,
            client: None,
        };
        jobs.insert_queued(new).unwrap();
        store
    }

    /// The job `id` as `jobs` reads it.
    fn job(jobs: &ServerJobs, id: &str) -> js_jobs::JobRecord {
        let id = js_jobs::JobId::parse(id).unwrap();
        jobs.get_job(&id).unwrap().unwrap()
    }

    /// Opens the job store over the state store `store`, as a server over Postgres does.
    fn open_kept(
        store: &Arc<Interfere<Fake>>,
        sources: &JobSources,
    ) -> Result<Opened, StartupError> {
        let state = Path::new("unused/leases.toml");
        let kept: Arc<dyn Store> = store.clone();
        open((state, Some(kept)), setup(60), sources)
    }

    /// Asserts that a boot's raise that another server's writes beat nine times in a row is
    /// tried again each time and then holds: the open succeeds, the queued job has its floor,
    /// the store serves a new job, and the waits between the tries doubled from 10 ms and
    /// stopped growing at one second. It catches a lost race at the open refusing the start
    /// (a start-first update that rolls back), and a backoff with no ceiling.
    #[test]
    fn a_boot_raise_that_other_servers_beat_is_tried_again_until_it_holds() {
        let store = with_a_queued_job();
        contend(&store, "job/job-q", 9);
        let (sources, waits) = recording();
        let jobs = open_kept(&store, &sources).unwrap().jobs;
        let ms = Duration::from_millis;
        let doubled = [10, 20, 40, 80, 160, 320, 640, 1000, 1000].map(ms);
        assert_eq!(*waits.lock().unwrap(), doubled);
        assert!(job(&jobs, "job-q").deadline_floor.is_some());
        let new = js_jobs::NewJob {
            id: Some(js_jobs::JobId::parse("job-after").unwrap()),
            spec: crate::agent_api::job_rig::spec(),
            priority: 0,
            client: None,
        };
        jobs.insert_queued(new).unwrap();
        assert_eq!(job(&jobs, "job-after").state, js_jobs::JobState::Queued);
    }

    /// Asserts that an index backfill whose commit another server's write beat is tried
    /// again: the open succeeds after one wait of 10 ms, and the index entries the backfill
    /// was to write back are there. It catches the backfill's lost race refusing the start.
    #[test]
    fn an_index_backfill_that_another_server_beat_is_tried_again() {
        let store = with_a_queued_job();
        // The backfill commits before the raise, so its commit is the one that loses.
        let inner = store.inner();
        let entries = inner.scan_prefix(ACTIVE_INDEX).unwrap();
        assert!(!entries.is_empty());
        let gone = entries.iter().fold(Txn::new(), |txn, (key, _)| {
            txn.delete(key.as_str(), Cond::Any)
        });
        inner.txn(gone).unwrap().unwrap();
        contend(&store, "job/job-q", 1);
        let (sources, waits) = recording();
        open_kept(&store, &sources).unwrap();
        assert_eq!(*waits.lock().unwrap(), [FIRST_RETRY]);
        let back: Vec<String> = inner
            .scan_prefix(ACTIVE_INDEX)
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let before: Vec<String> = entries.into_iter().map(|(key, _)| key).collect();
        assert_eq!(back, before);
    }

    /// Asserts that an open nothing races commits at once, with no wait. It catches a pause
    /// taken before the first try.
    #[test]
    fn an_open_no_other_server_races_waits_for_nothing() {
        let store = with_a_queued_job();
        let (sources, waits) = recording();
        let jobs = open_kept(&store, &sources).unwrap().jobs;
        assert!(waits.lock().unwrap().is_empty());
        assert!(job(&jobs, "job-q").deadline_floor.is_some());
    }

    /// Asserts that a schema row the open cannot read refuses the start at once, naming the
    /// flag and the row, with no try again: only a lost race is retried. It catches a retry
    /// loop that swallows a real fault and never ends.
    #[test]
    fn a_row_the_open_cannot_read_refuses_the_start_without_a_retry() {
        let store = with_a_queued_job();
        let garbage = Txn::new().put(js_jobs::keys::SCHEMA, b"not a schema".to_vec(), Cond::Any);
        store.inner().txn(garbage).unwrap().unwrap();
        let (sources, waits) = recording();
        let refused = open_kept(&store, &sources).err().unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert!(
            refused.reason().contains(js_jobs::keys::SCHEMA),
            "{refused}"
        );
        assert!(waits.lock().unwrap().is_empty());
    }

    /// Asserts that a store that fails at the open refuses the start at once, naming the
    /// flag and the store's reason, with no try again.
    #[test]
    fn a_store_that_fails_at_the_open_refuses_the_start_without_a_retry() {
        let store = with_a_queued_job();
        store.fail_read_after(0);
        let (sources, waits) = recording();
        let refused = open_kept(&store, &sources).err().unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert!(refused.reason().contains(INJECTED), "{refused}");
        assert!(waits.lock().unwrap().is_empty());
    }

    /// Asserts that a file that is not a state store is refused naming the flag, and is
    /// left as it was apart from its mode.
    #[test]
    fn a_file_that_is_not_a_state_store_is_refused_naming_the_flag() {
        let dir = scratch("garbage");
        let state = dir.path().join("leases.toml");
        let path = dir.path().join("leases.toml.jobs");
        std::fs::write(
            &path,
            b"this is not a SQLite database, and it is long enough",
        )
        .unwrap();
        let refused = open((&state, None), setup(60), &JobSources::default())
            .err()
            .unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"this is not a SQLite database, and it is long enough"
        );
    }
}
