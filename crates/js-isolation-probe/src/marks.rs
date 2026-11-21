//! The markers a job may leave behind, one [`Kind`] each: how `leave` makes it, how `find`
//! looks for it from the next job, and how it is removed again.
//!
//! Every marker carries the run's token, so `find` looks for exactly what `leave` made and
//! removes nothing else. (A System V object is found by a key hashed from the token: an
//! object of another program with the same key, which is unlikely, would be taken for it.) The probe phase makes the privileged kinds (a launchd job, a
//! crontab, an at job, an attribute on the account's own record) and removes them at once.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::host::Who;
use crate::ipc::{self, SysV};
use crate::outcome::{Line, Outcome};
use crate::tools::{Ran, Tools};

const DEFAULTS: &str = "/usr/bin/defaults";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
const PBCOPY: &str = "/usr/bin/pbcopy";
const PBPASTE: &str = "/usr/bin/pbpaste";
const SECURITY: &str = "/usr/bin/security";
const PS: &str = "/bin/ps";
const CRONTAB: &str = "/usr/bin/crontab";
const AT: &str = "/usr/bin/at";
const ATQ: &str = "/usr/bin/atq";
const ATRM: &str = "/usr/bin/atrm";
const LAUNCHCTL: &str = "/bin/launchctl";
const DSCL: &str = "/usr/bin/dscl";

/// The preferences domain, keychain service and launchd label prefix of every marker.
pub const DOMAIN: &str = "com.komira.js-isolation-probe";
/// The attribute written on the account's own record.
pub const ATTRIBUTE: &str = "dsAttrTypeNative:js-isolation-probe";
/// The time the at job is queued for: 2100-01-01 00:00, long after any run ends.
const AT_TIME: &str = "210001010000";

/// A kind of marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A POSIX named semaphore.
    NamedSemaphore,
    /// A System V semaphore set.
    SysvSemaphore,
    /// A System V shared memory segment.
    SysvSharedMemory,
    /// A key in the user's preferences.
    Preferences,
    /// An application registered with Launch Services.
    LaunchServices,
    /// The text on the pasteboard.
    Pasteboard,
    /// A generic password in the user's default keychain.
    Keychain,
    /// A process that started its own session and outlives the job.
    BackgroundProcess,
    /// The user's crontab.
    Crontab,
    /// A job queued with `at`.
    AtJob,
    /// A launchd job in the `user/<uid>` domain.
    LaunchdUser,
    /// A launchd job in the `gui/<uid>` domain.
    LaunchdGui,
    /// An attribute on the user's own directory record.
    AccountAttribute,
}

impl Kind {
    /// Every kind, in the order the phases print them.
    pub const ALL: [Self; 13] = [
        Self::NamedSemaphore,
        Self::SysvSemaphore,
        Self::SysvSharedMemory,
        Self::Preferences,
        Self::LaunchServices,
        Self::Pasteboard,
        Self::Keychain,
        Self::BackgroundProcess,
        Self::Crontab,
        Self::AtJob,
        Self::LaunchdUser,
        Self::LaunchdGui,
        Self::AccountAttribute,
    ];

    /// The kinds the probe phase tries and removes at once, as privilege probes.
    pub const PRIVILEGED: [Self; 5] = [
        Self::LaunchdUser,
        Self::LaunchdGui,
        Self::Crontab,
        Self::AtJob,
        Self::AccountAttribute,
    ];

    /// The name after `leave:`, `find:` or `priv:`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NamedSemaphore => "named-semaphore",
            Self::SysvSemaphore => SysV::Semaphore.name(),
            Self::SysvSharedMemory => SysV::SharedMemory.name(),
            Self::Preferences => "preferences",
            Self::LaunchServices => "launch-services",
            Self::Pasteboard => "pasteboard",
            Self::Keychain => "keychain",
            Self::BackgroundProcess => "background-process",
            Self::Crontab => "crontab",
            Self::AtJob => "at-job",
            Self::LaunchdUser => "launchd-user",
            Self::LaunchdGui => "launchd-gui",
            Self::AccountAttribute => "account-attribute",
        }
    }
}

/// What a probe needs of the host.
pub struct Ctx<'a> {
    /// The system tools.
    pub tools: &'a dyn Tools,
    /// Who the job runs as.
    pub who: &'a Who,
    /// How long a background process lingers.
    pub linger_secs: u64,
}

