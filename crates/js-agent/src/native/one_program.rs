//! The one-program engine of the native runtime: what the root helper did for a run, done by the
//! agent itself (`docs/design/mac-one-program.md`, `docs/design/mac-one-program-prototype.md`).
//!
//! - **The user.** An account the slot pool lends (`crate::mac_accounts::Pool`), never used for a
//!   job before; the lease's run directory is under its home, as under a pool user's.
//! - **The start.** The agent starts its own program in the start mode (`__mac-job-start`,
//!   `crate::mac_job::start`), with the run's environment, working directory and streams and a
//!   start file only root reads. That process, with one thread, opens a new audit session, drops
//!   to the account, applies the run's Seatbelt profile (the profile the helper's run would have
//!   had, `native/profile.rs`, applied with `sandbox_init_with_parameters` in place of
//!   `sandbox-exec`) and executes the command in its place, undisclaimed: the job answers to the
//!   privacy controls as the agent (the owner's decision).
//! - **The end.** A stop is `SIGTERM` to the run's process group; a kill is the sweep of the
//!   job's audit session and of its account (`crate::mac_job::macos::sweep_job`); the deletion
//!   of the user is the pool's retire, which ends the uid, deletes the account and shows it gone.
//!
//! Not done here yet: adoption after an agent restart (a restart retires every account the
//! pool's ledger holds as lent), and the helper's leak scan.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_mac_pool::seatbelt::Profile;
use js_mac_session::client::{ClientError, Exit};
use js_mac_session::disclaim::Program;

use crate::mac_accounts::{Account, Pool, Slots};
use crate::mac_job::start::{StartFile, VERB as START_VERB};
use crate::mac_job::sweep::JobIds;

/// What the engine holds of one lease.
struct Lease {
    account: Account,
    /// The started run's pid, its process group's id too.
    pid: Option<u32>,
    /// Where the start writes the run's audit session.
    asid_dir: Option<PathBuf>,
}

/// The engine: the pool and the leases it lent.
pub struct OneProgram {
    pool: Arc<Pool>,
    leases: Mutex<BTreeMap<String, Lease>>,
}

impl std::fmt::Debug for OneProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneProgram").finish_non_exhaustive()
    }
}

/// The job id the pool records for `lease` (`<term>.<seq>`): its `.` written `-`.
fn job_of(lease: &str) -> String {
    lease.replace('.', "-")
}

/// A refusal the native runtime reads as the helper's.
fn refused(why: impl std::fmt::Display) -> ClientError {
    ClientError::Refused(why.to_string())
}

impl OneProgram {
    /// Opens the slot pool in `state`, with the slots `slots` and `ready` accounts kept ready.
    /// Every account a crashed agent left lent is retired at the open.
    ///
    /// # Errors
    /// The pool's own: no Full Disk Access, an unusable ledger, an account it could not make or
    /// delete.
    pub fn open(state: &Path, slots: Slots, ready: usize) -> Result<Self, String> {
        let pool = Pool::open(state, slots, ready, &std::collections::BTreeSet::new())
            .map_err(|why| format!("the account pool: {why}"))?;
        Ok(Self {
            pool: Arc::new(pool),
            leases: Mutex::new(BTreeMap::new()),
        })
    }

