//! `kill-uid` and `user-delete`: the end of a lease user, and the errors their steps
//! fail with ([`TeardownError`], worded for the refusal at the protocol boundary).
//!
//! The order the helper's header describes: `kill-uid` kills, then boots out those of
//! the uid's launchd domains that launchd has, until neither a process nor a domain is left; `user-delete` asks for
//! the uid's `DARWIN_USER_DIR` while the record is there, settles (no process, no
//! domain, and still no process after the bootout), sweeps, removes the uid's entry
//! under the per-user folders, settles again, and only then deletes the record. A step
//! that fails leaves the lease live in the ledger, so its uid is held.

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
    /// Processes of the uid outlived every kill round.
    #[error("{}", remaining(*.uid, &format!("after {KILL_ROUNDS} rounds"), .left))]
    Survived { uid: u32, left: Vec<LiveProcess> },
    /// Launchd domains of the uid outlived every kill round.
    #[error("launchd domains of uid {uid} remain after {KILL_ROUNDS} rounds: {}", .stayed.join(", "))]
    DomainsSurvived { uid: u32, stayed: Vec<String> },
    /// Processes of the uid did not exit within a look of `user-delete`.
    #[error("{}; kill-uid first", remaining(*.uid, &format!("after {EXIT_LOOKS} looks"), .left))]
    StillRunning { uid: u32, left: Vec<LiveProcess> },
    /// Processes of the uid were seen after a bootout of `user-delete`.
    #[error(
        "{}; they started while its launchd domains were booted out, so the delete stops here",
        remaining(*.uid, "after the bootout", .started)
    )]
    StartedDuringBootout { uid: u32, started: Vec<LiveProcess> },
    /// Launchd domains of the uid stayed at a look of `user-delete`.
    #[error("launchd domains of uid {uid} remain: {}; kill-uid first", .stayed.join(", "))]
    DomainsLeft { uid: u32, stayed: Vec<String> },
    /// Launchd could not say whether it has the domain.
    #[error("asking launchd for {domain}: {source}")]
    Launchd { domain: String, source: io::Error },
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

/// The words `launchctl print` writes to standard error for a domain launchd does not
/// have.
const NO_DOMAIN_TEXT: &str = "Could not find domain";

/// The exit statuses of `launchctl print` that, with [`NO_DOMAIN_TEXT`], mean launchd
/// does not have the domain. 112 is launchd's "Could not find specified domain": on
/// macOS 26 `launchctl print gui/<uid>` for a pool user, which never logs in and so has
/// no GUI domain, exits 112 and writes "Bad request." and "Could not find domain for
/// user gui: <uid>". 113 with the same words is read as absence too.
const NO_DOMAIN_CODES: [i32; 2] = [112, 113];

/// What `answer` (how `launchctl print <domain>` ended) says: whether launchd has the
/// domain. Exit 0 is a domain that exists; only launchd's answer for a missing domain
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

