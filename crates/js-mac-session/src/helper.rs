//! The verbs: `user-create`, `run`, `kill-uid` and `user-delete` (S4.2), in [`pool`] the
//! pool of lease users (`pool-create`, `pool-status`, and what `user-create` and
//! `user-delete` do with it), in [`runs`] `watch` and `stop` and the deadline of a `run`,
//! in [`boot_reset`] the boot reset, and in [`leak`] the leak scan's `scan`, over a
//! [`Host`] that does what only the OS can (user records, launchd, processes, the programs
//! the boot reset and the scan run), so the rules here are tested on any platform.
//!
//! Rules every verb keeps:
//! - A lease id is parsed strictly and is used once.
//! - A lease's user is a pool user, `kbf-lease-pool-<slot>`, lent for the lease and
//!   reset in place when it ends ([`pool`]; the pool's users are permanent, never
//!   deleted); or, for a lease a release before the pool created, its own
//!   `kbf-lease-<term>-<seq>`, deleted when it ends (which macOS 27 refuses root).
//! - The uid comes from the ledger, never from a lookup, and must be in the lease
//!   range; a uid outside it is never touched.
//! - `user-create` is refused for a lease id used before, takes only a free pool user
//!   (never making one: a full pool is [`LeaseError::Exhausted`]) and never makes an
//!   administrator: a job that needs administrator rights runs in a VM, not as a pool
//!   user. A request that carries a grant (a field of the protocol from when a gate's
//!   signed grant could make one) is refused, and no user is taken.
//! - `run`, `kill-uid`, `stop` and `user-delete` act only on a lease the ledger holds;
//!   `run`, `kill-uid` and `stop` refuse one that is over (its uid may be another
//!   lease's now). `stop` kills only as `kill-uid` does, after a `SIGTERM` to the run's
//!   own process group and its grace; `watch` reads the helper's table of runs and acts
//!   on nothing.
//! - `kill-uid` is judged by its outcome, never by what a `launchctl` call answered. It
//!   decides, before the kill, whether the uid may have a GUI login session: it owns a
//!   `loginwindow` process (or one whose name cannot be read), or an earlier teardown left
//!   its GUI domain owed (a record in the state directory, kept until a teardown judges the
//!   domain gone). A pool user has neither, so its `gui/<uid>` is never asked about. Then, for at most
//!   [`KILL_ROUNDS`] attempts, it kills every process of the
//!   uid and boots out `user/<uid>` (and `gui/<uid>`, only with the session), the
//!   bootout's own failure only logged, and looks again: done once no process of the
//!   uid is left and none of those domains prints (`launchctl print` exits non-zero).
//!   launchd restarts a per-user agent such as `distnoted` for as long as the domain
//!   exists, and a live process of the uid makes the domain again. It then removes the
//!   user's crontab and `at` jobs, and does it all again, so no scheduled job of the
//!   user starts after it returns. An outcome that does not hold after the last attempt
//!   fails, naming what remains, and the lease stays live: its uid is quarantined.
//! - `user-delete` of a pool user's lease records the lease over and resets the user in
//!   place (its home and per-user folder emptied and kept, its record's pool-owned
//!   attributes set back, and the directory asked that it is not in `admin`, a member
//!   being a broken slot); a reset that is not clean breaks the slot, naming what
//!   blocked it ([`pool`]). Nothing of a pool user is deleted. The deletion of a lease's user from before the pool refuses while any
//!   process of the uid remains. It first asks for the
//!   uid's `DARWIN_USER_DIR` (in a short-lived process of the uid, while its record is
//!   there) and takes the uid's own entry under the per-user folders from it, refusing
//!   any answer that is not such an entry; then it looks (and boots out the domains),
//!   removes what can start a process (crontab, `at` jobs), looks again, removes the
//!   uid's System V IPC objects, sweeps the rest and that entry whole, looks a last time
//!   (and boots out the domains), and deletes the record. For a lease from before the
//!   pool it records the deletion last, and a failure at any step leaves the lease live
//!   in the ledger, so its uid is held and never handed to another lease, and the
//!   refusal says why; the delete is simply repeated. At each look it waits, bounded, for
//!   processes of the uid to exit (a cron job that started after `kill-uid`), and names
//!   those still alive when it refuses.
//! - Every verb but `boot-reset` and `full-disk-access` is refused until the boot reset
//!   has succeeded. `full-disk-access` lists the privacy database's directory
//!   ([`Host::list_privacy_dir`]) and reads nothing of the pool.
//!
//! The mutating verbs run one at a time (one lock around the ledger), and `run` holds
//! it while it starts the process, so no process starts for a lease being deleted.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use js_types::LeaseId;
use rustix::fs::{CWD, Gid, Mode, OFlags, Uid};

