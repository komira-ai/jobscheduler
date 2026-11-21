//! The post-job check of a native agent on a Mac ([`NativeChecks`]): the root helper's
//! leak scan (`scan`, [`js_mac_session::helper`]), judged against what the same scan
//! answered when the host was last idle.
//!
//! The helper's scan reports what changed against the node's recorded baseline: what was
//! added, what was removed, and what it could not read (each list cut to
//! [`js_mac_session::proto::SCAN_LINES`] lines and a last `and <n> more`). The baseline is
//! the node's, so a scan of a host that drifted since it was recorded is dirty before any
//! run; the check therefore keeps the scan of the idle host as its reference (taken again
//! each time the host is idle, as the Linux checks retake theirs) and judges only what is
//! new against it:
//!
//! - an added login item or Background Task Management entry of a uid other than root's
//!   is residue ([`code::SCAN_RESIDUE`]): a run as a pool user can make its own, and the
//!   reset removes neither the entries nor (for a uid the pool keeps) their owner, so the
//!   next run of the uid would inherit them. So is an added `and <n> more` line, which
//!   may hide one;
//! - every other change is a note ([`code::SCAN_HOST_CHANGED`]), never a taint: the
//!   launch agent and daemon folders and the auto-login state are root's, and the lease
//!   users are the pool's own (the helper reports no live lease's user, and the end of a
//!   lease fails when its user is not reset); so is a removal, which no pool user can make
//!   of another user's entries. Lines are compared by their text, so when the idle host
//!   already had more changes than a reply holds, a root-owned change that moves an item
//!   past the cut changes the count of `and <n> more` and taints the host, and a run's
//!   entry behind an unchanged count is missed: the reference holds both cases only while
//!   the node's drift fits in a reply;
//! - a scan that could not read an item, or that could not run, leaves the host unchecked
//!   ([`code::SCAN_UNREADABLE`], [`code::SCAN_FAILED`]). That taint clears itself as the
//!   Linux checks' unreadable inputs do, once two re-checks of the idle host pass
//!   ([`crate::lifecycle::clearable`]); a residue does not;
//! - a node with no recorded baseline is not judged: the scan has nothing to compare
//!   with. It is noted once ([`code::SCAN_NO_BASELINE`]) and every run passes until one is
//!   recorded.
//!
//! A change of another job running beside the one that ended is judged with the one that
//! ended: the host is tainted either way, and the finding names the ended run.

use std::collections::BTreeSet;

use js_mac_session::client::{ClientError, Scanned};
use js_mac_session::helper::NO_BASELINE;
use js_mac_session::scan::{BACKGROUND_TASKS, LOGIN_ITEMS};

use crate::lifecycle::{CheckFinding, CheckHook, CheckNote, Finished};

/// The codes of the native check's findings and notes.
pub mod code {
    /// A run left a login item or a Background Task Management entry.
    pub const SCAN_RESIDUE: &str = "scan-residue";
    /// The leak scan could not read an item.
    pub const SCAN_UNREADABLE: &str = "scan-unreadable";
    /// The leak scan could not be asked.
    pub const SCAN_FAILED: &str = "scan-failed";
    /// The node has no leak-scan baseline: nothing is judged (a note).
    pub const SCAN_NO_BASELINE: &str = "scan-no-baseline";
    /// The host changed where no run can change it (a note).
    pub const SCAN_HOST_CHANGED: &str = "scan-host-changed";
}

/// How the check asks for a scan: the helper's `scan` on a Mac.
pub type Scan = Box<dyn FnMut() -> Result<Scanned, ClientError> + Send>;

/// The post-job check of a native agent: see the module documentation.
pub struct NativeChecks {
    scan: Scan,
    /// The idle host's lines, each `+ <item>` or `- <item>`.
    reference: BTreeSet<String>,
    notes: Vec<CheckNote>,
    /// Whether the missing baseline was noted since a scan last ran.
    told_no_baseline: bool,
}

impl NativeChecks {
    /// The check over `scan`, with an empty reference until the host is first idle.
    #[must_use]
    pub fn new(scan: Scan) -> Self {
        Self {
            scan,
            reference: BTreeSet::new(),
            notes: Vec::new(),
            told_no_baseline: false,
        }
    }

