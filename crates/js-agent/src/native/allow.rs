//! The allow-list read model in the native runtime (`--read-model allow`, [`crate::mac_allow`]):
//! the start-up self-test, which [`NativeRuntime::self_test`] runs after its run of
//! `/usr/bin/true`, and the read grants of a run that pins a bundle.
//!
//! The self-test lends an account of the pool under a lease of its own, written to the
//! journal first as a run's is, makes the account's run directory, lays the fixture out
//! ([`mac_allow::plan`]) and runs each step of the [`SelfTest`] through the supervisor as that
//! account, one after the other, each with its records and its three standard streams in a
//! directory of its own below `self-test-allow` in `--native-accounts` (which no run may
//! read). A step whose end is not read within the self-test's polls counts as failed; its
//! processes end with the account. Then the canaries outside the account's home are removed
//! and the account is retired, which empties its home and the fixture with it. A pass is kept
//! as the runtime's assurance for this process only; a failure is the self-test's error, and
//! the agent does not start.
//!
//! A run that pins a bundle under the model gets the grants of [`mac_allow::grants`] (the
//! bundle's trees, the kinds its config declares, and the ready Xcode its start names) and,
//! when it has run files, a read grant of their directory, which the profile keeps
//! unwritable; a run that pins none keeps the open model.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use js_mac_pool::seatbelt::{
    Assurance, MacosMajor, Outcome, PrivilegedScan, ReadModel, SelfTest, Step,
};
use js_mac_pool::xcode::HostXcode;

use super::profile::Reads;
use super::start::{RECORDS_DIR, SELF_TEST_PAUSE, SELF_TEST_POLLS, fresh_dir};
use super::{NativeRuntime, accounts, fail, logs, make_dir, profile};
use crate::bundle::Pinned;
use crate::facts::CommandRunner;
use crate::lifecycle::RuntimeError;
use crate::mac_accounts::Account;
use crate::mac_allow::{self, Places, Plan};
use crate::mac_supervisor::{Ended, LaunchRequest, SuperviseSpec, Supervised};
use crate::xcode::{Site, Xcode};

/// The directory below `--native-accounts` the allow self-test keeps its steps' records and
/// streams in.
const SCRATCH_DIR: &str = "self-test-allow";

/// Where macOS keeps the folder every user may write and share.
const USERS_SHARED: &str = "/Users/Shared";

/// The temporary folder of `/var`, spelled as the kernel resolves it.
const VAR_TMP: &str = "/private/var/tmp";

/// What the allow read model needs of the host, given to the runtime with
/// [`NativeRuntime::with_read_allow`].
#[derive(Debug, Clone)]
pub struct ReadAllow {
    /// The host's macOS version, which has a base set.
    pub major: MacosMajor,
    /// The ready Xcodes: a run that names one of their builds is granted it, and the
    /// self-test compiles and signs with the first.
    pub xcodes: Vec<HostXcode>,
    /// The host's setuid and setgid files below the base set's directories.
    pub scan: PrivilegedScan,
    /// The folder every user may write (`/Users/Shared`), where a canary is placed.
    pub shared: PathBuf,
    /// `/private/var/tmp`, where a canary is placed.
    pub var_tmp: PathBuf,
}

impl ReadAllow {
    /// This host's: its macOS version ([`mac_allow::major_of`]), the ready Xcodes among
    /// `xcodes` (the ones the hello offers, put to the catalog again for their proof objects,
    /// [`crate::xcode::hosts`]), and its privileged files ([`MacosMajor::scan`]: the agent runs
    /// as root, so it reads every directory).
    ///
    /// # Errors
    ///
    /// The version cannot be read or has no base set: the host refuses the allow model.
    pub fn of_host(runner: &dyn CommandRunner, xcodes: &[Xcode]) -> Result<Self, String> {
        let major = mac_allow::major_of(runner)?;
        Ok(Self {
            major,
            xcodes: crate::xcode::hosts(runner, xcodes, Site::system().owner),
            scan: major.scan(),
            shared: PathBuf::from(USERS_SHARED),
            var_tmp: PathBuf::from(VAR_TMP),
        })
    }
}

