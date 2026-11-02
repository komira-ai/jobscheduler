//! `kill-uid` and `user-delete`: the end of a lease user, and the errors their steps
//! fail with ([`TeardownError`], worded for the refusal at the protocol boundary).
//!
//! Both are judged by their outcome, never by what a `launchctl` call answered: no
//! process of the uid is left, and the uid's launchd domains no longer print. A
//! `launchctl bootout` is asked for and its failure only logged; `launchctl print` is
//! read as printed (exit 0) or not (any other exit status), so no exit status of it is
//! matched.
//!
//! The order the helper's header describes: `kill-uid` looks once whether the uid has a
//! GUI login session (a `loginwindow` process of its own; a pool user never has one),
//! then kills and boots out its user domain (and its GUI domain, only if it has the
//! session), until neither a process nor a domain is left; `user-delete` asks for the
//! uid's `DARWIN_USER_DIR` while the record is there, settles (no process, the user
//! domain booted out and no longer printing, and still no process after the bootout),
//! sweeps, removes the uid's entry under the per-user folders, settles again, and only
//! then deletes the record. A step that fails leaves the lease live in the ledger, so
//! its uid is held (quarantined: handed to no other lease until a delete succeeds).

use std::io;
use std::path::PathBuf;

use js_types::LeaseId;

use super::folders::{self, NotAnEntry};
use super::{EXIT_LOOKS, Helper, KILL_ROUNDS, LiveProcess, NAMED, PrintAnswer};
use crate::autologin::{Intent, read_intent};
use crate::lease::{Dotted, parse_lease, user_name};
use crate::ledger::Ledger;
use crate::sweep::{SweepPlan, Swept, owned_entries, remove_owned, sweep};

/// Why a step of `kill-uid` or `user-delete` failed. Its text is the refusal the caller
/// reads.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TeardownError {
    /// The lease cannot be acted on (its id, its state in the ledger, the session
    /// switch's intent record), as the helper's other checks word it.
    #[error("{0}")]
    Refused(String),
    /// The kill could not be sent.
    #[error("killing the processes of uid {uid}: {source}")]
    Kill { uid: u32, source: io::Error },
    /// The process table could not be read.
    #[error("listing the processes of uid {uid}: {source}")]
    Listing { uid: u32, source: io::Error },
    /// The outcome of `kill-uid` did not hold after its last attempt: processes of the
    /// uid, or launchd domains that still print (each with why it could not be told
    /// gone, or why its last bootout failed), or both.
    #[error("{}", outlived(*.uid, .left, .stayed))]
    Outlived {
        uid: u32,
        left: Vec<LiveProcess>,
        stayed: Vec<String>,
    },
    /// Processes of the uid did not exit within a look of `user-delete`.
    #[error("{}; kill-uid first", remaining(*.uid, &format!("after {EXIT_LOOKS} looks"), .left))]
    StillRunning { uid: u32, left: Vec<LiveProcess> },
    /// Processes of the uid were seen after a bootout of `user-delete`.
    #[error(
        "{}; they started while its launchd domains were booted out, so the delete stops here",
        remaining(*.uid, "after the bootout", .started)
    )]
    StartedDuringBootout { uid: u32, started: Vec<LiveProcess> },
    /// Launchd domains of the uid still printed at a look of `user-delete`.
    #[error("launchd domains of uid {uid} remain: {}; kill-uid first", .stayed.join(", "))]
    DomainsLeft { uid: u32, stayed: Vec<String> },
    /// The user's record could not be looked up.
    #[error("looking up user {name}: {source}")]
    Lookup { name: String, source: io::Error },
    /// The uid's `DARWIN_USER_DIR` could not be had.
    #[error("asking for the DARWIN_USER_DIR of uid {uid}: {source}")]
    UserDir { uid: u32, source: io::Error },
    /// The `DARWIN_USER_DIR` answer names no entry of the per-user folders.
    #[error(transparent)]
    NotAnEntry(#[from] NotAnEntry),
    /// The per-user folders could not be searched for the uid's entries.
    #[error("searching {} for the entries of uid {uid}: {source}", .folders.display())]
    Folders {
        uid: u32,
        folders: PathBuf,
        source: io::Error,
    },
    /// Something of the user could not be removed.
    #[error("the sweep for {name} is incomplete: {}", .errors.join("; "))]
    Sweep { name: String, errors: Vec<String> },
    /// The user's record could not be deleted.
    #[error("deleting user {name}: {source}")]
    Delete { name: String, source: io::Error },
    /// The ledger could not record the deletion.
    #[error("recording the deletion of lease {lease}: {source}")]
    Record { lease: String, source: io::Error },
}

