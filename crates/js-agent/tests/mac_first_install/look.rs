//! What the test reads of the Mac, and what it prints when it fails: every step the launcher ran
//! with its report and log, the launchd jobs, the agent's logs and the daemon's records.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use crate::hostkeeper::{RUN, Ran, service_dir};

/// The agent's log directory, which the package installs.
const LOG_DIR: &str = "/Library/Application Support/js-agent/log";

/// The lines of a log printed from its end.
const TAIL_LINES: usize = 60;

/// Whether `program` with `args` exits 0.
pub fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"))
        .status
        .success()
}

/// What `program` with `args` printed on standard output and standard error.
pub fn output(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The `state` `launchctl print` gives the job `label` of the system domain (`running`,
/// `waiting`, ...); `None` when launchd has no such job.
pub fn launchd_state(label: &str) -> Option<String> {
    let out = Command::new("/bin/launchctl")
        .args(["print", &format!("system/{label}")])
        .output()
        .unwrap_or_else(|e| panic!("run launchctl: {e}"));
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let state = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("state = "))
        .next()
        .map(str::to_owned);
    Some(state.unwrap_or_else(|| format!("no state in: {text}")))
}

/// The last [`TAIL_LINES`] lines of `path`.
fn tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_else(|e| format!("({e})"));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

/// Prints what there is to know of the install when the test fails: dropped while the test
/// panics, it prints, and otherwise it says nothing.
pub struct Diagnose {
    ran: Arc<Mutex<Vec<Ran>>>,
    stores: Vec<PathBuf>,
}

impl Diagnose {
    pub fn new(ran: Arc<Mutex<Vec<Ran>>>, stores: Vec<PathBuf>) -> Diagnose {
        Diagnose { ran, stores }
    }

    fn print(&self) {
        let ran = self.ran.lock().unwrap_or_else(PoisonError::into_inner);
        for step in ran.iter() {
            eprintln!(
                "== step {}: exit {:?}, report {:?}\n{}",
                step.unit, step.code, step.last_line, step.stderr
            );
        }
        for label in RUN {
            eprintln!(
                "== launchctl print system/{label}\n{}",
                output("/bin/launchctl", &["print", &format!("system/{label}")])
            );
        }
        if let Ok(entries) = fs::read_dir(LOG_DIR) {
            for entry in entries.flatten() {
                eprintln!("== {}\n{}", entry.path().display(), tail(&entry.path()));
            }
        }
        let dirs = self.stores.iter().cloned().chain([service_dir()]);
        for dir in dirs {
            eprintln!(
                "== {}\n{}",
                dir.display(),
                output("/bin/ls", &["-la", &dir.to_string_lossy()])
            );
        }
        eprintln!(
            "== the record directory of the hook\n{}",
            output(
                "/bin/ls",
                &["-la", "/Library/Application Support/js-agent/update"]
            )
        );
        eprintln!(
            "== diskutil apfs list\n{}",
            output("/usr/sbin/diskutil", &["apfs", "list"])
        );
    }
}

impl Drop for Diagnose {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.print();
        }
    }
}