impl NativeRuntime {
    /// The same runtime, with `allow` as what the allow read model needs of the host. A
    /// runtime whose config asks for the model ([`ReadModel::Allow`]) and was given none fails
    /// its self-test.
    #[must_use]
    pub fn with_read_allow(self, allow: ReadAllow) -> Self {
        Self {
            read_allow: Some(allow),
            ..self
        }
    }

    /// The same runtime holding `assurance` as if its self-test had passed: how a test over the
    /// fake launcher, which applies no profile, reads the profile of a bundle run under the
    /// model. An agent never calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn with_assurance(self, assurance: Assurance) -> Self {
        Self {
            assurance: Some(assurance),
            ..self
        }
    }

    /// Whether the allow read model's self-test passed in this process, so a run that pins a
    /// bundle gets a profile under that model.
    #[must_use]
    pub fn read_allow_assured(&self) -> bool {
        self.assurance.is_some()
    }

    /// Runs the allow read model's self-test (see the module documentation) and keeps its
    /// assurance.
    ///
    /// # Errors
    ///
    /// The runtime has no [`ReadAllow`], the account cannot be lent or retired, the fixture
    /// cannot be placed, or a step did not do what its role says (every such step named).
    pub(super) fn allow_self_test(&mut self) -> Result<(), RuntimeError> {
        let Some(allow) = self.read_allow.clone() else {
            return Err(RuntimeError::new(
                "--read-model allow: the runtime was given nothing of the host for the allow \
                 read model",
            ));
        };
        let Some(pool) = self.pool.clone() else {
            return Ok(());
        };
        let lease = format!("{}.{}", self.term, self.next);
        self.next += 1;
        self.journal
            .add(&lease)
            .map_err(|why| fail(&"the lease journal", &why))?;
        let account = match pool.lend(&accounts::job_of(&lease)) {
            Ok(lent) => lent.account,
            Err(why) => {
                let _ = self.journal.done(&lease);
                return Err(fail(&"the account pool", &why));
            }
        };
        let tested = self.allow_steps(&allow, &account);
        let held = accounts::retire(&pool, account).err();
        if held.is_none() {
            let _ = self.journal.done(&lease);
        }
        match (tested, held) {
            (Ok(assurance), None) => {
                self.assurance = Some(assurance);
                Ok(())
            }
            (Ok(_), Some(held)) => Err(held),
            (Err(why), held) => Err(accounts::with_held(why, held)),
        }
    }

    /// The self-test's steps in the home of `account`, with what they need placed first and
    /// the canaries outside the home removed after.
    fn allow_steps(&self, allow: &ReadAllow, account: &Account) -> Result<Assurance, RuntimeError> {
        let home = self.config.homes.join(&account.name);
        let run = profile::run_dir(&home);
        make_dir(&run, account.uid, &self.hand_over).map_err(|why| fail(&run.display(), &why))?;
        let places = Places {
            home,
            run,
            shared: allow.shared.clone(),
            var_tmp: allow.var_tmp.clone(),
            tag: self.term.to_string(),
        };
        let xcode = allow.xcodes.first().cloned();
        let read_denies = &self.config.read_denies;
        let Plan {
            fixture,
            plants,
            outside,
        } = mac_allow::plan(&places, read_denies, xcode, allow.scan.clone())
            .map_err(RuntimeError::new)?;
        let tested = mac_allow::place(&plants)
            .map_err(RuntimeError::new)
            .and_then(|()| {
                SelfTest::new(allow.major, fixture)
                    .map_err(|why| fail(&"the allow read model's self-test", &why))
            })
            .and_then(|test| {
                let outcomes = self.run_steps(&test, account)?;
                mac_allow::verdict(&test, &outcomes).map_err(RuntimeError::new)
            });
        for why in mac_allow::remove(&outside) {
            tracing::warn!("native: the allow read model's self-test left a canary: {why}");
        }
        tested
    }

    /// What each step of `test` did, run one after the other as `account`.
    fn run_steps(&self, test: &SelfTest, account: &Account) -> Result<Vec<Outcome>, RuntimeError> {
        let scratch = self.config.accounts.state.join(SCRATCH_DIR);
        fresh_dir(&scratch).map_err(|why| fail(&scratch.display(), &why))?;
        test.steps()
            .iter()
            .enumerate()
            .map(|(i, step)| self.run_step(step, account, &scratch.join(i.to_string())))
            .collect()
    }

    /// What `step` did, run through the supervisor as `account`, its records and streams in
    /// `dir`.
    fn run_step(
        &self,
        step: &Step,
        account: &Account,
        dir: &Path,
    ) -> Result<Outcome, RuntimeError> {
        let records = dir.join(RECORDS_DIR);
        for made in [dir, records.as_path()] {
            fs::create_dir(made)
                .and_then(|()| fs::set_permissions(made, fs::Permissions::from_mode(0o700)))
                .map_err(|why| fail(&made.display(), &why))?;
        }
        let [stdin, stdout, stderr] = logs::open(dir).map_err(RuntimeError::new)?;
        let command = mac_allow::command_of(step);
        let env = step.env().to_vec();
        let request = LaunchRequest {
            spec: SuperviseSpec {
                uid: account.uid,
                gid: account.gid,
                new_session: self.new_session,
                profile: command.profile,
                params: command.params,
                program: command.program,
                args: command.args,
                max_file_bytes: self.config.max_file_bytes,
                max_processes: self.config.max_processes.saturating_add(1),
                record_dir: records.display().to_string(),
                parent: Some(std::process::id()),
            },
            env: &env,
            cwd: Path::new(step.cwd()),
            stdio: [stdin.into(), stdout.into(), stderr.into()],
        };
        let mut supervised = self
            .launcher
            .launch(request)
            .map_err(|why| fail(&"the supervisor", &why))?;
        let ended = match supervised.wait_ready() {
            Ok(_) => self.wait_end(&mut *supervised)?,
            Err(ended) => Some(ended),
        };
        // A stream that cannot be read holds no refusal: the step then reads as failed.
        let written = fs::read_to_string(dir.join(logs::STDERR)).unwrap_or_default();
        Ok(ended.map_or(Outcome::Failed, |ended| {
            mac_allow::outcome(&ended, &written)
        }))
    }

    /// How the started step of `supervised` ended, or none when its end was not read within
    /// the self-test's polls.
    fn wait_end(&self, supervised: &mut dyn Supervised) -> Result<Option<Ended>, RuntimeError> {
        for _ in 0..self.self_test_polls.unwrap_or(SELF_TEST_POLLS) {
            let record = supervised
                .try_exit()
                .map_err(|why| fail(&"the supervisor", &why))?;
            if let Some(record) = record {
                return Ok(Some(record.ended));
            }
            std::thread::sleep(SELF_TEST_PAUSE);
        }
        Ok(None)
    }

    /// What a run's profile reads by: the open model for a run that pins no bundle, or under
    /// `--read-model open`; under `allow`, the assurance and the grants of `pinned`, with the
    /// ready Xcode of build `xcode` when the start names one.
    ///
    /// # Errors
    ///
    /// The self-test has not passed in this process (an agent that could not run it at start
    /// runs no bundle under the model), the Xcode is not ready, or a tree cannot be named.
    pub(super) fn reads_of(
        &self,
        pinned: Option<&Pinned>,
        xcode: Option<&str>,
    ) -> Result<Reads, RuntimeError> {
        let pinned = match (self.config.read_model, pinned) {
            (ReadModel::Allow, Some(pinned)) => pinned,
            _ => return Ok(Reads::Open),
        };
        let Some(assurance) = self.assurance else {
            return Err(RuntimeError::new(
                "the allow read model: its self-test has not passed in this process",
            ));
        };
        let hosts = self
            .read_allow
            .as_ref()
            .map_or(&[][..], |allow| allow.xcodes.as_slice());
        let xcode = mac_allow::xcode_of(hosts, xcode).map_err(RuntimeError::new)?;
        let grants =
            mac_allow::grants(pinned.root(), pinned.config(), xcode).map_err(RuntimeError::new)?;
        Ok(Reads::Allow(assurance, grants))
    }
}
