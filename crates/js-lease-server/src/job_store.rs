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
//! The store is a `js-state` SQLite file beside the lease file, `<state file>.jobs`: the
//! single-node development backend. It moves into the shared store when the server's state
//! does. The file is the server user's and owner-only (mode 0600), as the agent side's files
//! beside it are: it holds the jobs' specs, which may carry secrets. A file already there is
//! kept and made owner-only. SQLite's own files beside it (`-wal`, `-shm`) take the file's
//! mode, and the store's lock (`.lock`) is owner-only too.

use std::fs::{OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use js_jobs::{IdSource, JobStore, MintError, RandomIds, Retention};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Store, SystemClock};
use tracing::info;

use crate::agents::state_path;
use crate::error::StartupError;

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
/// system's clock and random source unless the in-process seam gives others.
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
/// turned it on, and names it in a refusal), and how long an ended job is kept
/// (`--job-retention`, `server.job_retention`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct JobStoreSetup {
    /// `--job-listen` or `server.job_listen`.
    pub(crate) flag: &'static str,
    /// The window an ended job's record and result are kept for.
    pub(crate) retention: Retention,
}

/// The suffix of the job store's file, after the lease file's name.
pub(crate) const SUFFIX: &str = ".jobs";

/// Opens the job store beside the lease file `state_file`, as `setup` says, on the clock and
/// id source of `sources`, and raises its
/// deadlines for this boot in one transaction ([`JobStore::raise_at_boot`]) before the server
/// ticks. A file that
/// cannot be made owner-only or opened as a state store (a directory in its place, another
/// process holding it, a file that is not a state store) is a refused start naming the flag
/// of the job service's address.
pub(crate) fn open(
    state_file: &Path,
    setup: JobStoreSetup,
    sources: &JobSources,
) -> Result<Arc<ServerJobs>, StartupError> {
    let path = state_path(state_file, SUFFIX);
    if let Err(err) = owner_only(&path) {
        let reason = format!("{}: {err}", path.display());
        return Err(StartupError::new(setup.flag, reason));
    }
    let clock = Arc::clone(&sources.clock);
    let store = SqliteStore::open(&path, clock, Options::default())
        .map_err(StartupError::because(setup.flag))?;
    let store: Arc<dyn Store> = Arc::new(store);
    let boot = store.now();
    let jobs = JobStore::new(Arc::new(store), sources.ids.clone())
        .map_err(StartupError::because(setup.flag))?;
    // Before the first tick: an outage longer than a queue wait, a start timeout or a
    // retention window fails, ends or deletes nothing at that tick (design 6.7, step 7).
    let raised = jobs
        .raise_at_boot(boot)
        .map_err(StartupError::because(setup.flag))?;
    info!(file = %path.display(), raised, "the job store is open");
    Ok(Arc::new(jobs.with_retention(setup.retention)))
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
    use std::time::Duration;

    use js_testkit::dir::TestDir;

    use super::*;

    fn scratch(name: &str) -> TestDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/job_store");
        TestDir::under(root, name)
    }

    fn setup(retention_secs: u64) -> JobStoreSetup {
        JobStoreSetup {
            flag: "--job-listen",
            retention: Retention::new(Duration::from_secs(retention_secs)).unwrap(),
        }
    }

    /// Asserts that the store is opened beside the lease file under its suffix, with the
    /// retention it was given, and that a second open of the same file while the first is
    /// held is refused naming the flag, with the store's own reason. It catches a store
    /// opened with the default retention whatever the setting says, and one opened twice.
    #[test]
    fn the_store_is_opened_once_beside_the_lease_file_with_its_retention() {
        let dir = scratch("opened");
        let state = dir.path().join("leases.toml");
        let jobs = open(&state, setup(259_200), &JobSources::default()).unwrap();
        assert!(dir.path().join("leases.toml.jobs").is_file());
        assert_eq!(jobs.retention(), setup(259_200).retention);
        let second = open(&state, setup(259_200), &JobSources::default())
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
        let jobs = open(&state, setup(60), &JobSources::default()).unwrap();
        let id = js_jobs::JobId::parse("job-q").unwrap();
        let new = js_jobs::NewJob {
            id: Some(id.clone()),
            spec: crate::agent_api::job_rig::spec(),
            priority: 0,
        };
        let queued = jobs.insert_queued(new).unwrap().record;
        assert_eq!(queued.deadline_floor, None);
        drop(jobs);
        let floor = |jobs: &ServerJobs| jobs.get_job(&id).unwrap().unwrap().deadline_floor;
        let reopened = open(&state, setup(60), &JobSources::default()).unwrap();
        let first = floor(&reopened).unwrap();
        assert!(first >= queued.submitted_at.saturating_add(js_jobs::BOOT_GRACE));
        drop(reopened);
        let again = open(&state, setup(60), &JobSources::default()).unwrap();
        assert!(floor(&again).unwrap() >= first);
    }

    /// Asserts that a directory where the store's file goes (which no user, root included,
    /// can open as a file) is refused naming the flag, with the path in the reason.
    #[test]
    fn a_directory_in_the_files_place_is_refused_naming_the_flag() {
        let dir = scratch("directory");
        let state = dir.path().join("leases.toml");
        std::fs::create_dir(dir.path().join("leases.toml.jobs")).unwrap();
        let refused = open(&state, setup(60), &JobSources::default())
            .err()
            .unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert!(refused.reason().contains("leases.toml.jobs"), "{refused}");
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
        let refused = open(&state, setup(60), &JobSources::default())
            .err()
            .unwrap();
        assert_eq!(refused.flag(), "--job-listen");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"this is not a SQLite database, and it is long enough"
        );
    }
}