impl From<TeardownError> for String {
    fn from(error: TeardownError) -> Self {
        error.to_string()
    }
}

/// `<count> processes of uid <uid> remain <when>: <pid> (<command>), ...`, naming at
/// most [`NAMED`].
fn remaining(uid: u32, when: &str, left: &[LiveProcess]) -> String {
    let mut named: Vec<String> = left
        .iter()
        .take(NAMED)
        .map(|p| format!("{} ({})", p.pid, p.command))
        .collect();
    if left.len() > NAMED {
        named.push(format!("and {} more", left.len() - NAMED));
    }
    format!(
        "{} processes of uid {uid} remain {when}: {}",
        left.len(),
        named.join(", ")
    )
}

/// The text of [`TeardownError::Outlived`]: what remains of `uid` after the last attempt
/// (the processes `left`, the domains that `stayed`), and that the uid is quarantined.
fn outlived(uid: u32, left: &[LiveProcess], stayed: &[String]) -> String {
    let when = format!("after {KILL_ROUNDS} attempts");
    let mut parts = Vec::new();
    if !left.is_empty() {
        parts.push(remaining(uid, &when, left));
    }
    if !stayed.is_empty() {
        parts.push(format!(
            "launchd domains of uid {uid} remain {when}: {}",
            stayed.join(", ")
        ));
    }
    format!(
        "{}; uid {uid} is quarantined: no other lease gets it until a delete succeeds",
        parts.join("; ")
    )
}

/// The command name of the process the login window runs as the user of each GUI login
/// session; a uid that owns one has a session (and a `gui/<uid>` launchd domain).
const LOGINWINDOW: &str = "loginwindow";

/// Whether `launchctl print` printed the domain (`answer`): exit 0 is a domain launchd
/// has. Any other exit status is a domain it does not print; no exit status is matched
/// as "not found", because launchd's answer for a domain it does not have varies
/// (112, 113 and 125 have each been seen for the `gui/<uid>` of a user that never
/// logged in).
///
/// # Errors
/// A signal ended `launchctl`, so it gave no answer.
fn printed(answer: &PrintAnswer) -> io::Result<bool> {
    match answer.code {
        Some(code) => Ok(code == 0),
        None => Err(io::Error::other(format!(
            "launchctl print was ended by a signal: {}",
            answer.stderr.trim()
        ))),
    }
}

/// The launchd domains of `uid` its teardown boots out and judges, in order: the GUI
/// domain only when the uid has a GUI login session (`gui`), then the user domain.
fn domains(uid: u32, gui: bool) -> Vec<String> {
    let user = format!("user/{uid}");
    match gui {
        true => vec![format!("gui/{uid}"), user],
        false => vec![user],
    }
}

/// Reports that the user of `lease`, with `uid`, stays after a failed `kill-uid` or
/// `user-delete`: the lease is live in the ledger, so the uid is held and handed to no
/// other lease until a delete succeeds.
fn kept(lease: LeaseId, uid: u32, why: &TeardownError) {
    tracing::warn!(
        lease = %Dotted(lease),
        uid,
        "the lease user stays and uid {uid} is held until a delete succeeds: {why}"
    );
}

