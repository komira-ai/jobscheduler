//! A pool over a fake host and a scratch directory of its own.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use js_testkit::dir::TestDir;
use tracing::Level;

use super::super::{AccountError, Places, Pool, Settings};
use super::fake::Fake;
use super::os::FakeOs;
use crate::test_tracing::{Capture, capture};

/// What the code logged, in memory.
#[derive(Clone, Default)]
pub(in super::super) struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    pub(in super::super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

/// This process's own uid: the uid of every file a test makes, so the accounts of a test
/// that look at files get it.
pub(in super::super) fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// This process's own group.
pub(in super::super) fn my_gid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// Everything a test of the pool needs: a scratch directory laid out as a Mac is (a state
/// directory, homes, per-user folders, the places a sweep covers), the fake host, and the
/// settings that point at them. The range of uids starts at the test's own uid, so files
/// the test makes belong to the account that is made first.
pub(in super::super) struct Rig {
    pub(in super::super) fake: Arc<Fake>,
    pub(in super::super) settings: Settings,
    pub(in super::super) log: Log,
    pub(in super::super) dir: TestDir,
    _capture: Option<Capture>,
}

impl Rig {
    /// A rig of `uids` uids, with `ready` accounts kept ready.
    pub(in super::super) fn new(name: &str, uids: u32, ready: usize) -> Self {
        let log = Log::default();
        let capture = capture(log.clone(), Level::INFO);
        Self::build(name, uids, ready, log, Some(capture))
    }

    /// Another rig for the same test, logging to this one's log through this one's capture:
    /// a second capture on the test's thread would take the events from the first for as
    /// long as both are installed.
    pub(in super::super) fn next(&self, name: &str, uids: u32, ready: usize) -> Self {
        Self::build(name, uids, ready, self.log.clone(), None)
    }

    fn build(name: &str, uids: u32, ready: usize, log: Log, capture: Option<Capture>) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-mac-accounts-{name}"));
        for sub in [
            "state", "Users", "folders", "at/tabs", "at/jobs", "Shared", "tmp", "launchd",
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        // The state directory is the owner's alone to write in, as the check requires.
        std::fs::set_permissions(
            dir.join("state"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let settings = Settings {
            range: me()..=me() + uids - 1,
            gid: my_gid(),
            homes: dir.join("Users"),
            ready,
            user_folders: dir.join("folders"),
            schedules: Places {
                named: vec![format!("{}/at/tabs/{{name}}", dir.display())],
                owned: vec![dir.join("at/jobs")],
            },
            shared: Places {
                named: vec![format!("{}/launchd/disabled.{{uid}}.plist", dir.display())],
                owned: vec![dir.join("Shared"), dir.join("tmp")],
            },
            state_owner: Some(me()),
        };
        Self {
            fake: Fake::new(),
            settings,
            log,
            dir,
            _capture: capture,
        }
    }

    /// The pool, opened over the rig.
    pub(in super::super) fn open(&self) -> Pool {
        self.try_open().unwrap_or_else(|why| panic!("open: {why}"))
    }

    /// The pool, opened over the rig with `os` for the removal of what is in the per-user
    /// folders.
    pub(in super::super) fn open_with(&self, os: &FakeOs) -> Pool {
        Pool::with_os(
            &self.dir.join("state"),
            self.settings.clone(),
            &BTreeSet::new(),
            self.fake.clone(),
            Box::new(os.clone()),
        )
        .unwrap_or_else(|why| panic!("open: {why}"))
    }

    /// Opening the pool, whatever came of it, by an agent that adopted no job.
    pub(in super::super) fn try_open(&self) -> Result<Pool, AccountError> {
        self.try_open_adopting(&[])
    }

    /// The pool, opened over the rig by an agent that adopted the jobs `jobs` at its start.
    pub(in super::super) fn open_adopting(&self, jobs: &[&str]) -> Pool {
        self.try_open_adopting(jobs)
            .unwrap_or_else(|why| panic!("open: {why}"))
    }

    /// Opening the pool by an agent that adopted the jobs `jobs`, whatever came of it.
    fn try_open_adopting(&self, jobs: &[&str]) -> Result<Pool, AccountError> {
        let adopted: BTreeSet<String> = jobs.iter().map(|job| (*job).to_owned()).collect();
        Pool::with_settings(
            &self.dir.join("state"),
            self.settings.clone(),
            &adopted,
            self.fake.clone(),
        )
    }

    /// [`Rig::try_open`] for a test that expects it to be refused.
    pub(in super::super) fn refused(&self) -> AccountError {
        match self.try_open() {
            Ok(_) => panic!("the open was not refused"),
            Err(why) => why,
        }
    }

    /// The ledger, as text.
    pub(in super::super) fn ledger(&self) -> String {
        std::fs::read_to_string(self.dir.join("state/accounts")).unwrap_or_default()
    }

    /// The ledger's lines.
    pub(in super::super) fn lines(&self) -> Vec<String> {
        self.ledger().lines().map(str::to_owned).collect()
    }

    /// Writes `text` as the ledger.
    pub(in super::super) fn write_ledger(&self, text: &str) {
        std::fs::write(self.dir.join("state/accounts"), text).unwrap();
    }

    /// The name of account `n`.
    pub(in super::super) fn name(n: u64) -> String {
        super::super::ledger::name(n)
    }

    /// A per-user folder of the rig's uid, `folders/<shard>/<hash>`, as the system makes one:
    /// its four top-level directories, empty. (Every file a test makes belongs to the test's
    /// uid, which is the rig's first uid.)
    pub(in super::super) fn folder(&self, shard: &str, hash: &str) -> std::path::PathBuf {
        let folder = self.dir.join("folders").join(shard).join(hash);
        for top in ["0", "C", "T", "X"] {
            std::fs::create_dir_all(folder.join(top)).unwrap();
        }
        folder
    }

    /// The home folder of account `n`.
    pub(in super::super) fn home(&self, n: u64) -> std::path::PathBuf {
        self.settings.homes.join(Self::name(n))
    }
}