/// Whether a marker is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Look {
    /// It is; with what removing it needs (an id, a pid) and what to print.
    Found {
        /// The id or pid removing it needs.
        handle: Option<String>,
        /// What to print.
        detail: String,
    },
    /// It is not.
    Gone,
    /// The probe could not tell.
    CannotLook(String),
}

impl Look {
    fn found(detail: impl Into<String>) -> Self {
        Self::Found {
            handle: None,
            detail: detail.into(),
        }
    }
}

/// The launchd label of the run's job.
#[must_use]
pub fn label(tag: &str) -> String {
    format!("{DOMAIN}.{tag}")
}

fn domain(kind: Kind, uid: u32) -> String {
    if kind == Kind::LaunchdGui {
        format!("gui/{uid}")
    } else {
        format!("user/{uid}")
    }
}

fn record(user: &str) -> String {
    format!("/Users/{user}")
}

fn app_path(who: &Who, tag: &str) -> PathBuf {
    who.cwd.join(format!("js-isolation-probe-{tag}.app"))
}

/// The plist of the launchd job of `kind`, in the run directory.
fn plist_path(who: &Who, kind: Kind, tag: &str) -> PathBuf {
    who.cwd
        .join(format!("{}-{}.plist", label(tag), kind.name()))
}

fn bundle_id(tag: &str) -> String {
    format!("{DOMAIN}.{tag}")
}

/// The text the pasteboard marker holds.
fn paste_text(tag: &str) -> String {
    format!("js-isolation-probe {tag}")
}

fn plist(label: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{label}</string>\n\
         <key>ProgramArguments</key><array><string>/usr/bin/true</string></array>\n\
         <key>RunAtLoad</key><false/>\n\
         </dict></plist>\n"
    )
}

fn info_plist(id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <plist version=\"1.0\"><dict>\n\
         <key>CFBundleIdentifier</key><string>{id}</string>\n\
         <key>CFBundleName</key><string>js-isolation-probe</string>\n\
         <key>CFBundlePackageType</key><string>APPL</string>\n\
         </dict></plist>\n"
    )
}

/// Writes `text` to `path`, making its parent.
fn put(path: &Path, text: &str) -> io::Result<()> {
    path.parent().map_or(Ok(()), fs::create_dir_all)?;
    fs::write(path, text)
}

/// The job number `at` prints on standard error: `job 12 at ...`.
fn at_job_number(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    text.split_whitespace()
        .skip_while(|w| *w != "job")
        .nth(1)
        .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_owned)
}

/// A line of `ps -axo pid=,pgid=,uid=,command=` that is the run's `linger`: `(pid, pgid,
/// uid)`.
fn linger_row(row: &str, tag: &str) -> Option<(i32, i32, u32)> {
    let mut words = row.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let pgid = words.next()?.parse().ok()?;
    let uid = words.next()?.parse().ok()?;
    let rest: Vec<&str> = words.collect();
    let lingers = rest.get(1) == Some(&"linger");
    let tagged = rest.windows(2).any(|w| w[0] == "--token" && w[1] == tag);
    (lingers && tagged).then_some((pid, pgid, uid))
}

