//! The verbs: `user-create`, `run`, `kill-uid` and `user-delete` (S4.2), in [`pool`] the
//! pool of lease users (`pool-create`, `pool-status`, and what `user-create` and
//! `user-delete` do with it), in [`switch`] the session switch and the boot reset, and in
//! [`leak`] the leak scan's `scan`, `baseline` and `reboot-dirty`, over a [`Host`] that
//! does what only the OS can (user records, launchd, processes, the programs the switch
//! and the scan run), so the rules here are tested on any platform.
//!
//! Rules every verb keeps:
//! - A lease id is parsed strictly and is used once.
//! - A lease's user is a pool user, `kbf-lease-pool-<slot>`, lent for the lease and
//!   reset (or replaced) when it ends ([`pool`]); or, for a lease a release before the
//!   pool created, its own `kbf-lease-<term>-<seq>`, deleted when it ends.
//! - The uid comes from the ledger, never from a lookup, and must be in the lease
//!   range; a uid outside it is never touched.
//! - `user-create` is refused for a lease id used before (so a grant is single-use),
//!   takes only a free pool user (never making one: a full pool is
//!   [`LeaseError::Exhausted`]) and makes an administrator only with a valid grant.
//! - `run`, `kill-uid` and `user-delete` act only on a lease the ledger holds; `run`
//!   and `kill-uid` refuse one that is over (its uid may be another lease's now).
//! - `run` in the GUI session, and `session-ready`, act only for the lease the session
//!   switch's intent record names (the one `session-login` logged in); `run` there is
//!   refused until launchd has the user's `gui/<uid>` domain, which exists once the
//!   login has completed. `session-ready` answers whether it does.
//! - `kill-uid` is judged by its outcome, never by what a `launchctl` call answered. It
//!   decides, before the kill, whether the uid may have a GUI login session: the session
//!   switch's intent record names it, it owns a `loginwindow` process (or one whose name
//!   cannot be read), or an earlier teardown left its GUI domain owed (a record in the
//!   state directory, kept until a teardown judges the domain gone). A pool user has none
//!   of these, so its `gui/<uid>` is never asked about. Then, for at most
//!   [`KILL_ROUNDS`] attempts, it kills every process of the
//!   uid and boots out `user/<uid>` (and `gui/<uid>`, only with the session), the
//!   bootout's own failure only logged, and looks again: done once no process of the
//!   uid is left and none of those domains prints (`launchctl print` exits non-zero).
//!   launchd restarts a per-user agent such as `distnoted` for as long as the domain
//!   exists, and a live process of the uid makes the domain again. It then removes the
//!   user's crontab and `at` jobs, and does it all again, so no scheduled job of the
//!   user starts after it returns. An outcome that does not hold after the last attempt
//!   fails, naming what remains, and the lease stays live: its uid is quarantined.
//! - `user-delete` of a pool user's lease records the lease over and resets the user;
//!   a reset that is not clean replaces it, and a replacement that fails breaks the slot
//!   ([`pool`]). The deletion of a user (a lease's from before the pool, or a pool user
//!   being replaced) refuses while any process of the uid remains. It first asks for the
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
//!   those still alive when it refuses. `user-delete` is refused for the lease the
//!   session switch's intent record names (`session-idle` first).
//! - The first `user-create` closes the baseline window, and makes a dirty scan's
//!   verdict stale (`reboot-dirty` then needs a new scan). Making the pool does not.
//! - The standing VM user ([`role`], `--vm-user`) is the one account besides lease users
//!   the helper acts on. Its verbs (`role-run`, `role-session-ready`, `role-kill-uid`)
//!   take no lease: the account is looked up by its configured name, and its uid must be
//!   an ordinary uid outside the lease range. `role-run` and `role-session-ready` answer
//!   for the GUI session only while no lease holds it.
//! - The image verbs ([`image`]: `image-setup`, `image-harvest`) run the one installed
//!   `js-vmm` as root on a bundle directly inside the staging or images directory, with
//!   argv the helper builds; the caller names only the bundle (and the output directory).
//!   A base the setup built stays root's, sealed.
//! - `role-run` starts one program, the configured `js-vmm`, and only as `vm-run --spec
//!   <file>` with the file inside the images directory ([`vmrun`]). `role-reap` ends the
//!   process group of such a run, found again by pid, start time and boot ([`reap`]).
//! - Every verb but `boot-reset` is refused until the boot reset has succeeded.
//!
//! The mutating verbs run one at a time (one lock around the ledger), and `run` holds
//! it while it starts the process, so no process starts for a lease being deleted.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use js_types::LeaseId;
use rustix::fs::{CWD, Gid, Mode, OFlags, Uid};