use crate::autologin::{CommandOutput, LoginPaths};
use crate::full_disk_access::{self, FullDiskAccess};
use crate::lease::{Dotted, PoolSize, Slot, UidRange, parse_lease};
use crate::ledger::{Entry, Ledger};
use crate::proto::{Limits, PoolState, Reply, Request};
use crate::scan::ScanSettings;
use crate::spawn::{self, Spawn};
use crate::sweep::SweepPlan;

mod boot_reset;
pub(crate) mod folders;
mod ipc;
mod leak;
mod pool;
mod runs;
mod teardown;

pub use boot_reset::Reset;
pub use ipc::{IPCRM, IPCS};
pub use pool::{DSEDITGROUP, LeaseError, POOL_EXHAUSTED, PoolExhausted, PoolStatus};
pub use runs::{Ended, MAX_WAIT, NO_RUN};

use pool::LeaseUser;

/// How many attempts `kill-uid` makes (kill, boot out the uid's launchd domains, look at
/// the outcome) before it gives up.
pub const KILL_ROUNDS: usize = 10;

/// How many times each of `user-delete`'s looks lists the uid's processes, a
/// [`Host::pause`] apart, before it refuses: about five seconds on macOS, for a job
/// that started after `kill-uid` to exit.
pub const EXIT_LOOKS: usize = 50;

/// What every verb but `boot-reset` is refused with until the boot reset has run since the
/// helper started: a caller that finds it after the helper restarted under it asks again.
pub const RESET_NEEDED: &str = "the boot reset has not run since the helper started";

/// What a `scan` on a node whose leak-scan baseline was never recorded is refused with:
/// a caller can tell that the scan has nothing to compare with from a scan that failed.
/// The words are those of a release that had a `baseline` verb, kept so a caller of that
/// release still reads the refusal as this one.
pub const NO_BASELINE: &str = "no baseline was recorded; baseline first";

/// What a `user-create` that carries a grant is refused with: no lease user is ever an
/// administrator, whatever the request holds.
pub const NO_ADMINISTRATOR: &str =
    "no lease user is ever an administrator: a job that needs administrator rights runs in a VM";

/// The refusal of a second `boot-reset` since the helper started: a daemon that
/// restarted on its own (the helper did not) finds the reset done.
pub const RESET_ALREADY: &str = "the boot reset ran already; it runs once per start";

/// How many processes a refusal names; it counts the rest.
const NAMED: usize = 8;

/// How `launchctl print <domain>` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintAnswer {
    /// Its exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it wrote to standard output (the domain, as launchd prints it).
    pub stdout: String,
    /// What it wrote to standard error.
    pub stderr: String,
}

