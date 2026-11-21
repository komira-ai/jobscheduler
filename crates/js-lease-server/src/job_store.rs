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
//! old instance of a start-first update keeps serving until the new one is up, and on a busy
//! farm it changes a job every few milliseconds. The open is one pass that always finishes:
//! the index backfill and the boot's raise read the tables once and commit each row in a
//! transaction of its own at the version read ([`JobStore::backfill_indexes`],
//! [`JobStore::raise_at_boot`]), so a write of another server in between loses that row
//! alone, which is read again and decided again; nothing starts over and nothing waits. Its
//! cost is set by the size of the tables, not by how fast the jobs change, and its writes are
//! one row each, as the serving server's own are. Any error of the open refuses the start.

use std::fs::{OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use js_jobs::{IdSource, JobStore, JobStoreError, MintError, RandomIds, Retention};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Store, SystemClock};
use tracing::info;

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

/// The clock the job store stamps its rows with and the source it mints ids from: the
/// system's clock and random source unless the in-process seam or a test gives others.
pub(crate) struct JobSources {
    /// The store's clock.
    pub(crate) clock: Arc<dyn Clock>,
    /// The store's ids.
    pub(crate) ids: ServerIds,
}

impl Default for JobSources {
    fn default() -> JobSources {
        JobSources {
            clock: Arc::new(SystemClock::default()),
            ids: ServerIds::random(),
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
/// raises its deadlines for this boot ([`JobStore::raise_at_boot`]) before the server ticks
/// or serves; the same raise replaces the file contents a row of an ended job still holds
/// (one an earlier build ended) by their digests. Another server's writes during the open
/// cost it only the rows they changed, read again (see "Other servers at the open" above),
/// and it logs one line when it is open. A file that cannot be made owner-only or opened as a
/// state store (a directory in its place, another process holding it, a file that is not a
/// state store), and any other error of the open (a row that cannot be read, a store that
/// fails), is a refused start naming the flag of the job service's address.
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
    let (jobs, raised) = booted(&store, sources).map_err(StartupError::because(setup.flag))?;
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

/// The job store over `store`, with its indexes backfilled ([`JobStore::new`]) and its
/// deadlines raised for a boot at the store's reading now ([`JobStore::raise_at_boot`]),
/// with how many rows the raise wrote. Before the first tick: an outage longer than a queue
/// wait, a start timeout or a retention window fails, ends or deletes nothing at that tick
/// (design 6.7, step 7). An ended job's row that still holds its files' contents is
/// digested in the same put as its raise.
fn booted(
    store: &Arc<dyn Store>,
    sources: &JobSources,
) -> Result<(ServerJobs, usize), JobStoreError> {
    let boot = store.now()?;
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
#[path = "job_store_busy_tests.rs"]
mod busy_tests;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use js_jobs::interfere::{INJECTED, Interfere, Read};
    use js_state::fake::Fake;
    use js_state::{Cond, Txn};
    use js_testkit::dir::TestDir;

    use super::*;
    use crate::agent_api::job_rig::tables;
    use crate::api::rig::{Lines, process_sink};

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

    /// Opens the job store over the state store `store`, as a server over Postgres does, with
    /// what it logged (without times, so a line is compared whole).
    fn open_kept(store: &Arc<Interfere<Fake>>) -> (Result<Opened, StartupError>, String) {
        let state = Path::new("unused/leases.toml");
        let kept: Arc<dyn Store> = store.clone();
        process_sink();
        let lines = Lines::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(lines.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let opened = tracing::subscriber::with_default(subscriber, || {
            open((state, Some(kept)), setup(60), &JobSources::default())
        });
        (opened, lines.take())
    }

    /// How many times the reads since the last call scanned the whole job table.
    fn table_scans(store: &Interfere<Fake>) -> usize {
        let whole = Read::Scan(js_jobs::keys::JOB.to_owned());
        store
            .take_reads()
            .into_iter()
            .filter(|read| *read == whole)
            .count()
    }

    /// The line the open logs when it is open, over a fake state store, having raised `raised`
    /// rows.
    fn open_line(raised: usize) -> String {
        format!(
            " INFO js_lease_server::job_store: the job store is open home=\"the state store\" \
             raised={raised}\n"
        )
    }

    /// Asserts that a boot raise whose commit another server's writes beat nine times in a row
    /// reads that row again each time and then holds, in the same open: the open succeeds, the
    /// queued job has its floor, the store serves a new job, the job table was scanned once,
    /// and the open logged exactly two lines: one saying the raise read rows again, with the
    /// count, and the line that it is open. It catches a lost race that refuses the start (a
    /// start-first update that rolls back), one that starts the open over from its scans (on
    /// a busy farm that never ends), and one that logs a line per lost race.
    #[test]
    fn a_boot_raise_that_another_server_beats_nine_times_reads_the_row_again_and_holds() {
        let store = with_a_queued_job();
        contend(&store, "job/job-q", 9);
        store.take_reads();
        let (opened, said) = open_kept(&store);
        let jobs = opened.unwrap().jobs;
        assert_eq!(
            table_scans(&store),
            1,
            "the open read the job table more than once"
        );
        let wanted = format!(
            " INFO js_jobs::store::ending: the boot's raise read again the rows another server \
             changed before its commit again=9\n{}",
            open_line(1)
        );
        assert_eq!(said, wanted);
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

    /// Asserts that an index backfill whose commit another server's write beat reads the row
    /// again in the same open: the open succeeds, logs exactly the backfill's line with the
    /// count of rows read again and the line that it is open, scans the job table once, and
    /// the index entries the backfill was to write back are there. It catches the backfill's
    /// lost race refusing the start or starting the open over.
    #[test]
    fn an_index_backfill_that_another_server_beat_reads_the_row_again() {
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
        store.take_reads();
        let (opened, said) = open_kept(&store);
        opened.unwrap();
        assert_eq!(
            table_scans(&store),
            1,
            "the open read the job table more than once"
        );
        let wanted = format!(
            " INFO js_jobs::store::index: the index backfill read again the rows another server \
             changed while it ran again=1\n{}",
            open_line(1)
        );
        assert_eq!(said, wanted);
        let back: Vec<String> = inner
            .scan_prefix(ACTIVE_INDEX)
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let before: Vec<String> = entries.into_iter().map(|(key, _)| key).collect();
        assert_eq!(back, before);
    }

    /// Asserts that an open nothing races raises the queued job, scans the job table once and
    /// logs one line, that it is open. It catches a read-again line logged with nothing read
    /// again.
    #[test]
    fn an_open_no_other_server_races_reads_once_and_logs_one_line() {
        let store = with_a_queued_job();
        store.take_reads();
        let (opened, said) = open_kept(&store);
        let jobs = opened.unwrap().jobs;
        assert_eq!(table_scans(&store), 1);
        assert_eq!(said, open_line(1));
        assert!(job(&jobs, "job-q").deadline_floor.is_some());
    }

    /// Asserts that a schema row the open cannot read refuses the start at once, naming the
    /// flag and the row, and commits nothing. It catches an open that swallows a real fault.
    #[test]
    fn a_row_the_open_cannot_read_refuses_the_start_at_once() {
        let store = with_a_queued_job();
        let garbage = Txn::new().put(js_jobs::keys::SCHEMA, b"not a schema".to_vec(), Cond::Any);
        store.inner().txn(garbage).unwrap().unwrap();
        let commits = store.commit_stats();
        let refused = open_kept(&store).0.err().unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert!(
            refused.reason().contains(js_jobs::keys::SCHEMA),
            "{refused}"
        );
        assert_eq!(store.commit_stats(), commits);
    }

    /// Asserts that a store that fails at the open refuses the start at once, naming the
    /// flag and the store's reason, and commits nothing.
    #[test]
    fn a_store_that_fails_at_the_open_refuses_the_start_at_once() {
        let store = with_a_queued_job();
        store.fail_read_after(0);
        let commits = store.commit_stats();
        let refused = open_kept(&store).0.err().unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert!(refused.reason().contains(INJECTED), "{refused}");
        assert_eq!(store.commit_stats(), commits);
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
