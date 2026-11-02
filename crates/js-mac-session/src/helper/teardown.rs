//! `kill-uid` and `user-delete`: the end of a lease user, and the errors their steps
//! fail with ([`TeardownError`], worded for the refusal at the protocol boundary).
//!
//! Both are judged by their outcome, never by what a `launchctl` call answered: no
//! process of the uid is left, and the uid's launchd domains no longer print. A
//! `launchctl bootout` is asked for and its failure only logged; `launchctl print` is
//! read as printed (exit 0) or not (any other exit status), so no exit status of it is
//! matched.
//!
//! The order the helper's header describes: `kill-uid` decides whether the uid may have
//! a GUI login session (the intent record names it, it owns a `loginwindow` process or
//! one whose name cannot be read, or an earlier teardown left its GUI domain owed; a
//! pool user has none of these), then kills and boots out its user domain (and its GUI
//! domain, only with a session), until neither a process nor a domain is left. A GUI
//! domain is owed, in a record in the helper's state directory, from the first look
//! that sees the session until a teardown judges the domain gone, so neither a retry
//! (the kill ended the `loginwindow` that showed the session) nor a restart of the
//! helper forgets it. `user-delete` asks for the uid's `DARWIN_USER_DIR` while the
//! record is there, settles (no process, the user domain, and the GUI domain while
//! owed, booted out and no longer printing, and still no process after the bootout),
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
    /// `lease` is whether the uid is a lease's (quarantined by the ledger) or the VM
    /// user's.
    #[error("{}", outlived(*.uid, .left, .stayed, *.lease))]
    Outlived {
        uid: u32,
        left: Vec<LiveProcess>,
        stayed: Vec<String>,
        lease: bool,
    },
    /// The record that the uid's GUI domain is still owed a teardown could not be read,
    /// written or removed.
    #[error("the GUI teardown record of uid {uid} ({}): {source}", .path.display())]
    GuiRecord {
        uid: u32,
        path: PathBuf,
        source: io::Error,
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
/// (the processes `left`, the domains that `stayed`), and what happens to the uid: a
/// lease's (`lease`) is quarantined; the VM user's stays as it is, for `role-kill-uid`
/// to be repeated.
fn outlived(uid: u32, left: &[LiveProcess], stayed: &[String], lease: bool) -> String {
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
    let then = match lease {
        true => format!("uid {uid} is quarantined: no other lease gets it until a delete succeeds"),
        false => "role-kill-uid can be repeated".to_owned(),
    };
    format!("{}; {then}", parts.join("; "))
}

/// The command name of the process the login window runs as the user of each GUI login
/// session; a uid that owns one has a session (and a `gui/<uid>` launchd domain).
const LOGINWINDOW: &str = "loginwindow";

/// The command name of a live process whose name could not be read
/// ([`LiveProcess::command`]): it may be a login window, so it does not rule a session
/// out.
const UNREADABLE: &str = "?";

/// Whether `processes` show a GUI login session or cannot rule one out: one of them is
/// the login window's, or has a name that could not be read.
fn session_seen(processes: &[LiveProcess]) -> bool {
    processes
        .iter()
        .any(|p| p.command == LOGINWINDOW || p.command == UNREADABLE)
}

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
    /// `kill-uid <lease>`: looks, before anything is killed, whether the uid may have a
    /// GUI login session: the session switch's intent record names the lease (the login
    /// window logs in only the user it names), the uid owns a `loginwindow` process (or
    /// one whose name cannot be read), or an earlier teardown left its GUI domain owed
    /// ([`Self::gui_owed`]). A pool user has none of these, so its `gui/<uid>` is never
    /// asked about. A session seen at a later look of the processes counts from the next
    /// attempt on. Then, for at most
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
    /// sent, a process table that could not be read (not retried: the teardown stops
    /// there, the uid held, and is repeated), the GUI teardown record that could not be
    /// kept, or a crontab or `at` job that could not be removed (after the second
    /// attempts).
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
        let intended = read_intent(&self.settings.session.intent)
            .map_err(TeardownError::Refused)?
            == Some(Intent::Lease(lease));
        self.kill_account(&user_name(lease), uid, intended)
            .map_err(|why| match why {
                TeardownError::Outlived {
                    uid, left, stayed, ..
                } => TeardownError::Outlived {
                    uid,
                    left,
                    stayed,
                    lease: true,
                },
                other => other,
            })
            .inspect_err(|why| kept(lease, uid, why))?;
        tracing::info!(lease = %Dotted(lease), uid, "no process of the lease user is left");
        Ok(())
    }

    /// Kills every process of `uid` and boots out its domains, as [`Self::kill_uid`]
    /// describes, for the account `name`; `intended` is whether the session switch's
    /// intent record names the account.
    pub(super) fn kill_account(
        &self,
        name: &str,
        uid: u32,
        intended: bool,
    ) -> Result<(), TeardownError> {
        // Looked at before the kill, which ends the session's `loginwindow`.
        let mut gui = false;
        if intended || self.gui_owed(uid)? || session_seen(&self.processes(uid)?) {
            self.owe_gui(uid)?;
            gui = true;
        }
        tracing::info!(uid, gui, "tearing down uid {uid}");
        self.kill_rounds(uid, &mut gui)?;
        // Swept only once nothing of the user runs (the sweep's contract).
        let schedules = Self::swept(sweep(&self.settings.schedules, name, uid), name);
        self.kill_rounds(uid, &mut gui)?;
        schedules?;
        if gui {
            self.discharge_gui(uid)?;
        }
        Ok(())
    }

    /// For at most [`KILL_ROUNDS`] attempts: kills every process of `uid`, boots out
    /// its domains (`gui/<uid>` too while `gui`), and looks at the outcome, until no
    /// process is left and none of them prints. A session seen at a look (the processes
    /// left show one, [`session_seen`]) sets `gui`, and is recorded as owed, for the
    /// next attempt and every later teardown until one discharges it.
    fn kill_rounds(&self, uid: u32, gui: &mut bool) -> Result<(), TeardownError> {
        let mut left = Vec::new();
        let mut stayed = Vec::new();
        for _ in 0..KILL_ROUNDS {
            self.host
                .kill_all(uid)
                .map_err(|source| TeardownError::Kill { uid, source })?;
            let domains = domains(uid, *gui);
            let refused = self.boot_out(&domains);
            left = self.processes(uid)?;
            stayed = self.still_printing(&domains, &refused);
            if left.is_empty() && stayed.is_empty() {
                return Ok(());
            }
            if !*gui && session_seen(&left) {
                self.owe_gui(uid)?;
                *gui = true;
            }
            self.host.pause();
        }
        Err(TeardownError::Outlived {
            uid,
            left,
            stayed,
            lease: false,
        })
    }

    /// Where the record that `uid`'s GUI domain is owed a teardown is kept: beside the
    /// session switch's intent record, in the helper's state directory, so it outlives a
    /// failed teardown and a restart of the helper.
    fn gui_record(&self, uid: u32) -> PathBuf {
        self.settings
            .session
            .intent
            .with_file_name(format!("gui-teardown.{uid}"))
    }

    /// Whether `uid`'s GUI domain is owed a teardown: one that saw a GUI session of the
    /// uid did not end in its domain judged gone (its `loginwindow` may be gone, so the
    /// session is not seen again).
    /// The record is opened, never through a link: one there is an error.
    fn gui_owed(&self, uid: u32) -> Result<bool, TeardownError> {
        use rustix::fs::{CWD, Mode, OFlags};
        let path = self.gui_record(uid);
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match rustix::fs::openat(CWD, &path, flags, Mode::empty()) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(why) => Err(TeardownError::GuiRecord {
                uid,
                path,
                source: why.into(),
            }),
        }
    }

    /// Records, durably, that `uid`'s GUI domain is owed a teardown.
    fn owe_gui(&self, uid: u32) -> Result<(), TeardownError> {
        let path = self.gui_record(uid);
        crate::autologin::write_whole(&path, b"").map_err(|source| TeardownError::GuiRecord {
            uid,
            path,
            source,
        })
    }

    /// Removes the record that `uid`'s GUI domain is owed a teardown, once its domain was
    /// judged gone.
    fn discharge_gui(&self, uid: u32) -> Result<(), TeardownError> {
        let path = self.gui_record(uid);
        crate::autologin::remove(&path)
            .map(drop)
            .map_err(|source| TeardownError::GuiRecord { uid, path, source })
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
    /// The GUI domain is asked about only while it is owed a teardown ([`Self::gui_owed`]):
    /// with no process of the uid left there is no login session, and a `kill-uid` that
    /// judged the domain gone discharged it. An owed domain is booted out and judged with
    /// the user domain at both looks, and discharged after the last. Each look waits up
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
    /// folders, a process or a domain of the uid remains, the GUI teardown record could not
    /// be read or removed, or the sweep or the deletion failed (the lease stays live, its uid held, so the delete can be
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
        if self.settled(uid)? {
            self.discharge_gui(uid)?;
        }
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

    /// [`Self::none_left`], the uid's user domain (and its GUI domain, while owed) booted
    /// out, and one more look: refused while a process is left or a domain still prints,
    /// and refused at once (no wait)
    /// when any process of the uid is seen after the bootout. Such a process is a job
    /// launchd started before its domain went; it may have written again what the sweep
    /// removed, so it is not waited out: the delete stops, the uid stays held, and the
    /// repeated delete sweeps everything again. Returns whether the GUI domain was owed
    /// (and is now judged gone).
    fn settled(&self, uid: u32) -> Result<bool, TeardownError> {
        self.none_left(uid)?;
        let gui = self.gui_owed(uid)?;
        let domains = domains(uid, gui);
        let refused = self.boot_out(&domains);
        let stayed = self.still_printing(&domains, &refused);
        if !stayed.is_empty() {
            return Err(TeardownError::DomainsLeft { uid, stayed });
        }
        let started = self.processes(uid)?;
        match started.is_empty() {
            true => Ok(gui),
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