    /// The check over the helper's `scan` at `helper`.
    #[must_use]
    pub fn over(helper: js_mac_session::client::Client) -> Self {
        Self::new(Box::new(move || helper.scan()))
    }

    /// Judges the scan after a run.
    fn judge(&mut self) -> Result<(), CheckFinding> {
        let scanned = match (self.scan)() {
            Ok(scanned) => scanned,
            Err(ClientError::Refused(why)) if why == NO_BASELINE => {
                if !self.told_no_baseline {
                    self.told_no_baseline = true;
                    self.notes.push(CheckNote::new(
                        code::SCAN_NO_BASELINE,
                        "the node has no leak-scan baseline, so the leak scan judges no run \
                         until one is recorded",
                    ));
                }
                return Ok(());
            }
            Err(why) => {
                return Err(CheckFinding::unchecked(
                    code::SCAN_FAILED,
                    &format!("the leak scan could not run: {why}"),
                ));
            }
        };
        self.told_no_baseline = false;
        if !scanned.errors.is_empty() {
            return Err(CheckFinding::unchecked(
                code::SCAN_UNREADABLE,
                &format!(
                    "the leak scan could not read: {}",
                    scanned.errors.join("; ")
                ),
            ));
        }
        let lines = lines_of(&scanned);
        let (left, changed): (Vec<&String>, Vec<&String>) = lines
            .difference(&self.reference)
            .partition(|line| left_by_a_run(line));
        if !changed.is_empty() {
            self.notes.push(CheckNote::new(
                code::SCAN_HOST_CHANGED,
                &format!("the leak scan saw the host change: {}", joined(&changed)),
            ));
        }
        let finding = (!left.is_empty()).then(|| {
            CheckFinding::residue(
                code::SCAN_RESIDUE,
                &format!("the leak scan found what a run left: {}", joined(&left)),
            )
        });
        // What the host changed is known now; what a run left stays new, so a re-check
        // finds it again.
        let left: BTreeSet<String> = left.into_iter().cloned().collect();
        self.reference = lines.difference(&left).cloned().collect();
        finding.map_or(Ok(()), Err)
    }
}

impl CheckHook for NativeChecks {
    fn run(&mut self, _: &Finished<'_>) -> Result<(), CheckFinding> {
        self.judge()
    }

    fn notes(&mut self) -> Vec<CheckNote> {
        std::mem::take(&mut self.notes)
    }

    /// The idle host's scan becomes the reference. A scan that could not run or read an
    /// item, or a node with no baseline, keeps the reference there was.
    fn idle(&mut self) {
        if let Ok(scanned) = (self.scan)()
            && scanned.errors.is_empty()
        {
            self.reference = lines_of(&scanned);
        }
    }
}

/// The lines of `scanned`: `+ <item>` for each added item, `- <item>` for each removed.
fn lines_of(scanned: &Scanned) -> BTreeSet<String> {
    let added = scanned.added.iter().map(|item| format!("+ {item}"));
    let removed = scanned.removed.iter().map(|item| format!("- {item}"));
    added.chain(removed).collect()
}

/// Whether `line` is one a run can have left: an added login item or Background Task
/// Management entry of a uid other than root's (or whose uid cannot be read), or an
/// added `and <n> more`, which may hide one.
fn left_by_a_run(line: &str) -> bool {
    let Some(item) = line.strip_prefix("+ ") else {
        return false;
    };
    if is_more(item) {
        return true;
    }
    let (class, rest) = item.split_once(' ').unwrap_or((item, ""));
    let uid = match class {
        LOGIN_ITEMS => rest
            .strip_prefix("\"loginitems.")
            .and_then(|rest| rest.split_once(".plist\""))
            .map(|(uid, _)| uid),
        BACKGROUND_TASKS => rest
            .strip_prefix("uid=")
            .map(|rest| rest.split_once(' ').map_or(rest, |(uid, _)| uid)),
        _ => return false,
    };
    uid.and_then(|uid| uid.parse::<u32>().ok()) != Some(0)
}

/// Whether `item` is the `and <n> more` that ends a cut list.
fn is_more(item: &str) -> bool {
    item.strip_prefix("and ")
        .and_then(|rest| rest.strip_suffix(" more"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `lines`, joined for a detail.
fn joined(lines: &[&String]) -> String {
    lines
        .iter()
        .map(|line| line.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests;