/// Makes the marker of `kind`. A made marker is [`Outcome::Allowed`], with the handle its
/// removal needs; a refusal is [`Outcome::Denied`] (or an error), with why.
pub fn make(ctx: &Ctx<'_>, kind: Kind, prefix: &str, tag: &str) -> (Line, Option<String>) {
    let name = format!("{prefix}:{}", kind.name());
    let tools = ctx.tools;
    let who = ctx.who;
    match kind {
        Kind::NamedSemaphore => {
            let sem = ipc::semaphore_name(tag);
            match ipc::semaphore_create(&sem) {
                Ok(()) => (Line::new(name, Outcome::Allowed).detail(sem), None),
                Err(err) => (Line::refused(name, &err, sem), None),
            }
        }
        Kind::SysvSemaphore | Kind::SysvSharedMemory => {
            let sysv = sysv_of(kind);
            match sysv.create(ipc::sysv_key(tag)) {
                Ok(id) => (
                    Line::new(name, Outcome::Allowed).detail(format!("id {id}")),
                    Some(id.to_string()),
                ),
                Err(err) => (Line::refused(name, &err, ""), None),
            }
        }
        Kind::Preferences => (
            tools
                .run(DEFAULTS, &["write", DOMAIN, "marker", tag], b"")
                .line(&name),
            None,
        ),
        Kind::LaunchServices => {
            let app = app_path(who, tag);
            let info = app.join("Contents/Info.plist");
            if let Err(err) = put(&info, &info_plist(&bundle_id(tag))) {
                return (Line::refused(name, &err, "writing the application"), None);
            }
            let shown = app.display().to_string();
            (
                tools.run(LSREGISTER, &["-f", &shown], b"").line(&name),
                None,
            )
        }
        Kind::Pasteboard => (
            tools
                .run(PBCOPY, &[], paste_text(tag).as_bytes())
                .line(&name),
            None,
        ),
        Kind::Keychain => (
            tools
                .run(
                    SECURITY,
                    &["add-generic-password", "-s", DOMAIN, "-a", tag, "-w", tag],
                    b"",
                )
                .line(&name),
            None,
        ),
        Kind::BackgroundProcess => match tools.linger(tag, ctx.linger_secs) {
            Ok(pid) => (
                Line::new(name, Outcome::Allowed).detail(format!("pid {pid}")),
                Some(pid.to_string()),
            ),
            Err(err) => (Line::refused(name, &err, "starting it"), None),
        },
        Kind::Crontab => make_crontab(ctx, &name, tag),
        Kind::AtJob => {
            let ran = tools.run(AT, &["-t", AT_TIME], at_text(tag).as_bytes());
            let number = at_job_number(&ran.stderr);
            (ran.line(&name), number)
        }
        Kind::LaunchdUser | Kind::LaunchdGui => {
            let path = plist_path(who, kind, tag);
            if let Err(err) = put(&path, &plist(&label(tag))) {
                return (Line::refused(name, &err, "writing the plist"), None);
            }
            let shown = path.display().to_string();
            let target = domain(kind, who.uid);
            (
                tools
                    .run(LAUNCHCTL, &["bootstrap", &target, &shown], b"")
                    .line(&name),
                None,
            )
        }
        Kind::AccountAttribute => (
            tools
                .run(
                    DSCL,
                    &[".", "-create", &record(&who.user), ATTRIBUTE, tag],
                    b"",
                )
                .line(&name),
            None,
        ),
    }
}

fn at_text(tag: &str) -> String {
    format!("/usr/bin/true # js-isolation-probe {tag}\n")
}

fn sysv_of(kind: Kind) -> SysV {
    if kind == Kind::SysvSemaphore {
        SysV::Semaphore
    } else {
        SysV::SharedMemory
    }
}

fn make_crontab(ctx: &Ctx<'_>, name: &str, tag: &str) -> (Line, Option<String>) {
    // Only an account with no crontab, or an empty one, gets the probe's: one that cannot be
    // read might hold someone's, which installing and removing the probe's would replace.
    let existing = ctx.tools.run(CRONTAB, &["-l"], b"");
    if tool_look(&existing, !existing.out().is_empty(), &["no crontab"]) != Look::Gone {
        return (
            Line::new(name, Outcome::NotApplicable).detail(format!(
                "the account has a crontab, or it cannot be read ({}); it is left as it is",
                if existing.ok() {
                    "it has one".to_owned()
                } else {
                    existing.why()
                }
            )),
            None,
        );
    }
    let file = ctx.who.cwd.join(format!("{}.crontab", label(tag)));
    if let Err(err) = put(&file, &format!("# js-isolation-probe {tag}\n")) {
        return (Line::refused(name, &err, "writing the crontab file"), None);
    }
    let shown = file.display().to_string();
    let line = ctx.tools.run(CRONTAB, &[&shown], b"").line(name);
    let _ = fs::remove_file(&file);
    (line, None)
}