impl Helper {
    /// `kill-uid <lease>`: looks once, before anything is killed, whether the uid has a
    /// GUI login session (it owns a `loginwindow` process; a pool user never logs in, so
    /// it has none and its `gui/<uid>` is never asked about). Then, for at most
    /// [`KILL_ROUNDS`] attempts, kills every process of the uid and boots out its user
    /// domain (and its GUI domain, if it has the session), so launchd restarts none of
    /// its agents, such as `distnoted`; a bootout's failure is only logged. Each attempt
    /// ends with a look at the outcome: no process of the uid is left and none of those
    /// domains prints. Then it removes the user's crontab and `at` jobs (so no job of the
    /// user starts after kill-uid returns: one would start a process `user-delete`
    /// refuses), and does the attempts again, for a job that fired before they went.
    ///
    /// # Errors
    /// Why it was refused, an outcome that did not hold after the last attempt (the uid
    /// is then quarantined: held, named with what remains), a kill that could not be
    /// sent or a process table that could not be read, or a crontab or `at` job that
    /// could not be removed (after the second attempts).
    pub fn kill_uid(&self, lease: &str) -> Result<(), String> {
        let lease = parse_lease(lease)?;
        let ledger = self.lock();
        Ok(self.kill_locked(&ledger, lease)?)
    }

    /// [`Self::kill_uid`], with the ledger's lock held.
    pub(super) fn kill_locked(&self, ledger: &Ledger, lease: LeaseId) -> Result<(), TeardownError> {
        let uid = self
            .live_uid(ledger, lease)
            .map_err(TeardownError::Refused)?;
        self.kill_account(&user_name(lease), uid)
            .inspect_err(|why| kept(lease, uid, why))?;
        tracing::info!(lease = %Dotted(lease), uid, "no process of the lease user is left");
        Ok(())
    }

    /// Kills every process of `uid` and boots out its domains, as [`Self::kill_uid`]
    /// describes, for the account `name`.
    pub(super) fn kill_account(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        // Looked at before the kill, which ends the session's `loginwindow`.
        let gui = self.gui_session(uid)?;
        tracing::info!(uid, gui, "tearing down uid {uid}");
        let domains = domains(uid, gui);
        self.kill_rounds(uid, &domains)?;
        // Swept only once nothing of the user runs (the sweep's contract).
        let schedules = Self::swept(sweep(&self.settings.schedules, name, uid), name);
        self.kill_rounds(uid, &domains)?;
        schedules.map(drop)
    }

    /// Whether `uid` has a GUI login session: one of its live processes is the login
    /// window's ([`LOGINWINDOW`]).
    fn gui_session(&self, uid: u32) -> Result<bool, TeardownError> {
        Ok(self
            .processes(uid)?
            .iter()
            .any(|p| p.command == LOGINWINDOW))
    }

    /// For at most [`KILL_ROUNDS`] attempts: kills every process of `uid`, boots out
    /// each of `domains`, and looks at the outcome, until no process is left and none of
    /// `domains` prints.
    fn kill_rounds(&self, uid: u32, domains: &[String]) -> Result<(), TeardownError> {
        let mut left = Vec::new();
        let mut stayed = Vec::new();
        for _ in 0..KILL_ROUNDS {
            self.host
                .kill_all(uid)
                .map_err(|source| TeardownError::Kill { uid, source })?;
            let refused = self.boot_out(domains);
            left = self.processes(uid)?;
            stayed = self.still_printing(domains, &refused);
            if left.is_empty() && stayed.is_empty() {
                return Ok(());
            }
            self.host.pause();
        }
        Err(TeardownError::Outlived { uid, left, stayed })
    }

    /// The live processes of `uid`.
    fn processes(&self, uid: u32) -> Result<Vec<LiveProcess>, TeardownError> {
        self.host
            .live_processes(uid)
            .map_err(|source| TeardownError::Listing { uid, source })
    }

    /// Whether launchd prints `domain` ([`printed`]), as the teardown judges it: a
    /// domain wrongly read as gone is caught by the look at the processes, so any exit
    /// status but 0 is gone. The readiness checks read `launchctl print` more strictly
    /// ([`Self::domain_exists`]).
    ///
    /// # Errors
    /// `launchctl` could not be run, or a signal ended it.
    fn domain_prints(&self, domain: &str) -> io::Result<bool> {
        printed(&self.host.launchctl_print(domain)?)
    }

    /// `launchctl bootout` of each of `domains`. A bootout's own exit status is no
    /// verdict (it fails for a domain that is not there, and can fail after it ended the
    /// domain): each failure is logged and returned with its domain, for the report of a
    /// domain that still prints.
    fn boot_out(&self, domains: &[String]) -> Vec<(String, String)> {
        let mut refused = Vec::new();
        for domain in domains {
            if let Err(why) = self.host.bootout(domain) {
                tracing::info!("launchctl bootout {domain} (the outcome decides): {why}");
                refused.push((domain.clone(), why.to_string()));
            }
        }
        refused
    }