/// What the OS does for the helper. The macOS implementation is in `crate::macos`.
pub trait Host: Send + Sync {
    /// The wall clock, for the runs' deadlines.
    fn now(&self) -> SystemTime;
    /// Whether any user record has `uid`.
    ///
    /// # Errors
    /// The directory could not be asked.
    fn uid_taken(&self, uid: u32) -> io::Result<bool>;
    /// Makes the user's home folder, or takes the one that is there ([`make_home`] on a
    /// real host): owned by the user, mode 0700, with no flag, ACL or extended attribute
    /// of what the last user set on it. The caller has emptied it.
    ///
    /// # Errors
    /// The folder could not be made, or an existing one could not be given back.
    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()>;
    /// Creates the user record.
    ///
    /// # Errors
    /// The record could not be created.
    fn create_user(&self, user: &NewUser) -> io::Result<()>;
    /// Deletes the user record `name` (and any group membership it has); `false` when
    /// there is none. Only a lease user from before the pool is ever deleted: the pool's
    /// users are permanent, and macOS refuses root the deletion of a local user record
    /// unless the helper holds Full Disk Access ([`Host::list_privacy_dir`] shows whether it
    /// holds it). The privacy profile grants that access to `js-agent`, not to the helper,
    /// and the pool's reset does not use it: `docs/design/mac-account-per-job.md` deletes an
    /// account per job, in the agent.
    ///
    /// # Errors
    /// The record could not be deleted.
    fn delete_user(&self, name: &str) -> io::Result<bool>;
    /// The attributes of the record `name` that the pool owns ([`UserRecord`]); `None`
    /// when there is no such record.
    ///
    /// # Errors
    /// The directory could not be asked.
    fn user_record(&self, name: &str) -> io::Result<Option<UserRecord>>;
    /// Sets the pool-owned attributes of the existing record `user.name` that macOS lets
    /// root write on an existing account to what [`Host::create_user`] gives a new one:
    /// its primary group, shell and hidden flag. Its uid, home path, real name, password
    /// and group memberships are left alone (macOS 27 refuses root a write of an existing
    /// account's home attribute; the caller compares the uid and home first, and asks the
    /// directory that the user is not in `admin`).
    ///
    /// # Errors
    /// The record could not be found or an attribute could not be set.
    fn restore_user(&self, user: &NewUser) -> io::Result<()>;
    /// How `launchctl print <domain>` ended: its exit code and its standard output
    /// and error. The teardown reads any exit status but 0, and an exit 0 that printed a
    /// domain holding nothing, as a domain that is gone.
    ///
    /// # Errors
    /// `launchctl` could not be run.
    fn launchctl_print(&self, domain: &str) -> io::Result<PrintAnswer>;
    /// `launchctl bootout <domain>`: ends the domain and every job in it. Its failure is
    /// no verdict (it fails for a domain that is not there, and can fail after ending
    /// one): the caller logs it and asks `launchctl print` afterwards.
    ///
    /// # Errors
    /// The domain could not be booted out.
    fn bootout(&self, domain: &str) -> io::Result<()>;
    /// The uid's `DARWIN_USER_DIR` (`/var/folders/<xx>/<hash>/0/`) as
    /// `confstr(_CS_DARWIN_USER_DIR)` answers it in a process of `uid` and `gid`
    /// (`getconf DARWIN_USER_DIR` on a real host), which makes the folder if it is not
    /// there. The process has exited when this returns. Asked while the user's record
    /// exists: the entry is keyed by the record's UUID.
    ///
    /// # Errors
    /// The process could not be started or did not answer.
    fn user_dir(&self, uid: u32, gid: u32) -> io::Result<PathBuf>;
    /// Sends `SIGKILL` to every process of `uid`.
    ///
    /// # Errors
    /// The signal could not be sent.
    fn kill_all(&self, uid: u32) -> io::Result<()>;
    /// The live (not zombie) processes that have `uid` as their real or effective uid.
    ///
    /// # Errors
    /// The process table could not be read.
    fn live_processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>>;
    /// Lists the directory of the privacy database ([`full_disk_access::PRIVACY_DIR`]),
    /// which only a process with Full Disk Access may list: opens no file and reads no
    /// database.
    ///
    /// # Errors
    /// The listing failed; a refusal for want of permission is
    /// [`io::ErrorKind::PermissionDenied`].
    fn list_privacy_dir(&self) -> io::Result<()>;
    /// Waits a moment between kill rounds, and between the listings of a wait.
    fn pause(&self);
    /// Runs `program` with `args` and an empty environment
    /// ([`run_command`](crate::autologin::run_command) on a real host).
    ///
    /// # Errors
    /// The program could not be started or waited for.
    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
    /// The account record named `name`; `None` when there is none.
    ///
    /// # Errors
    /// The directory could not be asked.
    fn account(&self, name: &str) -> io::Result<Option<Account>>;
    /// Sends `SIGTERM` to the process group `pgid` (`killpg`), for `stop`; a group with no
    /// process is not an error.
    ///
    /// # Errors
    /// The signal could not be sent.
    fn term_group(&self, pgid: i32) -> io::Result<()>;
}