/// Looks for the marker of `kind` that the account `leaver` left.
pub fn look(ctx: &Ctx<'_>, kind: Kind, tag: &str, leaver: &Who) -> Look {
    let tools = ctx.tools;
    match kind {
        Kind::NamedSemaphore => match ipc::semaphore_open(&ipc::semaphore_name(tag)) {
            Ok(()) => Look::found("opened it"),
            Err(err) => ipc_look(&err, "it exists and is another user's"),
        },
        Kind::SysvSemaphore | Kind::SysvSharedMemory => {
            match sysv_of(kind).lookup(ipc::sysv_key(tag)) {
                Ok(id) => Look::Found {
                    handle: Some(id.to_string()),
                    detail: format!("id {id}"),
                },
                Err(err) => ipc_look(&err, "it exists and is another user's"),
            }
        }
        Kind::Preferences => {
            let ran = tools.run(DEFAULTS, &["read", DOMAIN, "marker"], b"");
            tool_look(&ran, ran.ok() && ran.out() == tag, &["does not exist"])
        }
        Kind::LaunchServices => {
            let ran = tools.run(LSREGISTER, &["-dump"], b"");
            if !ran.ok() {
                return Look::CannotLook(ran.why());
            }
            if String::from_utf8_lossy(&ran.stdout).contains(&bundle_id(tag)) {
                Look::found("in the database")
            } else {
                Look::Gone
            }
        }
        Kind::Pasteboard => {
            let ran = tools.run(PBPASTE, &[], b"");
            if !ran.ok() {
                return Look::CannotLook(ran.why());
            }
            if String::from_utf8_lossy(&ran.stdout) == paste_text(tag) {
                Look::found("on the pasteboard")
            } else {
                Look::Gone
            }
        }
        Kind::Keychain => {
            let ran = tools.run(
                SECURITY,
                &["find-generic-password", "-s", DOMAIN, "-a", tag],
                b"",
            );
            tool_look(&ran, ran.ok(), &["could not be found"])
        }
        Kind::BackgroundProcess => look_process(tools, tag),
        Kind::Crontab => {
            let ran = tools.run(CRONTAB, &["-l"], b"");
            let held = ran.ok() && ran.out().contains(&format!("js-isolation-probe {tag}"));
            tool_look(&ran, held, &["no crontab"])
        }
        Kind::AtJob => look_at(tools, tag),
        Kind::LaunchdUser | Kind::LaunchdGui => {
            let service = format!("{}/{}", domain(kind, leaver.uid), label(tag));
            let ran = tools.run(LAUNCHCTL, &["print", &service], b"");
            tool_look(&ran, ran.ok(), &["Could not find", "does not support"])
        }
        Kind::AccountAttribute => {
            let ran = tools.run(DSCL, &[".", "-read", &record(&leaver.user), ATTRIBUTE], b"");
            let held = ran.ok() && ran.out().contains(tag);
            tool_look(&ran, held, &["No such key"])
        }
    }
}

/// An IPC lookup's error: `ENOENT` is gone, `EACCES` is found (another user's), anything
/// else cannot tell.
fn ipc_look(err: &io::Error, other: &str) -> Look {
    match err.raw_os_error() {
        Some(libc::ENOENT) => Look::Gone,
        Some(libc::EACCES) => Look::found(other),
        _ => Look::CannotLook(err.to_string()),
    }
}

