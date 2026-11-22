//! `js-agent __account-cycle`: the account life cycle once, for proving it on a Mac.
//!
//! ```text
//! js-agent __account-cycle --state <dir> [-- <program> [args...]]
//! ```
//!
//! A development verb: it makes the pool's one account of the Mac it runs on, runs programs as
//! that account, retires the account and shows what became of it, so that the life cycle of
//! [`super`] can be seen working on a real Mac by the signed agent that holds Full Disk Access.
//! It is removed before any release; no job is run this way.
//!
//! Run as root, it:
//!
//! 1. prints whether the program holds Full Disk Access, then opens the [`super::Pool`] over
//!    `<dir>` (made, root's and mode 0755, when it is missing) with one account kept ready.
//!    Without the grant the pool refuses, and the verb says why and stops;
//! 2. lends the account to the job `account-cycle` and prints it: its name, uid, group,
//!    `GeneratedUID`, and whether it was made on the spot;
//! 3. runs each command as the account (its uid and group, that group the only one it holds,
//!    `HOME` and the working directory its home, no other environment) and prints how it ended
//!    and what it printed. With none given it runs `/usr/bin/getconf DARWIN_USER_DIR`, which
//!    makes the slot's per-user folder, and `/usr/bin/defaults write com.example.cycle written
//!    -bool yes`, which writes into the home;
//! 4. prints the slot's per-user folder and what is in it (at most [`LISTED`] entries);
//! 5. retires the account and prints what was shown gone ([`super::Proof`]), the pids that
//!    were killed and how long the deletion and the refill took;
//! 6. prints the slot's per-user folder again: emptied, and kept.
//!
//! Its statuses ([`status`]): 0 the account was retired and shown gone; 1 it was held (what
//! stopped it is printed), or a command could not be started; 2 the pool refused to open or
//! to lend, or the state directory could not be made; 64 the command line is wrong.
//!
//! The command line and the output are platform-neutral and tested everywhere; what runs a
//! program as the account is `macos::AsAccount`, and only macOS has it: elsewhere the verb
//! parses its command line and refuses.

use std::ffi::OsString;
use std::io::Write;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(any(target_os = "macos", test))]
use std::collections::VecDeque;
#[cfg(any(target_os = "macos", test))]
use std::io;
#[cfg(any(target_os = "macos", test))]
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::sync::Arc;

#[cfg(any(target_os = "macos", test))]
use super::{Account, AccountHost, Pool, Settings, tree};

/// The mode word `main` answers before anything else.
pub const VERB: &str = "__account-cycle";

/// The statuses of the verb.
pub mod status {
    /// The account was retired and shown gone.
    pub const OK: u8 = 0;
    /// The account was held, or a command could not be started.
    pub const HELD: u8 = 1;
    /// The pool refused to open or to lend, or the state directory could not be made.
    pub const REFUSED: u8 = 2;
    /// The command line is wrong.
    pub const USAGE: u8 = 64;
}

/// The most entries of the slot's folder the verb prints.
pub const LISTED: usize = 20;

/// The job id the account is lent to.
#[cfg(any(target_os = "macos", test))]
const JOB: &str = "account-cycle";

/// How the command line reads.
const USAGE: &str = "usage: js-agent __account-cycle --state <dir> [--range <first>-<last>] [-- <program> [args...]]";

/// The command line after the mode word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// The directory the pool keeps its ledger in.
    pub state: PathBuf,
    /// The programs to run as the account, each with its arguments, in order.
    pub commands: Vec<Vec<String>>,
    /// The uids the pool's slots take, when not the pool's own range ([`super::UID_RANGE`]):
    /// a proof on a Mac whose range an earlier release's accounts still fill runs beside them.
    pub range: Option<RangeInclusive<u32>>,
}

/// The programs run when none is given: the one that makes the slot's per-user folder, and
/// one that writes into the home.
fn default_commands() -> Vec<Vec<String>> {
    [
        &["/usr/bin/getconf", "DARWIN_USER_DIR"][..],
        &[
            "/usr/bin/defaults",
            "write",
            "com.example.cycle",
            "written",
            "-bool",
            "yes",
        ][..],
    ]
    .iter()
    .map(|command| command.iter().map(|word| (*word).to_owned()).collect())
    .collect()
}