    /// Those of `domains` that are not known to be gone: each that `launchctl print`
    /// printed (with why its bootout failed, from `refused`, if it did), or that it gave
    /// no answer for (with why).
    fn still_printing(&self, domains: &[String], refused: &[(String, String)]) -> Vec<String> {
        let mut left = Vec::new();
        for domain in domains {
            match self.domain_prints(domain) {
                Ok(false) => {}
                Ok(true) => left.push(match refused.iter().find(|(d, _)| d == domain) {
                    Some((_, why)) => format!("{domain} (bootout: {why})"),
                    None => domain.clone(),
                }),
                Err(why) => left.push(format!("{domain} (launchctl print: {why})")),
            }
        }
        left
    }

    /// `user-delete <lease>`: sweeps what the user leaves, deletes its record and
    /// records the deletion. Returns whether a record was there to delete; a lease
    /// already deleted returns `false` and touches nothing.
    ///
    /// The uid's entry under the per-user folders is taken from its `DARWIN_USER_DIR`,
    /// asked first, while the record is there ([`super::Host::user_dir`]); an answer that
    /// is not an entry of [`super::Settings::user_folders`] is refused, never guessed at,
    /// and the entry is removed whole with the rest of the sweep. When the record is gone
    /// already (a delete repeated after it went, or a record removed by someone else),
    /// there is no `DARWIN_USER_DIR` of the uid's own to ask for: every entry of the
    /// per-user folders that the uid owns is removed instead.
    ///
    /// No process of the uid may be left at any of three looks: before anything is
    /// removed (so nothing of the user races the walk), once the crontab and `at` jobs
    /// are gone (a job that started after `kill-uid` is found here, and none can start
    /// later), and after the whole sweep, just before the record goes (so no process
    /// outlives its user as an orphan uid that later leases' files are open to). At the
    /// first and the last look the uid's user domain is booted out too (its bootout's
    /// failure only logged), one that still prints refuses the delete, and the processes
    /// are looked at again after the bootout (a job launchd started before it went).
    /// The GUI domain is not asked about: with no process of the uid left there is no
    /// login session (`kill-uid` ended any, and judged its domain). Each look waits up
    /// to [`EXIT_LOOKS`] listings for the processes it finds to exit: a cron job that
    /// started between `kill-uid` and this delete ends by itself, and one that does not
    /// is named in the refusal. The ledger lock is held throughout, so a refused delete
    /// holds back the other verbs for up to about five seconds per look.
    ///
    /// Refused while the session switch's intent record names the lease: its user is
    /// the one the login window logs in.
    ///
    /// # Errors
    /// Why it was refused: the intent record names the lease or cannot be read, the
    /// uid's `DARWIN_USER_DIR` could not be asked or is not an entry of the per-user
    /// folders, a process or the user domain of the uid remains, or the sweep or the
    /// deletion failed (the lease stays live, its uid held, so the delete can be
    /// repeated).
    pub fn user_delete(&self, lease: &str) -> Result<bool, String> {
        let lease = parse_lease(lease)?;
        let mut ledger = self.lock();
        if ledger.get(lease).is_some_and(|entry| entry.deleted) {
            return Ok(false);
        }
        if read_intent(&self.settings.session.intent)? == Some(Intent::Lease(lease)) {
            return Err(format!(
                "lease {} holds the GUI session (auto-login); session-idle first",
                Dotted(lease)
            ));
        }
        Ok(self.delete_locked(&mut ledger, lease)?)
    }

    /// [`Self::user_delete`] of a live lease, with the ledger's lock held.
    pub(super) fn delete_locked(
        &self,
        ledger: &mut Ledger,
        lease: LeaseId,
    ) -> Result<bool, TeardownError> {
        let uid = self
            .live_uid(ledger, lease)
            .map_err(TeardownError::Refused)?;
        self.delete_steps(ledger, lease, uid)
            .inspect_err(|why| kept(lease, uid, why))
    }