/// The ids of an account record, as the directory answers for its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    /// The account's uid.
    pub uid: u32,
    /// The account's primary group.
    pub gid: u32,
}

/// A live process of a lease uid, as a refusal names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProcess {
    /// Its pid.
    pub pid: i32,
    /// Its command name as the kernel keeps it (truncated), or `?` if unreadable.
    pub command: String,
}

/// The login shell of every pool user.
pub const SHELL: &str = "/bin/zsh";

/// The attributes of a user record that the pool owns: what `pool-create` makes and
/// every reset sets back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRecord {
    /// The uid.
    pub uid: u32,
    /// The primary group.
    pub gid: u32,
    /// The home folder's path.
    pub home: PathBuf,
    /// Whether the record is kept off the login window's list of users.
    pub hidden: bool,
    /// The login shell.
    pub shell: String,
}

/// A user record to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewUser {
    /// The account name.
    pub name: String,
    /// The uid.
    pub uid: u32,
    /// The primary group.
    pub gid: u32,
    /// The home folder.
    pub home: PathBuf,
}

/// The helper's fixed configuration.
#[derive(Debug)]
pub struct Settings {
    /// The uids lease users may have.
    pub range: UidRange,
    /// How many users the pool holds (`--pool-size`); slot `n`'s user has the range's
    /// first uid plus `n`.
    pub pool: PoolSize,
    /// Every lease user's primary group.
    pub gid: u32,
    /// Where home folders are made (`/Users` on macOS).
    pub homes: PathBuf,
    /// What `user-delete` sweeps first: where a process of the user can be started
    /// with no session (crontab, `at` jobs).
    pub schedules: SweepPlan,
    /// What `user-delete` sweeps then, besides the home folder and the uid's entry under
    /// [`Self::user_folders`].
    pub sweep: SweepPlan,
    /// Where macOS keeps each user's `DARWIN_USER_DIR` entry (`/private/var/folders`): the
    /// uid's own entry here, named by its `DARWIN_USER_DIR`, is emptied and kept (a pool
    /// user's reset) or, for a lease from before the pool, removed whole by `user-delete`
    /// (emptied and kept where the system placed it, `sunlnk`).
    pub user_folders: PathBuf,
    /// The helper's state directory (`--state-dir`), where a teardown keeps the record of
    /// a GUI domain it still owes.
    pub state_dir: PathBuf,
    /// The login window's auto-login, which the boot reset clears and the leak scan reads.
    pub login: LoginPaths,
    /// What the leak scan looks at, and where it reads its baseline.
    pub scan: ScanSettings,
}

/// What a request comes to.
#[derive(Debug)]
pub enum Outcome {
    /// One reply, and the request is done.
    Reply(Reply),
    /// A process started; the caller reports its exit ([`Helper::wait`]).
    Running(Child),
    /// `watch` found the run of this lease, whose process is this pid: the caller answers
    /// `started` and then, when [`Helper::wait_watched`] returns, how it ended.
    Watching(String, u32),
}

/// The helper: settings, the host, and the ledger behind one lock.
pub struct Helper {
    host: Box<dyn Host>,
    settings: Settings,
    ledger: Mutex<Ledger>,
    /// Set, under the ledger's lock, once the boot reset has succeeded.
    reset_done: AtomicBool,
    /// The last run of each lease ([`runs`]).
    runs: runs::Runs,
}