/// Parses the command line after the mode word: `--state <dir>` once, and optionally the one
/// command to run instead of the defaults, as every word after `--` (or after `--command`).
///
/// # Errors
/// What is wrong with it, in words.
pub fn parse(args: &[OsString]) -> Result<Args, String> {
    let mut state = None;
    let mut range = None;
    let mut command: Option<Vec<String>> = None;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.to_str() {
            Some("--state") => {
                let value = words.next().ok_or("--state needs a directory")?;
                if state.replace(PathBuf::from(value)).is_some() {
                    return Err("--state is given twice".to_owned());
                }
            }
            Some("--range") => {
                let value = words.next().ok_or("--range needs <first>-<last>")?;
                if range.replace(range_of(value)?).is_some() {
                    return Err("--range is given twice".to_owned());
                }
            }
            Some("--" | "--command") => {
                let rest: Vec<String> = words
                    .by_ref()
                    .map(|arg| {
                        arg.to_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("{arg:?} is not UTF-8"))
                    })
                    .collect::<Result<_, _>>()?;
                if rest.is_empty() {
                    return Err(format!("{} needs a program", word.to_string_lossy()));
                }
                command = Some(rest);
            }
            _ => return Err(format!("unknown argument {word:?}")),
        }
    }
    Ok(Args {
        state: state.ok_or("--state is required")?,
        commands: command.map_or_else(default_commands, |command| vec![command]),
        range,
    })
}

/// The range `<first>-<last>` of `value`, both uids, the first not above the last.
fn range_of(value: &OsString) -> Result<RangeInclusive<u32>, String> {
    let bad =
        || format!("--range {value:?}: not <first>-<last>, two uids, the first not above the last");
    let (first, last) = value
        .to_str()
        .and_then(|text| text.split_once('-'))
        .ok_or_else(bad)?;
    match (first.parse::<u32>(), last.parse::<u32>()) {
        (Ok(first), Ok(last)) if first <= last => Ok(first..=last),
        _ => Err(bad()),
    }
}

/// Runs the verb: `args` is the whole command line, the mode word second.
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let args = match parse(args.get(2..).unwrap_or_default()) {
        Ok(args) => args,
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}\n{USAGE}");
            return ExitCode::from(status::USAGE);
        }
    };
    ExitCode::from(run_parsed(&args, out, err))
}

/// Runs the parsed verb on this Mac, as the signed agent: the real host and the real way to
/// run a program as an account.
#[cfg(target_os = "macos")]
fn run_parsed(args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let mut settings = Settings::macos(1);
    if let Some(range) = &args.range {
        settings.range = range.clone();
    }
    drive(
        args,
        settings,
        Arc::new(super::MacHost),
        &super::macos::AsAccount,
        out,
        err,
    )
}

/// Refuses: only a Mac has accounts to make and a Full Disk Access to hold.
#[cfg(not(target_os = "macos"))]
fn run_parsed(_args: &Args, _out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let _ = writeln!(err, "js-agent: {VERB}: macOS only");
    status::REFUSED
}

/// How a program run as the account ended and what it printed.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Ran {
    /// Its exit status; `None` when a signal ended it.
    pub(super) code: Option<i32>,
    /// The signal that ended it, when one did.
    pub(super) signal: Option<i32>,
    /// What it printed.
    pub(super) stdout: String,
    /// What it printed to its error stream.
    pub(super) stderr: String,
}

/// Runs a program as an account. The macOS implementation is `macos::AsAccount`; the tests
/// run the same cycle over a fake.
#[cfg(any(target_os = "macos", test))]
pub(super) trait Runner {
    /// Runs `command` (the program, then its arguments) as `account`: its uid, its group and
    /// no other, `HOME` and the working directory `home`, no other environment, no input.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, account: &Account, home: &Path, command: &[String]) -> io::Result<Ran>;
}

/// The cycle, over `host` and `runner`. Returns the status.
#[cfg(any(target_os = "macos", test))]
pub(super) fn drive(
    args: &Args,
    settings: Settings,
    host: Arc<dyn AccountHost>,
    runner: &dyn Runner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let grant = match host.full_disk_access() {
        Ok(()) => "granted",
        Err(why) if why.kind() == io::ErrorKind::PermissionDenied => "not-granted",
        Err(_) => "undetermined",
    };
    let _ = writeln!(out, "full disk access: {grant}");
    let refused = |err: &mut dyn Write, why: &dyn std::fmt::Display| {
        let _ = writeln!(err, "js-agent: {VERB}: {why}");
        status::REFUSED
    };
    if let Err(why) = make_state(&args.state) {
        let state = args.state.display();
        return refused(err, &format!("making the state directory {state}: {why}"));
    }
    let pool = match Pool::with_settings(&args.state, settings.clone(), host) {
        Ok(pool) => pool,
        Err(why) => return refused(err, &why),
    };
    let lent = match pool.lend(JOB) {
        Ok(lent) => lent,
        Err(why) => return refused(err, &why),
    };
    let account = lent.account;
    let uid = account.uid;
    let home = settings.homes.join(&account.name);
    let _ = writeln!(
        out,
        "account: name={} uid={uid} gid={} generated_uid={} made_now={}",
        account.name, account.gid, account.generated_uid, lent.made_now
    );
    let _ = writeln!(out, "home: {}", home.display());
    let mut started = true;
    for command in &args.commands {
        let _ = writeln!(out, "command: {}", command.join(" "));
        match runner.run(&account, &home, command) {
            Ok(ran) => print_ran(out, &ran),
            Err(why) => {
                started = false;
                let _ = writeln!(out, "  not started: {why}");
            }
        }
    }
    print_folders(out, "before", &settings, uid);
    let retired = pool.retire(account);
    match &retired {
        Ok(done) => print_retired(out, done),
        Err(held) => {
            let _ = writeln!(out, "retire: held: {held}");
        }
    }
    print_folders(out, "after", &settings, uid);
    match retired.is_ok() && started {
        true => status::OK,
        false => status::HELD,
    }
}