/// The GUI and the user launchd domain of `uid`, in the order they are booted out.
fn domains(uid: u32) -> [String; 2] {
    [format!("gui/{uid}"), format!("user/{uid}")]
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
    /// `kill-uid <lease>`: kills every process of the uid, then boots out those of its
    /// GUI and user launchd domains that launchd has (so launchd restarts none of its
    /// agents, such as `distnoted`; a domain launchd does not have, such as the GUI
    /// domain of a user that never logged in, is skipped), and repeats both until no
    /// process and neither domain is left; then
    /// removes the user's crontab and `at` jobs (so no job of the user starts after
    /// kill-uid returns: one would start a process `user-delete` refuses), and kills and
    /// boots out again, for a job that fired before they went.
    ///
    /// # Errors
    /// Why it was refused, processes or a domain that survived [`KILL_ROUNDS`] rounds,
    /// launchd failing to say whether it has a domain, or a crontab or `at` job that
    /// could not be removed (after the second kill).
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
        self.kill_rounds(uid)?;
        // Swept only once nothing of the user runs (the sweep's contract).
        let schedules = Self::swept(sweep(&self.settings.schedules, name, uid), name);
        self.kill_rounds(uid)?;
        schedules.map(drop)
    }

    /// Kills every process of `uid`, then boots out those of its GUI and user domains
    /// that launchd has, until no process and neither domain is left, for at most
    /// [`KILL_ROUNDS`] rounds.
    fn kill_rounds(&self, uid: u32) -> Result<(), TeardownError> {
        let mut left = Vec::new();
        let mut stayed = Vec::new();
        for _ in 0..KILL_ROUNDS {
            self.host
                .kill_all(uid)
                .map_err(|source| TeardownError::Kill { uid, source })?;
            let refused = self.boot_out(uid)?;
            left = self.processes(uid)?;
            if left.is_empty() {
                stayed = self.domains_left(uid, &refused)?;
                if stayed.is_empty() {
                    return Ok(());
                }
            }
            self.host.pause();
        }
        match left.is_empty() {
            true => Err(TeardownError::DomainsSurvived { uid, stayed }),
            false => Err(TeardownError::Survived { uid, left }),
        }
    }

    /// The live processes of `uid`.
    fn processes(&self, uid: u32) -> Result<Vec<LiveProcess>, TeardownError> {
        self.host
            .live_processes(uid)
            .map_err(|source| TeardownError::Listing { uid, source })
    }

    /// Whether launchd has `domain`, from `launchctl print` ([`launchctl_print_answer`]).
    ///
    /// # Errors
    /// `launchctl` could not be run, or failed other than by saying the domain is not
    /// there.
    pub(super) fn domain_exists(&self, domain: &str) -> io::Result<bool> {
        launchctl_print_answer(&self.host.launchctl_print(domain)?)
    }

    /// [`Self::domain_exists`], its error naming the domain.
    fn has_domain(&self, domain: &str) -> Result<bool, TeardownError> {
        self.domain_exists(domain)
            .map_err(|source| TeardownError::Launchd {
                domain: domain.to_owned(),
                source,
            })
    }

    /// `launchctl bootout` of each of the GUI and the user domain of `uid` that launchd
    /// has; a domain it does not have is not booted out. Returns why each bootout that
    /// failed did (one that went between the look and the bootout fails too, which
    /// [`Self::domains_left`] tells apart).
    ///
    /// # Errors
    /// launchd could not say whether it has a domain.
    fn boot_out(&self, uid: u32) -> Result<Vec<(String, String)>, TeardownError> {
        let mut refused = Vec::new();
        for domain in domains(uid) {
            if !self.has_domain(&domain)? {
                continue;
            }
            if let Err(why) = self.host.bootout(&domain) {
                tracing::debug!("launchctl bootout {domain}: {why}");
                refused.push((domain, why.to_string()));
            }
        }
        Ok(refused)
    }

    /// The domains of `uid` that launchd still has, each with why its bootout failed
    /// (from `refused`), if it did.
    fn domains_left(
        &self,
        uid: u32,
        refused: &[(String, String)],
    ) -> Result<Vec<String>, TeardownError> {
        let mut left = Vec::new();
        for domain in domains(uid) {
            if self.has_domain(&domain)? {
                left.push(match refused.iter().find(|(d, _)| *d == domain) {
                    Some((_, why)) => format!("{domain} (bootout: {why})"),
                    None => domain,
                });
            }
        }
        Ok(left)
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
    /// first and the last look the uid's launchd domains are booted out too, one that
    /// stays refuses the delete, and the processes are looked at again after the
    /// bootout (a job launchd started before it went). Each look waits up to
    /// [`EXIT_LOOKS`] listings for the processes it finds to exit: a cron job that
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
    /// folders, a process or a launchd domain of the uid remains, or the sweep or the
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

    /// [`Self::none_left`], the uid's launchd domains booted out, and one more look:
    /// refused while a process or a domain of the uid is left, and refused at once (no
    /// wait) when any process of the uid is seen after the bootout. Such a process is a
    /// job launchd started before its domain went; it may have written again what the
    /// sweep removed, so it is not waited out: the delete stops, the uid stays held, and
    /// the repeated delete sweeps everything again.
    fn settled(&self, uid: u32) -> Result<(), TeardownError> {
        self.none_left(uid)?;
        let refused = self.boot_out(uid)?;
        let stayed = self.domains_left(uid, &refused)?;
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