    /// The steps of [`Self::user_delete`] for the live lease `lease` with `uid`.
    fn delete_steps(
        &self,
        ledger: &mut Ledger,
        lease: LeaseId,
        uid: u32,
    ) -> Result<bool, TeardownError> {
        let name = user_name(lease);
        let entries = self.user_entries(&name, uid)?;
        self.settled(uid)?;
        let mut removed = Self::swept(sweep(&self.settings.schedules, &name, uid), &name)?;
        self.none_left(uid)?;
        let mut plan: SweepPlan = self.settings.sweep.clone();
        plan.named
            .push(format!("{}/{{user}}", self.settings.homes.display()));
        removed += Self::swept(sweep(&plan, &name, uid), &name)?;
        for entry in &entries {
            removed += Self::swept(remove_owned(entry, uid), &name)?;
        }
        self.settled(uid)?;
        let existed = self
            .host
            .delete_user(&name)
            .map_err(|source| TeardownError::Delete {
                name: name.clone(),
                source,
            })?;
        ledger
            .record_deleted(lease)
            .map_err(|source| TeardownError::Record {
                lease: Dotted(lease).to_string(),
                source,
            })?;
        tracing::info!(lease = %Dotted(lease), uid, removed, existed, "deleted lease user {name}");
        Ok(existed)
    }

    /// The entries under [`super::Settings::user_folders`] of the user `name` with `uid`:
    /// the one its `DARWIN_USER_DIR` names while its record exists; once the record is
    /// gone, every entry there that the uid owns.
    fn user_entries(&self, name: &str, uid: u32) -> Result<Vec<PathBuf>, TeardownError> {
        let folders_root = &self.settings.user_folders;
        let account = self
            .host
            .account(name)
            .map_err(|source| TeardownError::Lookup {
                name: name.to_owned(),
                source,
            })?;
        if account.is_none() {
            return owned_entries(folders_root, uid).map_err(|source| TeardownError::Folders {
                uid,
                folders: folders_root.clone(),
                source,
            });
        }
        let dir = self
            .host
            .user_dir(uid, self.settings.gid)
            .map_err(|source| TeardownError::UserDir { uid, source })?;
        Ok(vec![folders::entry(&dir, folders_root)?])
    }

    /// [`Self::none_left`], the uid's user domain booted out, and one more look: refused
    /// while a process is left or the domain still prints, and refused at once (no wait)
    /// when any process of the uid is seen after the bootout. Such a process is a job
    /// launchd started before its domain went; it may have written again what the sweep
    /// removed, so it is not waited out: the delete stops, the uid stays held, and the
    /// repeated delete sweeps everything again.
    fn settled(&self, uid: u32) -> Result<(), TeardownError> {
        self.none_left(uid)?;
        let domains = domains(uid, false);
        let refused = self.boot_out(&domains);
        let stayed = self.still_printing(&domains, &refused);
        if !stayed.is_empty() {
            return Err(TeardownError::DomainsLeft { uid, stayed });
        }
        let started = self.processes(uid)?;
        match started.is_empty() {
            true => Ok(()),
            false => Err(TeardownError::StartedDuringBootout { uid, started }),
        }
    }

    /// Waits, for at most [`EXIT_LOOKS`] listings, until no process of `uid` is
    /// alive; refuses, naming the processes, if some still are.
    fn none_left(&self, uid: u32) -> Result<(), TeardownError> {
        let mut left = self.processes(uid)?;
        let mut looks = 1;
        while !left.is_empty() && looks < EXIT_LOOKS {
            self.host.pause();
            left = self.processes(uid)?;
            looks += 1;
        }
        match left.is_empty() {
            true => Ok(()),
            false => Err(TeardownError::StillRunning { uid, left }),
        }
    }

    /// What the sweep `swept` for the user `name` removed, or why it is incomplete.
    fn swept(swept: Swept, name: &str) -> Result<usize, TeardownError> {
        match swept.errors.is_empty() {
            true => Ok(swept.removed),
            false => Err(TeardownError::Sweep {
                name: name.to_owned(),
                errors: swept.errors,
            }),
        }
    }
}

#[cfg(test)]
mod tests;
