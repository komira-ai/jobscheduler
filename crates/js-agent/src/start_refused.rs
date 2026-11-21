//! `js-agent crun-start-refused --result <word>`: ends the runs a stop or a crash left in the
//! crun unit's cgroup when systemd refused to start the unit because of them.
//!
//! The unit (`js-agent-crun.service`) runs it as `ExecStopPost=` with systemd's `SERVICE_RESULT`
//! as the word, so it runs after every stop of the unit and does nothing unless the result is
//! `resources`.
//!
//! **Why.** systemd 258 and newer clones the first process of a service, the `ExecStart=`
//! program, into the unit's own cgroup and moves it into `DelegateSubgroup=` only afterwards
//! (255 to 257 cloned it into the subgroup). The kernel refuses a process in a cgroup that has
//! controllers enabled for its children, and systemd cannot switch them off while
//! `<unit>/actions` uses them. The runs a hand-over by stop or an agent crash leave under
//! `KillMode=process` keep `<unit>/actions` populated, so every start fails with `Device or
//! resource busy` and the result `resources` until they end. `ExecStopPost=` runs in
//! `<unit>/.control`, which the kernel accepts, so this verb can end them. Only a start that
//! systemd refused gets here: it trades the runs left, which no agent can report while the unit
//! cannot start, for a node that is back at the next restart (`Restart=always`).
//!
//! **What it does.** With the result `resources`, on systemd 258 or newer (the version
//! `systemctl --version` prints), it finds the unit's cgroup as the parent of the leaf this
//! process runs in, and does nothing when that cgroup's `cgroup.subtree_control` is empty or
//! when no child of `<unit>/actions` is populated. Otherwise it writes `1` to
//! `<unit>/supervisor/cgroup.kill` when that cgroup is populated and waits until its
//! `cgroup.events` says `populated 0`, then does the same for `<unit>/actions`, which holds the
//! lease cgroups. Each wait is a fixed count of one-second pauses. One line says what it found
//! or ended; the exit status is 0 unless a step failed or a cgroup stayed populated.
//!
//! **Why the supervisors first.** A supervisor that sees its container end writes an exit
//! record in the lease directory, which the next agent would report as the job's own exit code.
//! A supervisor ended before its container leaves no record: the next agent's adoption finds the
//! supervisor gone and no record (`the supervisor has ended and left no exit record`), refuses
//! the run as inconsistent, removes what its lease made and reports the job lost, with no exit
//! code (the end `js_contain::oci::supervise` gives a run whose supervisor died without a
//! record).

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use crate::SystemCommands;
use crate::facts::CommandRunner as _;
use crate::systemd::{OWN_CGROUP_START, version};

/// The verb: `js-agent crun-start-refused`.
pub const VERB: &str = "crun-start-refused";

/// The result word of a start that systemd refused for want of resources.
const REFUSED: &str = "resources";

/// The program that prints systemd's version.
const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// How many pauses of one second a cgroup gets to empty after its kill.
const PAUSES: u32 = 30;

/// The line every message starts with.
const PREFIX: &str = "crun-start-refused: ";

/// The verb's flags.
#[derive(Debug, Parser)]
#[command(name = "js-agent crun-start-refused")]
struct Args {
    /// The result of the unit's stop (systemd's `SERVICE_RESULT`).
    #[arg(long)]
    result: String,
}

/// What the verb reads and writes the host through.
pub(crate) struct Host<'a> {
    /// The text of a file.
    pub(crate) read: &'a dyn Fn(&Path) -> io::Result<String>,
    /// Writes a file with the text.
    pub(crate) write: &'a dyn Fn(&Path, &str) -> io::Result<()>,
    /// The names of the directories in a directory, sorted.
    pub(crate) list: &'a dyn Fn(&Path) -> io::Result<Vec<String>>,
    /// What `systemctl --version` prints.
    pub(crate) systemctl: &'a dyn Fn() -> Result<String, String>,
    /// Waits one second.
    pub(crate) pause: &'a dyn Fn(),
    /// The cgroup version 2 mount.
    pub(crate) mount: &'a Path,
    /// The file that names this process's cgroup (`/proc/self/cgroup`).
    pub(crate) self_cgroup: &'a Path,
}

impl Host<'static> {
    /// This machine.
    pub(crate) fn system() -> Self {
        Self {
            read: &read_file,
            write: &write_file,
            list: &list_dirs,
            systemctl: &|| systemctl_version(SYSTEMCTL),
            pause: &pause_one_second,
            mount: Path::new("/sys/fs/cgroup"),
            self_cgroup: Path::new("/proc/self/cgroup"),
        }
    }
}

/// The text of the file `path`.
fn read_file(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
}

/// Writes `text` to the file `path`.
fn write_file(path: &Path, text: &str) -> io::Result<()> {
    std::fs::write(path, text)
}