/// A tool's look: `held` is found; a success that does not hold it, or a failure whose
/// output says one of `absent`, is gone; any other failure cannot tell.
fn tool_look(ran: &Ran, held: bool, absent: &[&str]) -> Look {
    if held {
        return Look::found("");
    }
    if ran.ok() {
        return Look::Gone;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    if absent.iter().any(|a| text.contains(a)) {
        Look::Gone
    } else {
        Look::CannotLook(ran.why())
    }
}

fn look_process(tools: &dyn Tools, tag: &str) -> Look {
    let ran = tools.run(PS, &["-axo", "pid=,pgid=,uid=,command="], b"");
    if !ran.ok() {
        return Look::CannotLook(ran.why());
    }
    let text = String::from_utf8_lossy(&ran.stdout);
    match text.lines().find_map(|row| linger_row(row, tag)) {
        Some((pid, pgid, uid)) => Look::Found {
            handle: Some(pid.to_string()),
            detail: format!(
                "pid {pid} uid {uid}{}",
                if pgid == pid { ", its own group" } else { "" }
            ),
        },
        None => Look::Gone,
    }
}

fn look_at(tools: &dyn Tools, tag: &str) -> Look {
    let queue = tools.run(ATQ, &[], b"");
    if !queue.ok() {
        return Look::CannotLook(queue.why());
    }
    let wanted = format!("js-isolation-probe {tag}");
    for number in queue
        .out()
        .lines()
        .filter_map(|l| l.split_whitespace().next())
    {
        let job = tools.run(AT, &["-c", number], b"");
        if job.ok() && String::from_utf8_lossy(&job.stdout).contains(&wanted) {
            return Look::Found {
                handle: Some(number.to_owned()),
                detail: format!("job {number}"),
            };
        }
    }
    Look::Gone
}

/// Removes the marker of `kind` the account `owner` made, with the handle `make` or `look`
/// gave.
///
/// # Errors
/// Why it could not be removed.
pub fn remove(
    ctx: &Ctx<'_>,
    kind: Kind,
    tag: &str,
    owner: &Who,
    handle: Option<&str>,
) -> Result<(), String> {
    let tools = ctx.tools;
    let ran = match kind {
        Kind::NamedSemaphore => {
            return ipc::semaphore_remove(&ipc::semaphore_name(tag)).map_err(|e| e.to_string());
        }
        Kind::SysvSemaphore | Kind::SysvSharedMemory => {
            let id = handle
                .and_then(|h| h.parse().ok())
                .ok_or_else(|| "no id".to_owned())?;
            return sysv_of(kind).remove(id).map_err(|e| e.to_string());
        }
        Kind::BackgroundProcess => {
            let pid = handle
                .and_then(|h| h.parse().ok())
                .ok_or_else(|| "no pid".to_owned())?;
            return tools.kill(pid).map_err(|e| e.to_string());
        }
        Kind::Preferences => tools.run(DEFAULTS, &["delete", DOMAIN], b""),
        Kind::LaunchServices => {
            let app = app_path(owner, tag).display().to_string();
            tools.run(LSREGISTER, &["-u", &app], b"")
        }
        Kind::Pasteboard => tools.run(PBCOPY, &[], b""),
        Kind::Keychain => tools.run(
            SECURITY,
            &["delete-generic-password", "-s", DOMAIN, "-a", tag],
            b"",
        ),
        Kind::Crontab => tools.run(CRONTAB, &["-r"], b""),
        Kind::AtJob => {
            let number = handle.ok_or_else(|| "no job number".to_owned())?;
            tools.run(ATRM, &[number], b"")
        }
        Kind::LaunchdUser | Kind::LaunchdGui => {
            let service = format!("{}/{}", domain(kind, owner.uid), label(tag));
            tools.run(LAUNCHCTL, &["bootout", &service], b"")
        }
        Kind::AccountAttribute => tools.run(
            DSCL,
            &[".", "-delete", &record(&owner.user), ATTRIBUTE],
            b"",
        ),
    };
    if ran.ok() { Ok(()) } else { Err(ran.why()) }
}

/// `priv:<kind>`: makes the marker and, when that was allowed, removes it at once, and
/// removes the plist a launchd job was given either way.
#[must_use]
pub fn try_and_undo(ctx: &Ctx<'_>, kind: Kind, tag: &str) -> Line {
    let (mut line, handle) = make(ctx, kind, "priv", tag);
    // Absent for the other kinds, and for a plist that could not be written.
    let _ = fs::remove_file(plist_path(ctx.who, kind, tag));
    if line.outcome == Outcome::Allowed {
        let undone = match remove(ctx, kind, tag, ctx.who, handle.as_deref()) {
            Ok(()) => "undone".to_owned(),
            Err(why) => format!("could not undo: {why}"),
        };
        line.detail = join(&line.detail, &undone);
    }
    line
}

/// `leave:<kind>`: makes the marker and keeps it. A tool that said yes but whose marker
/// cannot be found right after is a denial: nothing was left.
#[must_use]
pub fn leave(ctx: &Ctx<'_>, kind: Kind, tag: &str) -> Line {
    let (mut line, _) = make(ctx, kind, "leave", tag);
    if line.outcome == Outcome::Allowed {
        match look(ctx, kind, tag, ctx.who) {
            Look::Found { .. } => {}
            Look::Gone => {
                line.outcome = Outcome::Denied;
                line.detail = join(&line.detail, "accepted, but not there right after");
            }
            Look::CannotLook(why) => {
                line.detail = join(&line.detail, &format!("cannot look back: {why}"));
            }
        }
    }
    line
}

/// `find:<kind>`: whether the marker `leaver` left is there; on the leaver's own slot
/// (`same_slot`) a found marker is removed.
#[must_use]
pub fn find(ctx: &Ctx<'_>, kind: Kind, tag: &str, leaver: &Who, same_slot: bool) -> Line {
    let name = format!("find:{}", kind.name());
    match look(ctx, kind, tag, leaver) {
        Look::Gone => Line::new(name, Outcome::Gone),
        Look::CannotLook(why) => {
            Line::new(name, Outcome::NotApplicable).detail(format!("cannot look: {why}"))
        }
        Look::Found { handle, detail } => {
            let after = if same_slot {
                match remove(ctx, kind, tag, leaver, handle.as_deref()) {
                    Ok(()) => "removed".to_owned(),
                    Err(why) => format!("could not remove: {why}"),
                }
            } else {
                "left for its own slot".to_owned()
            };
            Line::new(name, Outcome::Found).detail(join(&detail, &after))
        }
    }
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_owned()
    } else {
        format!("{a}, {b}")
    }
}

#[cfg(test)]
mod tests;
