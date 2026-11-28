//! The leak scan's verb (S4.2; fleet-updates.md 10.2, layer L3), over the items of
//! [`crate::scan`]:
//!
//! - `scan` collects the items and diffs them against the recorded baseline. It is
//!   dirty when anything was added or removed, or an item could not be read (a scan
//!   that could not look is no proof of a clean node). Refused when no baseline was
//!   recorded ([`NO_BASELINE`]): no verb of the helper records one. The pool's free users
//!   are not items: they are the clean node's; nor are the users of the live leases, so a
//!   scan while leases run reports what they cannot account for, not that they run.

use std::collections::BTreeSet;

use super::{Helper, NO_BASELINE};
use crate::ledger::Ledger;
use crate::ledger::pool::SlotState;
use crate::proto::{Reply, SCAN_LINE_BYTES, SCAN_LINES};
use crate::scan::{self, Diff, read_baseline};

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub diff: Diff,
    pub errors: Vec<String>,
}

impl ScanReport {
    /// Nothing added, nothing removed, nothing unreadable.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.diff.added.is_empty() && self.diff.removed.is_empty() && self.errors.is_empty()
    }
}

/// The reply to a scan, each list cut to what a frame holds.
pub(super) fn scanned(report: ScanReport) -> Reply {
    Reply::Scanned {
        clean: report.clean(),
        added: capped(report.diff.added),
        removed: capped(report.diff.removed),
        errors: capped(report.errors),
    }
}

/// At most [`SCAN_LINES`] lines of at most [`SCAN_LINE_BYTES`], then how many more.
fn capped(mut lines: Vec<String>) -> Vec<String> {
    let more = lines.len().saturating_sub(SCAN_LINES);
    lines.truncate(SCAN_LINES);
    for line in &mut lines {
        line.truncate(line.floor_char_boundary(SCAN_LINE_BYTES));
    }
    if more > 0 {
        lines.push(format!("and {more} more"));
    }
    lines
}

/// The uids of the pool's free users and of the live leases, which the scan does not
/// report.
fn own_uids(ledger: &Ledger) -> BTreeSet<u32> {
    let mut own = ledger.held_uids();
    own.extend(
        ledger
            .slots()
            .into_iter()
            .filter(|(_, entry)| entry.state == SlotState::Free)
            .map(|(_, entry)| entry.uid),
    );
    own
}

impl Helper {
    /// `scan`: see the module documentation.
    ///
    /// # Errors
    /// No baseline was recorded, or it cannot be read.
    pub fn scan(&self) -> Result<ScanReport, String> {
        let ledger = self.lock();
        let baseline = read_baseline(&self.settings.scan.baseline)?.ok_or(NO_BASELINE)?;
        let snapshot = scan::collect(self.host.as_ref(), &self.settings, &own_uids(&ledger));
        let report = ScanReport {
            diff: scan::diff(&baseline, &snapshot.items),
            errors: snapshot.errors,
        };
        tracing::info!(
            clean = report.clean(),
            added = ?report.diff.added,
            removed = ?report.diff.removed,
            errors = ?report.errors,
            "leak scan"
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
