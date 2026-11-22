//! Whether a failed farm build failed only because its connection to the farm was cut, so that
//! the same build may run once more. The rule is buildrules' (`farm.onlyCuts`, tools/stage of
//! komira-ai/buildrules#172), read the same way here:
//!
//! - a **cut** is buck2's own `Error:` line (after its `[<time>] ` stamp) that holds
//!   [`CUT_MARKER`], the error buck2's remote-execution client gives when the stream to the farm
//!   breaks (`RE channel error: code: 'Unknown error', message: "h2 protocol error: ..."`,
//!   `BrokenPipe`);
//! - a failed action's report is a cut when its reason, the line after `Action failed:`, is
//!   `Internal error (stage: ...)` naming a `Remote Execution Error on ...`, and the line after
//!   that is a cut. Any other reason (a compiler or a test that `returned non-zero exit code`, a
//!   timeout, an internal error with no cut under it) is a real failure;
//! - any other sign of a failure anywhere in the output makes the build's failure real: a line
//!   starting `Error` that is not a cut (an analysis or load error, a download that failed,
//!   `Failed to make BatchReadBlobs request: code: 'The operation was cancelled'` standing alone),
//!   a compiler's `error:` or `error[`, or a line that says `non-zero exit code`, `timed out` or
//!   `panic`.
//!
//! So a build whose output mixes one real failure with any number of cuts is final, and a retry
//! never passes over a real failure. A test that only prints the cut's words is not a cut: its
//! output is not buck2's `Error:` line under an action's remote-execution error.

/// What buck2 writes in the error of a cut stream to the farm.
pub(crate) const CUT_MARKER: &str = "RE channel error: code:";

/// One line of buck2's output with its stamp (`[2026-11-21T07:48:10.059+00:00] `), its indent
/// and its line end taken off.
fn bare(raw: &str) -> &str {
    let line = raw.trim_matches([' ', '\t', '\r']);
    match line.strip_prefix('[').and_then(|_| line.find("] ")) {
        Some(end) => line[end + 2..].trim_start_matches([' ', '\t']),
        None => line,
    }
}

/// Whether `line` (bare) is buck2's own error of a cut stream.
fn is_cut(line: &str) -> bool {
    line.starts_with("Error:") && line.contains(CUT_MARKER)
}

/// Whether `line` (bare) shows a failure that is not a cut.
fn is_other_failure(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.starts_with("Error")
        || line.starts_with("error:")
        || line.starts_with("error[")
        || lower.contains("non-zero exit code")
        || lower.contains("timed out")
        || lower.contains("panic")
}

/// Where the reading of an action's report is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Report {
    /// Outside a report, or past its cut line.
    None,
    /// The `Action failed:` line was read; its reason comes next.
    Reason,
    /// The reason was a remote-execution error; the cut line comes next.
    CutNext,
}

/// How many cuts the output `text` of a failed build reports, when every failure in it is a cut;
/// `None` when any failure is not one, or when there is no cut at all.
pub(crate) fn only_cuts(text: &str) -> Option<usize> {
    let mut cuts = 0;
    let mut report = Report::None;
    for line in text.lines().map(bare) {
        if line.starts_with("Action failed:") {
            // A report whose reason or cut line never came is not a cut.
            if report != Report::None {
                return None;
            }
            report = Report::Reason;
            continue;
        }
        if report == Report::Reason {
            if !(line.starts_with("Internal error (stage:")
                && line.contains("Remote Execution Error on "))
            {
                return None;
            }
            report = Report::CutNext;
            continue;
        }
        if report == Report::CutNext && !is_cut(line) {
            return None;
        }
        report = Report::None;
        if is_cut(line) {
            cuts += 1;
        } else if is_other_failure(line) {
            return None;
        }
    }
    (report == Report::None && cuts > 0).then_some(cuts)
}

#[cfg(test)]
mod tests;