/// The names of the directories in `path`, sorted.
fn list_dirs(path: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// What `program --version` prints.
fn systemctl_version(program: &str) -> Result<String, String> {
    SystemCommands::default().run(program, &["--version"])
}

/// Waits one second.
fn pause_one_second() {
    std::thread::sleep(std::time::Duration::from_secs(1));
}

/// The `--result` word of the command line `args` (the program name, then the verb), or the
/// usage message of a command line the verb does not accept.
///
/// # Errors
/// The command line has no `--result`, or has a flag the verb does not take.
pub fn result_word(args: &[OsString]) -> Result<String, String> {
    Args::try_parse_from(args.iter().skip(1))
        .map(|args| args.result)
        .map_err(|why| why.to_string())
}

/// Runs the verb when `args` (the program name, then the verb) name it; `None` for any other
/// verb.
pub(crate) fn dispatch(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<ExitCode> {
    let named = args.get(1).is_some_and(|verb| verb.to_str() == Some(VERB));
    named.then(|| run(args, out, err, &Host::system()))
}

/// Runs the verb on `args` (the program name, then the verb).
pub(crate) fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    let result = match result_word(args) {
        Ok(result) => result,
        Err(why) => {
            let _ = write!(err, "{why}");
            return ExitCode::from(2);
        }
    };
    if result != REFUSED {
        return ExitCode::SUCCESS;
    }
    match end_runs(host) {
        Ok(said) => {
            let _ = writeln!(out, "{PREFIX}{said}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            let _ = writeln!(err, "{PREFIX}{why}");
            ExitCode::from(1)
        }
    }
}

/// Ends the runs left, or says why there is nothing to end; an error is what could not be done.
fn end_runs(host: &Host<'_>) -> Result<String, String> {
    let found = match (host.systemctl)() {
        Ok(printed) => {
            version(&printed).ok_or("`systemctl --version` printed no version".to_owned())
        }
        Err(why) => Err(format!("`systemctl --version`: {why}")),
    };
    let found = match found {
        Ok(found) => found,
        Err(why) => {
            return Ok(format!(
                "the systemd version cannot be read ({why}): nothing is ended"
            ));
        }
    };
    if found < OWN_CGROUP_START {
        return Ok(format!(
            "systemd {found} starts the agent in its subgroup, so the runs left are not why the \
             start was refused: nothing is ended"
        ));
    }
    let unit = unit_cgroup(host)?;
    let control = unit.join("cgroup.subtree_control");
    let enabled = (host.read)(&control).map_err(|why| format!("{}: {why}", control.display()))?;
    if enabled.trim().is_empty() {
        return Ok(format!(
            "no controller is enabled in {}: nothing holds the cgroup",
            unit.display()
        ));
    }
    let actions = unit.join("actions");
    let names = match (host.list)(&actions) {
        Ok(names) => names,
        Err(why) if why.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(why) => return Err(format!("{}: {why}", actions.display())),
    };
    let mut leases = Vec::new();
    for name in names {
        if populated(host, &actions.join(&name))? {
            leases.push(name);
        }
    }
    if leases.is_empty() {
        return Ok(format!("no run is left in {}", actions.display()));
    }
    let left = leases.join(", ");
    for cgroup in [unit.join("supervisor"), actions.clone()] {
        end(host, &cgroup).map_err(|why| format!("{why}; the runs left: {left}"))?;
    }
    Ok(format!(
        "systemd {found} refused the start: ended the runs left in {}: {left}; the next start \
         places the agent",
        actions.display()
    ))
}

/// The unit's cgroup: the parent of the leaf (`.control` for an `ExecStopPost=`) this process is
/// in, below the mount.
fn unit_cgroup(host: &Host<'_>) -> Result<PathBuf, String> {
    let own = (host.read)(host.self_cgroup)
        .map_err(|why| format!("{}: {why}", host.self_cgroup.display()))?;
    let leaf = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| {
            format!(
                "{} names no cgroup of version 2",
                host.self_cgroup.display()
            )
        })?;
    let parent = Path::new(leaf)
        .parent()
        .and_then(|parent| parent.strip_prefix("/").ok())
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("this process is in {leaf}, which is no leaf of a unit"))?;
    Ok(host.mount.join(parent))
}

/// Whether the cgroup `path` holds a process: its `cgroup.events` says `populated 1`. A cgroup
/// that is not there holds none.
fn populated(host: &Host<'_>, path: &Path) -> Result<bool, String> {
    let events = path.join("cgroup.events");
    match (host.read)(&events) {
        Ok(text) => Ok(text.lines().any(|line| line.trim() == "populated 1")),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(why) => Err(format!("{}: {why}", events.display())),
    }
}

/// Kills what the cgroup `path` holds, when it holds anything, and waits until it holds nothing.
fn end(host: &Host<'_>, path: &Path) -> Result<(), String> {
    if !populated(host, path)? {
        return Ok(());
    }
    let kill = path.join("cgroup.kill");
    (host.write)(&kill, "1").map_err(|why| format!("{}: {why}", kill.display()))?;
    for _ in 0..PAUSES {
        if !populated(host, path)? {
            return Ok(());
        }
        (host.pause)();
    }
    match populated(host, path)? {
        true => Err(format!(
            "{} is still populated after {PAUSES} s",
            path.display()
        )),
        false => Ok(()),
    }
}

#[cfg(test)]
#[path = "start_refused_tests.rs"]
mod tests;