use crate::autologin::{CommandOutput, Intent, SessionPaths, read_intent};
use crate::grant::{self, Expect, GrantKeys};
use crate::lease::{Dotted, PoolSize, Slot, UidRange, VmUser, parse_lease};
use crate::ledger::{Entry, Ledger};
use crate::proto::{Limits, PoolState, Reply, Request, Session};
use crate::scan::ScanSettings;
use crate::spawn::{self, Spawn};
use crate::sweep::SweepPlan;

pub(crate) mod folders;
mod image;
mod ipc;
mod leak;
mod pool;
mod reap;
mod role;
mod switch;
mod teardown;
mod vmrun;

pub use image::{CHFLAGS, ImageSettings};
pub use ipc::{IPCRM, IPCS};
pub use pool::{DSEDITGROUP, LeaseError, POOL_EXHAUSTED, PoolExhausted, PoolStatus};
pub use switch::Reset;

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
    /// What it wrote to standard error.
    pub stderr: String,
}

/// What the OS does for the helper. The macOS implementation is in `crate::macos`.
pub trait Host: Send + Sync {
    /// The wall clock, for grant expiry.
    fn now(&self) -> SystemTime;
    /// Whether any user record has `uid`.
    ///
    /// # Errors
    /// The directory could not be asked.
    fn uid_taken(&self, uid: u32) -> io::Result<bool>;
    /// Makes the user's home folder ([`make_home`] on a real host).
    ///
    /// # Errors
    /// The folder exists or could not be made.
    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()>;
    /// Creates the user record.
    ///
    /// # Errors
    /// The record could not be created.
    fn create_user(&self, user: &NewUser) -> io::Result<()>;
    /// Deletes the user record `name` (and any group membership it has); `false` when
    /// there is none.
    ///
    /// # Errors
    /// The record could not be deleted.
    fn delete_user(&self, name: &str) -> io::Result<bool>;
    /// How `launchctl print <domain>` ended: its exit code and its standard error. The
    /// teardown reads exit 0 as a domain launchd has and any other exit status as one it
    /// does not print; the readiness checks take only launchd's not-found answer as
    /// absence.
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
    /// Gives the user `name` a fresh random password and returns it, for
    /// `/etc/kcpassword`; no one else is told it.
    ///
    /// # Errors
    /// There is no such user, or the password could not be set.
    fn reset_password(&self, name: &str) -> io::Result<String>;
    /// Clones the open file `src` as the new file `name` in the directory `dir`: a
    /// copy-on-write copy (macOS: `fclonefileat` with `CLONE_NOOWNCOPY`) with a new inode,
    /// owned by the caller, so a descriptor someone holds on `src` reaches nothing of it.
    ///
    /// # Errors
    /// `name` exists, the volume cannot clone, or the copy failed.
    fn clone_file(
        &self,
        src: std::os::fd::BorrowedFd<'_>,
        dir: std::os::fd::BorrowedFd<'_>,
        name: &str,
    ) -> io::Result<()>;
    /// Clears every user flag (`uchg` among them) of the open file `file` (macOS:
    /// `fchflags(fd, 0)`), as root. A clone of a flagged file is flagged.
    ///
    /// # Errors
    /// The flags could not be cleared.
    fn clear_user_flags(&self, file: std::os::fd::BorrowedFd<'_>) -> io::Result<()>;
    /// This boot's identifier (macOS: `kern.bootsessionuuid`), for the baseline
    /// window.
    ///
    /// # Errors
    /// The kernel did not say.
    fn boot_session(&self) -> io::Result<String>;
    /// Every process in the table, zombies included.
    ///
    /// # Errors
    /// The process table could not be read.
    fn processes(&self) -> io::Result<Vec<Process>>;
    /// Sends `SIGKILL` to the process group `pgid` (`killpg`); a group with no process
    /// is not an error.
    ///
    /// # Errors
    /// The signal could not be sent.
    fn kill_group(&self, pgid: i32) -> io::Result<()>;
}

/// A process as the table shows it, for `role-reap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Process {
    /// Its pid.
    pub pid: i32,
    /// Its process group.
    pub pgid: i32,
    /// Its effective uid.
    pub uid: u32,
    /// Its real uid.
    pub ruid: u32,
    /// When it started, in microseconds of the epoch: with `pid`, it names one process
    /// for its whole life.
    pub start: u64,
    /// Exited but not yet reaped: it cannot be signalled further.
    pub zombie: bool,
}