impl Helper {
    /// A helper that acts through `host`, under `settings`, recording lease users in `ledger`.
    #[must_use]
    pub fn new(host: Box<dyn Host>, settings: Settings, ledger: Ledger) -> Self {
        Self {
            host,
            settings,
            ledger: Mutex::new(ledger),
            reset_done: AtomicBool::new(false),
            runs: runs::Runs::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Carries out `request`. Only `run` takes descriptors, and nothing but
    /// `boot-reset` is done before the boot reset has succeeded.
    pub fn handle(&self, request: Request, fds: Vec<OwnedFd>) -> Outcome {
        let refused = |reason: String| Outcome::Reply(Reply::Refused { reason });
        if !self.reset_done()
            && !matches!(request, Request::BootReset { .. } | Request::FullDiskAccess)
        {
            return refused(RESET_NEEDED.to_owned());
        }
        let result = match request {
            Request::Run {
                lease,
                argv,
                env,
                limits,
                deadline_ms,
            } => {
                let started = self.run_limited(&lease, argv, env, fds, limits);
                if let (Ok(child), Some(after)) = (&started, deadline_ms) {
                    self.set_deadline(&lease, child.id(), Duration::from_millis(after));
                }
                return started.map_or_else(refused, Outcome::Running);
            }
            Request::Watch { lease } => {
                return self
                    .watch(&lease)
                    .map_or_else(refused, |pid| Outcome::Watching(lease, pid));
            }
            _ if !fds.is_empty() => Err("only run takes descriptors".to_owned()),
            Request::UserCreate { grant: Some(_), .. } => Err(NO_ADMINISTRATOR.to_owned()),
            Request::UserCreate { lease, grant: None } => self
                .user_create(&lease)
                .map(|uid| Reply::Created { uid })
                .map_err(|why| why.to_string()),
            Request::PoolCreate => Ok(pool_reply(self.pool_create())),
            Request::PoolStatus => Ok(pool_reply(self.pool_status())),
            Request::FullDiskAccess => Ok(Reply::FullDiskAccess {
                state: self.full_disk_access(),
            }),
            Request::KillUid { lease } => self.kill_uid(&lease).map(|()| Reply::Killed),
            Request::Stop { lease, grace_ms } => self
                .stop(&lease, Duration::from_millis(grace_ms))
                .map(|()| Reply::Killed),
            Request::UserDelete { lease } => self.user_delete(&lease).map(|existed| {
                self.forget_run(&lease);
                Reply::Deleted { existed }
            }),
            Request::BootReset { held } => self.boot_reset(&held).map(|reset| Reply::Reset {
                kept: None,
                deleted: reset
                    .deleted
                    .iter()
                    .map(|lease| Dotted(*lease).to_string())
                    .collect(),
            }),
            Request::Scan => self.scan().map(leak::scanned),
        };
        result.map_or_else(refused, Outcome::Reply)
    }

    /// `full-disk-access`: whether the helper holds Full Disk Access, as a listing of the
    /// privacy database's directory shows ([`full_disk_access::judge`]). Logs the answer
    /// each time, and the error of an undetermined one.
    pub fn full_disk_access(&self) -> FullDiskAccess {
        let (state, why) = full_disk_access::judge(self.host.list_privacy_dir());
        match why {
            None => tracing::info!("full disk access: {}", state.as_str()),
            Some(why) => tracing::warn!("full disk access: {} ({why})", state.as_str()),
        }
        state
    }

    /// `user-create <lease>`: the lease takes a free pool user, never an administrator,
    /// whose home folder is empty and only it can enter. Returns its uid.
    ///
    /// # Errors
    /// [`LeaseError::Exhausted`] when no pool user is free (retry once a lease ends);
    /// otherwise why it was refused or failed.
    pub fn user_create(&self, lease: &str) -> Result<u32, LeaseError> {
        let lease = parse_lease(lease)?;
        let mut ledger = self.lock();
        if ledger.get(lease).is_some() {
            return Err(format!(
                "lease {} was used before; a lease id is used once",
                Dotted(lease)
            )
            .into());
        }
        self.take_slot(&mut ledger, lease)
    }

    fn live(&self, uid: u32) -> Result<Vec<LiveProcess>, String> {
        self.host
            .live_processes(uid)
            .map_err(|why| format!("listing the processes of uid {uid}: {why}"))
    }

    /// The uid of a live lease, in range.
    pub(super) fn live_uid(&self, ledger: &Ledger, lease: LeaseId) -> Result<u32, String> {
        match ledger.get(lease) {
            None => Err(format!("lease {} has no user", Dotted(lease))),
            Some(Entry { deleted: true, .. }) => Err(format!(
                "lease {} is over: its user was deleted or returned to the pool",
                Dotted(lease)
            )),
            Some(Entry { uid, .. }) if !self.settings.range.contains(uid) => Err(format!(
                "lease {}'s uid {uid} is outside {}",
                Dotted(lease),
                self.settings.range
            )),
            Some(Entry { uid, .. }) => Ok(uid),
        }
    }

    /// `run <lease> <fd-passed argv>`: starts the process as the lease's user, outside
    /// any login session. The descriptors are stdin, stdout, stderr and the lease
    /// directory.
    ///
    /// # Errors
    /// Why it was refused, or why the process did not start.
    pub fn run(
        &self,
        lease: &str,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        fds: Vec<OwnedFd>,
    ) -> Result<Child, String> {
        self.run_limited(lease, argv, env, fds, None)
    }

    /// [`Self::run`] with `limits` set on the process (`setrlimit`, in the child before it
    /// executes), when there are any.
    ///
    /// # Errors
    /// As [`Self::run`]; also a limit that is zero or above its cap
    /// ([`crate::limits::Limits::check`]).
    pub fn run_limited(
        &self,
        lease: &str,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        fds: Vec<OwnedFd>,
        limits: Option<Limits>,
    ) -> Result<Child, String> {
        let lease = parse_lease(lease)?;
        spawn::check(&argv, &env)?;
        spawn::check_limits(limits.as_ref())?;
        let count = fds.len();
        let [stdin, stdout, stderr, cwd]: [OwnedFd; 4] = fds.try_into().map_err(|_| {
            format!(
                "run takes stdin, stdout, stderr and the lease directory; got {count} descriptors"
            )
        })?;
        let ledger = self.lock();
        let LeaseUser {
            name: user, uid, ..
        } = self.lease_user(&ledger, lease)?;
        let program = argv[0].clone();
        let child = spawn::spawn(Spawn {
            uid,
            gid: self.settings.gid,
            home: self.settings.homes.join(&user),
            user,
            argv,
            env,
            stdin,
            stdout,
            stderr,
            cwd,
            limits,
        })
        .map_err(|why| format!("starting {program}: {why}"))?;
        drop(ledger);
        tracing::info!(lease = %Dotted(lease), uid, pid = child.id(), "started {program}");
        self.runs.started(lease, child.id(), None);
        Ok(child)
    }
}

/// The reply to `pool-create` and `pool-status`.
fn pool_reply(status: PoolStatus) -> Reply {
    let indexes = |slots: Vec<Slot>| slots.into_iter().map(Slot::index).collect();
    Reply::Pool(Box::new(PoolState {
        size: status.size,
        free: indexes(status.free),
        leased: status
            .leased
            .into_iter()
            .map(|(slot, lease)| (slot.index(), Dotted(lease).to_string()))
            .collect(),
        resetting: indexes(status.resetting),
        broken: indexes(status.broken),
        missing: indexes(status.missing),
        errors: status.errors,
    }))
}

/// Makes the home folder `name` in `homes` if it is not there, and gives it (made now or
/// found) to the user: owner `uid`, group `gid`, mode 0700, and the user flags, ACL and
/// extended attributes a lease set on it taken off (the system's own, which it protects,
/// stay). An existing home is kept, never removed: macOS refuses the removal of a folder
/// of `/Users` to a process without Full Disk Access, so the pool's homes are emptied
/// and kept (the caller empties it first). Never through a link. `homes` is root's to
/// write, so the directory just made is still the one opened.
///
/// # Errors
/// `homes` cannot be opened, the folder could not be made or is not a directory, or it
/// could not be given away or scrubbed.
pub fn make_home(homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent = rustix::fs::openat(CWD, homes, flags, Mode::empty())?;
    match rustix::fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(why) => return Err(why.into()),
    }
    let home = rustix::fs::openat(&parent, name, flags, Mode::empty())?;
    rustix::fs::fchown(&home, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
    rustix::fs::fchmod(&home, Mode::from_raw_mode(0o700))?;
    crate::sweep::scrub_home(&home)
}

#[cfg(test)]
mod tests;