/// Makes the state directory (and what is above it) when it is not there: root's and mode
/// 0755, which the pool requires.
#[cfg(any(target_os = "macos", test))]
fn make_state(state: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(state)
}

/// What a command printed and how it ended.
#[cfg(any(target_os = "macos", test))]
fn print_ran(out: &mut dyn Write, ran: &Ran) {
    match (ran.code, ran.signal) {
        (Some(code), _) => {
            let _ = writeln!(out, "  exit: {code}");
        }
        (None, signal) => {
            let _ = writeln!(out, "  signal: {}", signal.unwrap_or(0));
        }
    }
    for (name, text) in [("stdout", &ran.stdout), ("stderr", &ran.stderr)] {
        let text = text.trim_end();
        match text.is_empty() {
            true => {
                let _ = writeln!(out, "  {name}: (nothing)");
            }
            false => {
                let _ = writeln!(out, "  {name}: {}", text.replace('\n', "\n    "));
            }
        }
    }
}

/// What a retirement did: the pids killed, each item of the proof, and the times.
#[cfg(any(target_os = "macos", test))]
fn print_retired(out: &mut dyn Write, done: &super::Retired) {
    let _ = writeln!(out, "retire: ok");
    let _ = writeln!(out, "  killed: {:?}", done.killed);
    let gone = done.gone;
    for (item, held) in [
        ("record", gone.record),
        ("processes", gone.processes),
        ("home", gone.home),
        ("folders", gone.folders),
        ("places", gone.places),
        ("ipc", gone.ipc),
    ] {
        let _ = writeln!(out, "  proof.{item}: {held}");
    }
    let _ = writeln!(out, "  delete_ms: {}", done.delete_ms);
    let _ = writeln!(out, "  refill_ms: {}", done.refill_ms);
}

/// Every per-user folder of `uid`, `when` (`before` or `after` the retirement), each with
/// what is in it.
#[cfg(any(target_os = "macos", test))]
fn print_folders(out: &mut dyn Write, when: &str, settings: &Settings, uid: u32) {
    let folders = match tree::folder_entries(&settings.user_folders, uid) {
        Ok(folders) => folders,
        Err(why) => {
            let _ = writeln!(out, "per-user folders {when}: cannot look: {why}");
            return;
        }
    };
    if folders.is_empty() {
        let _ = writeln!(out, "per-user folders {when}: none of uid {uid}'s");
    }
    for folder in folders {
        let _ = writeln!(out, "per-user folder {when}: {}", folder.display());
        match listing(&folder, LISTED) {
            Ok((entries, _)) if entries.is_empty() => {
                let _ = writeln!(out, "  (empty)");
            }
            Ok((entries, more)) => {
                for entry in entries {
                    let _ = writeln!(out, "  {entry}");
                }
                if more {
                    let _ = writeln!(out, "  ... and more");
                }
            }
            Err(why) => {
                let _ = writeln!(out, "  cannot list: {why}");
            }
        }
    }
}

/// A `find`-like listing of `root`: the paths below it, relative to it, each directory before
/// what is in it and with a `/` after its name, in the order of their names; at most `limit`,
/// and whether there were more. A link is listed and not followed.
///
/// # Errors
/// A directory could not be read or an entry looked at.
#[cfg(any(target_os = "macos", test))]
fn listing(root: &Path, limit: usize) -> io::Result<(Vec<String>, bool)> {
    let names = |dir: &Path| -> io::Result<VecDeque<OsString>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names.into())
    };
    let mut found = Vec::new();
    let mut open = vec![(PathBuf::new(), names(root)?)];
    while let Some((above, pending)) = open.last_mut() {
        let Some(name) = pending.pop_front() else {
            open.pop();
            continue;
        };
        if found.len() == limit {
            return Ok((found, true));
        }
        let path = above.join(&name);
        let is_dir = std::fs::symlink_metadata(root.join(&path))?.is_dir();
        found.push(entry_text(&path, is_dir));
        if is_dir {
            let inner = names(&root.join(&path))?;
            open.push((path, inner));
        }
    }
    Ok((found, false))
}

/// `path` as the listing prints it: a directory with a `/` at its end.
#[cfg(any(target_os = "macos", test))]
fn entry_text(path: &Path, is_dir: bool) -> String {
    let mut text = path.as_os_str().to_string_lossy().into_owned();
    if is_dir {
        text.push('/');
    }
    text
}

#[cfg(test)]
mod tests;