    fn leases(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Lease>> {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lends an account to `lease`: its uid.
    pub fn user_create(&self, lease: &str) -> Result<u32, ClientError> {
        let lent = self.pool.lend(&job_of(lease)).map_err(refused)?;
        let uid = lent.account.uid;
        self.leases().insert(
            lease.to_owned(),
            Lease {
                account: lent.account,
                pid: None,
                asid_dir: None,
            },
        );
        Ok(uid)
    }

    /// The name of the account `lease` holds.
    pub fn user_name(&self, lease: &str) -> Result<String, ClientError> {
        self.leases()
            .get(lease)
            .map(|held| held.account.name.clone())
            .ok_or_else(|| refused(format!("lease {lease} has no user")))
    }

    /// Starts the run of `lease`: `command` under `profile`, in `cwd`, with `env` and the first
    /// three of `fds` as its streams; the start's files go to `scratch`.
    pub fn run(
        &self,
        lease: &str,
        (profile, command): (&Profile, &[String]),
        env: &[(String, String)],
        fds: [BorrowedFd<'_>; 4],
        (cwd, scratch): (&Path, &Path),
    ) -> Result<Started, ClientError> {
        let (uid, gid) = {
            let leases = self.leases();
            let held = leases
                .get(lease)
                .ok_or_else(|| refused(format!("lease {lease} has no user")))?;
            (held.account.uid, held.account.gid)
        };
        let (program, args) = command
            .split_first()
            .ok_or_else(|| refused("an empty command"))?;
        let dir = scratch.join("start");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).map_err(ClientError::Io)?;
        let start = StartFile {
            uid,
            gid,
            new_session: true,
            asid_file: dir.join("asid").display().to_string(),
            profile: profile.text().to_owned(),
            params: profile.params().to_vec(),
            program: program.clone(),
            args: args.to_vec(),
            disclaim: false,
        };
        let path = dir.join("start.json");
        let text = serde_json::to_string(&start).map_err(refused)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(ClientError::Io)?;
        let me = std::env::current_exe().map_err(ClientError::Io)?;
        let stream = |fd: BorrowedFd<'_>| fd.try_clone_to_owned().map(File::from).map(Stdio::from);
        let mut start = Program::new(
            &me,
            [
                std::ffi::OsStr::new(START_VERB),
                std::ffi::OsStr::new("--start"),
                path.as_os_str(),
            ],
        );
        start.keep_responsibility();
        start
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(cwd)
            .process_group(0)
            .stdin(stream(fds[0]).map_err(ClientError::Io)?)
            .stdout(stream(fds[1]).map_err(ClientError::Io)?)
            .stderr(stream(fds[2]).map_err(ClientError::Io)?);
        let child = start.spawn().map_err(ClientError::Io)?;
        let pid = child.id();
        if let Some(held) = self.leases().get_mut(lease) {
            held.pid = Some(pid);
            held.asid_dir = Some(dir);
        }
        Ok(Started::Local(child))
    }

    /// `SIGTERM` to the run's process group, then the sweep once `grace` has passed.
    pub fn stop(self: &Arc<Self>, lease: &str, grace: Duration) -> Result<(), ClientError> {
        let pid = self.leases().get(lease).and_then(|held| held.pid);
        if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: `kill` takes plain values and touches no memory of ours.
            let _ = unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
        let engine = Arc::clone(self);
        let lease = lease.to_owned();
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            let _ = engine.kill_uid(&lease);
        });
        Ok(())
    }

    /// Ends every process of the lease's run: its audit session's and its account's.
    pub fn kill_uid(&self, lease: &str) -> Result<(), ClientError> {
        let (uid, asid_dir) = {
            let leases = self.leases();
            let held = leases
                .get(lease)
                .ok_or_else(|| refused(format!("lease {lease} has no user")))?;
            (held.account.uid, held.asid_dir.clone())
        };
        let ids = JobIds {
            uid,
            asid: asid_dir
                .as_deref()
                .and_then(crate::mac_job::macos::read_asid),
            own_pid: std::process::id().cast_signed(),
        };
        let tally = crate::mac_job::macos::sweep_job(&ids);
        for swept in tally.by_session.iter().chain(&tally.by_user_only) {
            tracing::info!("native {lease}: ended {} ({})", swept.pid, swept.name);
        }
        match tally.error {
            Some(why) => Err(refused(why)),
            None => Ok(()),
        }
    }

    /// Retires the account of `lease`: the pool ends its uid, deletes it and shows it gone.
    pub fn user_delete(&self, lease: &str) -> Result<(), ClientError> {
        let held = self
            .leases()
            .remove(lease)
            .ok_or_else(|| refused(format!("lease {lease} has no user")))?;
        match self.pool.retire(held.account) {
            Ok(done) => {
                tracing::info!("native {lease}: account retired ({done:?})");
                Ok(())
            }
            Err(held) => Err(refused(held)),
        }
    }
}

/// A started run, of the helper or of this engine.
pub enum Started {
    /// The helper's run.
    Helper(js_mac_session::client::Running),
    /// This engine's start process, the job's program after its `exec`.
    Local(Child),
}

impl Started {
    /// The run's pid.
    #[must_use]
    pub fn pid(&self) -> u32 {
        match self {
            Self::Helper(running) => running.pid,
            Self::Local(child) => child.id(),
        }
    }

    /// Waits for the run to end.
    ///
    /// # Errors
    /// The helper's connection failed, or the wait did.
    pub fn wait(self) -> Result<Exit, ClientError> {
        use std::os::unix::process::ExitStatusExt as _;
        match self {
            Self::Helper(running) => running.wait(),
            Self::Local(mut child) => child.wait().map_err(ClientError::Io).map(|status| Exit {
                code: status.code(),
                signal: status.signal(),
            }),
        }
    }
}
