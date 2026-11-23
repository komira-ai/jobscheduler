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
//! - a line of buck2's own log (its `tracing` lines: a time stamp ending `Z`, then the level, often
//!   in colour) at `WARN` or below is buck2 saying it retries a call, not a failure, and is passed
//!   over, whatever its words (`Transient error (attempt 1/5), retrying ...: keep-alive timed out`
//!   comes with every cut); one at `ERROR` is a failure;
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

/// The level of `line` (bare) when it is a line of buck2's own log: its colour codes taken off, a
/// time stamp ending `Z`, then one of the `tracing` levels.
fn log_level(line: &str) -> Option<&'static str> {
    let mut plain = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('\u{1b}') {
        plain.push_str(&rest[..at]);
        let code = &rest[at..];
        // A colour code is ESC `[`, digits and `;`, then `m`; anything else is kept as it is.
        match code
            .strip_prefix("\u{1b}[")
            .and_then(|c| c.find('m').map(|m| (c, m)))
        {
            Some((c, m)) if c[..m].bytes().all(|b| b.is_ascii_digit() || b == b';') => {
                rest = &c[m + 1..];
            }
            _ => {
                plain.push('\u{1b}');
                rest = &code[1..];
            }
        }
    }
    plain.push_str(rest);
    let mut words = plain.split_whitespace();
    let stamp = words.next()?;
    if !(stamp.ends_with('Z')
        && stamp.contains('T')
        && stamp.starts_with(|c: char| c.is_ascii_digit()))
    {
        return None;
    }
    let level = words.next()?;
    ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]
        .into_iter()
        .find(|l| *l == level)
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
        match log_level(line) {
            Some("ERROR") => return None,
            // buck2's own warning that it retries a call: not a failure, and not part of a report.
            Some(_) => continue,
            None => {}
        }
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