/// The ids of an account record the helper did not create (the VM user's).
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
    /// Whether the user is in the `admin` group.
    pub admin: bool,
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
    /// uid's own entry here, named by its `DARWIN_USER_DIR`, is removed whole by
    /// `user-delete`.
    pub user_folders: PathBuf,
    /// The gate's grant keys; without them no administrator is ever made.
    pub grant_keys: Option<GrantKeys>,
    /// This Mac's serial number, which a grant must name.
    pub serial: String,
    /// Where the session switch keeps its state.
    pub session: SessionPaths,
    /// The standing VM user, when this Mac is provisioned for VMs: the Mac rests at it
    /// (`session-idle`) and the `role-*` verbs act for it. `None` for a Mac that serves
    /// whole-machine leases only, which rests at the login window.
    pub vm_user: Option<VmUser>,
    /// What the leak scan looks at, and where it keeps its baseline and window.
    pub scan: ScanSettings,
    /// The images directory and the installed programs of `image-setup` and
    /// `image-harvest`.
    pub image: ImageSettings,
}

/// What a request comes to.
#[derive(Debug)]
pub enum Outcome {
    /// One reply, and the request is done.
    Reply(Reply),
    /// A process started; the caller reports its exit.
    Running(Child),
}

/// The helper: settings, the host, and the ledger behind one lock.
pub struct Helper {
    host: Box<dyn Host>,
    settings: Settings,
    ledger: Mutex<Ledger>,
    /// Set, under the ledger's lock, once the boot reset has succeeded.
    reset_done: AtomicBool,
    /// Set, under the ledger's lock, when the last scan came back dirty and no lease
    /// user was created since: the one state `reboot-dirty` is accepted in.
    dirty: AtomicBool,
    /// Held while an image verb runs: one at a time, apart from the ledger's lock, which
    /// a minutes-long `js-vmm` must not hold back the other verbs with.
    images: Mutex<()>,
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
            dirty: AtomicBool::new(false),
            images: Mutex::new(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Carries out `request`. Only `run` takes descriptors, and nothing but
    /// `boot-reset` is done before the boot reset has succeeded.
    pub fn handle(&self, request: Request, fds: Vec<OwnedFd>) -> Outcome {
        let refused = |reason: String| Outcome::Reply(Reply::Refused { reason });
        if !self.reset_done() && !matches!(request, Request::BootReset { .. }) {
            return refused(RESET_NEEDED.to_owned());
        }
        let result = match request {
            Request::Run {
                lease,
                argv,
                env,
                session,
                limits,
            } => {
                return self
                    .run_limited(&lease, argv, env, fds, session, limits)
                    .map_or_else(refused, Outcome::Running);
            }
            Request::RoleRun { argv, env } => {
                return self
                    .role_run(argv, env, fds)
                    .map_or_else(refused, Outcome::Running);
            }
            _ if !fds.is_empty() => Err("only run and role-run take descriptors".to_owned()),
            Request::UserCreate { lease, grant } => self
                .user_create(&lease, grant.as_deref())
                .map(|uid| Reply::Created { uid })
                .map_err(|why| why.to_string()),
            Request::PoolCreate => Ok(pool_reply(self.pool_create())),
            Request::PoolStatus => Ok(pool_reply(self.pool_status())),
            Request::KillUid { lease } => self.kill_uid(&lease).map(|()| Reply::Killed),
            Request::UserDelete { lease } => self
                .user_delete(&lease)
                .map(|existed| Reply::Deleted { existed }),
            Request::SessionLogin { lease } => {
                self.session_login(&lease).map(|()| Reply::Restarting)
            }
            Request::SessionIdle => self.session_idle().map(|()| Reply::Restarting),
            Request::RoleSessionReady => self
                .role_session_ready()
                .map(|ready| Reply::SessionReady { ready }),
            Request::RoleKillUid => self.role_kill_uid().map(|()| Reply::Killed),
            Request::RoleReap { pgid, start, boot } => {
                self.role_reap(pgid, start, &boot).map(|()| Reply::Killed)
            }
            Request::ImageSetup { bundle } => self
                .image_setup(&bundle)
                .map(|report| Reply::Imaged { report }),
            Request::ImageHarvest { bundle, out } => self
                .image_harvest(&bundle, &out)
                .map(|report| Reply::Imaged { report }),
            Request::SessionReady { lease } => self
                .session_ready(&lease)
                .map(|ready| Reply::SessionReady { ready }),
            Request::BootReset { held } => self.boot_reset(&held).map(|reset| Reply::Reset {
                kept: reset.kept.map(|lease| Dotted(lease).to_string()),
                deleted: reset
                    .deleted
                    .iter()
                    .map(|lease| Dotted(*lease).to_string())
                    .collect(),
            }),
            Request::Scan => self.scan().map(leak::scanned),
            Request::Baseline => self.baseline().map(|classes| Reply::Baselined { classes }),
            Request::RebootDirty => self.reboot_dirty().map(|()| Reply::Restarting),
        };
        result.map_or_else(refused, Outcome::Reply)
    }

    /// `user-create <lease> [grant]`: the lease takes a free pool user (an administrator,
    /// replaced for the lease, with a valid grant), whose home folder is empty and only it
    /// can enter. Returns its uid.
    ///
    /// # Errors
    /// [`LeaseError::Exhausted`] when no pool user is free (retry once a lease ends);
    /// otherwise why it was refused or failed.
    pub fn user_create(&self, lease: &str, grant: Option<&str>) -> Result<u32, LeaseError> {
        let lease = parse_lease(lease)?;
        let admin = match grant {
            None => false,
            Some(grant) => {
                let keys = self.settings.grant_keys.as_ref().ok_or(
                    "this Mac holds no gate key, so no lease user is ever an administrator",
                )?;
                let expect = Expect {
                    serial: &self.settings.serial,
                    lease: &Dotted(lease).to_string(),
                    now: self.host.now(),
                };
                grant::verify(grant, keys, expect)?;
                true
            }
        };
        let mut ledger = self.lock();
        if ledger.get(lease).is_some() {
            return Err(format!(
                "lease {} was used before; a lease id is used once",
                Dotted(lease)
            )
            .into());
        }
        self.take_slot(&mut ledger, lease, admin)
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
        self.run_in(lease, argv, env, fds, Session::Background)
    }

    /// [`Self::run`] in `session`. In the GUI session, refused unless the intent record
    /// names the lease and its `gui/<uid>` domain exists.
    ///
    /// # Errors
    /// Why it was refused, or why the process did not start.
    pub fn run_in(
        &self,
        lease: &str,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        fds: Vec<OwnedFd>,
        session: Session,
    ) -> Result<Child, String> {
        self.run_limited(lease, argv, env, fds, session, None)
    }

    /// [`Self::run_in`] with `limits` set on the process (`setrlimit`, in the child before
    /// it executes), when there are any. Limits are refused in the GUI session.
    ///
    /// # Errors
    /// As [`Self::run_in`]; also a limit that is zero or above its cap
    /// ([`crate::limits::Limits::check`]), or limits asked for a GUI run.
    pub fn run_limited(
        &self,
        lease: &str,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        fds: Vec<OwnedFd>,
        session: Session,
        limits: Option<Limits>,
    ) -> Result<Child, String> {
        let lease = parse_lease(lease)?;
        spawn::check(&argv, &env)?;
        spawn::check_limits(session, limits.as_ref())?;
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
        if session == Session::Gui && !self.gui_up(lease, uid)? {
            return Err(format!(
                "gui/{uid} does not exist: the login of lease {}'s user has not completed",
                Dotted(lease)
            ));
        }
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
            session,
            limits,
        })
        .map_err(|why| format!("starting {program}: {why}"))?;
        drop(ledger);
        tracing::info!(lease = %Dotted(lease), uid, pid = child.id(), ?session, "started {program}");
        Ok(child)
    }

    /// `session-ready <lease>`: whether the GUI session `session-login` started for the
    /// lease is up, its `gui/<uid>` domain there.
    ///
    /// # Errors
    /// The lease has no live user, the intent record names another lease (or none) or
    /// cannot be read, or launchd could not be asked.
    pub fn session_ready(&self, lease: &str) -> Result<bool, String> {
        let lease = parse_lease(lease)?;
        let ledger = self.lock();
        let uid = self.live_uid(&ledger, lease)?;
        self.gui_up(lease, uid)
    }

    /// Whether `lease`, whose user has `uid`, holds the GUI session and its domain is
    /// up; refused when the intent record does not name it.
    fn gui_up(&self, lease: LeaseId, uid: u32) -> Result<bool, String> {
        if read_intent(&self.settings.session.intent)? != Some(Intent::Lease(lease)) {
            return Err(format!(
                "lease {} does not hold the GUI session; session-login first",
                Dotted(lease)
            ));
        }
        let domain = format!("gui/{uid}");
        self.domain_exists(&domain)
            .map_err(|why| format!("asking launchd for {domain}: {why}"))
    }

    /// Whether launchd has `domain`, for a readiness check ([`launchctl_print_answer`]).
    ///
    /// # Errors
    /// `launchctl` could not be run, or failed other than by saying the domain is not
    /// there.
    pub(super) fn domain_exists(&self, domain: &str) -> io::Result<bool> {
        launchctl_print_answer(&self.host.launchctl_print(domain)?)
    }
}

