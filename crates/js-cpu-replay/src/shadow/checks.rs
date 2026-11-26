//! The last two of the four checks a class passes to book its curve, read from its check
//! lines (`docs/design/placement-profiles-live.md` section 4): the overruns of its last scored
//! lines and the usable series among its last lines. Checks 1 and 2 are the profile's own
//! shape rule, which the server logs as `cpu_shaped`.
//!
//! The windows are the ones the job service keeps (`Checks::of` in `js-sizing`): the last
//! [`CHECK_RUNS`] lines of the class, in log order, and for the overruns the last
//! [`CHECK_RUNS`] of its scored ones.

use js_sizing::{CHECK_RUNS, MAX_OVERRUNS, MIN_USABLE_SERIES, OVERRUN_PERMILLE};

use super::{Dim, share};

/// What a class's last check lines say of checks 3 and 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    /// The last [`CHECK_RUNS`] scored lines (both errors present).
    scored: usize,
    /// Of them, the lines whose peak was more than [`OVERRUN_PERMILLE`] per mille of the
    /// predicted peak; a scored line with no predicted peak did not overrun.
    overran: usize,
    /// The last [`CHECK_RUNS`] lines, scored or not.
    lines: usize,
    /// Of them, the lines whose CPU status is `learned` or `not-learned`.
    usable: usize,
}

/// Whether `dim`'s line was scored: it gives both errors.
fn is_scored(dim: &Dim) -> bool {
    dim.curve_error.is_some() && dim.flat_error.is_some()
}

/// Whether `dim`'s peak is more than [`OVERRUN_PERMILLE`] per mille of its predicted peak.
fn overran(dim: &Dim) -> bool {
    matches!(
        (dim.peak, dim.predicted_peak),
        (Some(peak), Some(predicted))
            if u128::from(peak) * 1000 > u128::from(predicted) * u128::from(OVERRUN_PERMILLE)
    )
}

/// Whether `dim`'s series was usable: its CPU status is `learned` or `not-learned`.
fn is_usable(dim: &Dim) -> bool {
    matches!(dim.status.as_str(), "learned" | "not-learned")
}

impl Window {
    /// The windows of one class's CPU checks, `dims` in log order.
    pub(super) fn of(dims: &[&Dim]) -> Self {
        let scored: Vec<&&Dim> = dims
            .iter()
            .rev()
            .filter(|dim| is_scored(dim))
            .take(CHECK_RUNS)
            .collect();
        let last: Vec<&&Dim> = dims.iter().rev().take(CHECK_RUNS).collect();
        Self {
            scored: scored.len(),
            overran: scored.iter().filter(|dim| overran(dim)).count(),
            lines: last.len(),
            usable: last.iter().filter(|dim| is_usable(dim)).count(),
        }
    }

    /// The share of the scored lines that overran, to a tenth of a percent; `-` with none.
    pub(super) fn over_share(self) -> String {
        share(self.overran as u64, self.scored as u64)
    }

    /// The share of the lines whose series was usable, to a tenth of a percent; `-` with none.
    pub(super) fn usable_share(self) -> String {
        share(self.usable as u64, self.lines as u64)
    }

    /// `yes` when all four checks hold at the class's last line: `shaped` is the server's
    /// own reading of checks 1 and 2, at most [`MAX_OVERRUNS`] of the scored lines overran
    /// (check 3), and at least [`MIN_USABLE_SERIES`] of the lines were usable (check 4, a
    /// count: a class with fewer than 16 lines needs 12 of them). Otherwise `no`.
    pub(super) fn books_curve(self, shaped: Option<bool>) -> &'static str {
        let holds = shaped == Some(true)
            && self.overran <= MAX_OVERRUNS
            && self.usable >= MIN_USABLE_SERIES;
        if holds { "yes" } else { "no" }
    }
}