/// The words `launchctl print` writes to standard error for a domain launchd does not
/// have.
const NO_DOMAIN_TEXT: &str = "Could not find domain";

/// The exit statuses of `launchctl print` that, with [`NO_DOMAIN_TEXT`], mean launchd
/// does not have the domain. 112 is launchd's "Could not find specified domain": on
/// macOS 26 `launchctl print gui/<uid>` for a pool user, which never logs in and so has
/// no GUI domain, exits 112 and writes "Bad request." and "Could not find domain for
/// user gui: <uid>". 113 with the same words is read as absence too.
const NO_DOMAIN_CODES: [i32; 2] = [112, 113];

/// What `answer` (how `launchctl print <domain>` ended) says to a readiness check
/// (`session-ready`, `role-session-ready`, `role-run`, the session switch's rest at the
/// VM user): whether launchd has the domain. A "no" there has effects (the switch logs
/// the VM user in again), so it is read strictly; the teardown reads `launchctl print`
/// by whether it printed, in `teardown`. Exit 0 is a domain that exists; only launchd's answer for a missing domain
/// (an exit status in [`NO_DOMAIN_CODES`] together with [`NO_DOMAIN_TEXT`] on standard
/// error, never the words alone) is absence. Any other failure (refused, another exit,
/// killed, launchd not answering) is an error, so it is never read as a domain that is
/// gone.
///
/// # Errors
/// `launchctl` failed in any other way.
fn launchctl_print_answer(answer: &PrintAnswer) -> io::Result<bool> {
    let stderr = answer.stderr.trim();
    match answer.code {
        Some(0) => Ok(true),
        Some(code) if NO_DOMAIN_CODES.contains(&code) && stderr.contains(NO_DOMAIN_TEXT) => {
            Ok(false)
        }
        Some(code) => Err(io::Error::other(format!(
            "launchctl print exited {code}: {stderr}"
        ))),
        None => Err(io::Error::other(format!(
            "launchctl print was ended by a signal: {stderr}"
        ))),
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

/// Makes the home folder `name` in `homes`: new (an existing one is an error), owned
/// by the user, mode 0700. Never through a link. `homes` is root's to write, so the
/// directory just made is still the one opened.
///
/// # Errors
/// `homes` cannot be opened, the folder exists, or it could not be given away.
pub fn make_home(homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent = rustix::fs::openat(CWD, homes, flags, Mode::empty())?;
    rustix::fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700))?;
    let home = rustix::fs::openat(&parent, name, flags, Mode::empty())?;
    rustix::fs::fchown(&home, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
    rustix::fs::fchmod(&home, Mode::from_raw_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests;
