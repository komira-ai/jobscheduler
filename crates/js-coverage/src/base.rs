//! The comparison with the base branch's `coverage-baseline`.
//!
//! The ratchet in [`crate::ratchet`] compares a measurement with the baseline file of
//! the same change, so a change that adds uncovered code and raises its own file to
//! match would pass it. This comparison closes that: CI reads the base branch's file
//! with git and checks the change's file against it ([`compare`]). A count of missed
//! lines or branches may stay or fall, never rise, and a crate the base file does not
//! list enters at zero. A recorded `-` (nothing to measure) counts as 0, as it does for
//! `js-coverage tighten`. Removing a crate, or lowering a count, passes.

use std::fmt;

use crate::baseline::Baseline;
use crate::ratchet::Metric;

/// One way the change's file is looser than the base branch's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A crate the base file lists records a higher count.
    Rose {
        /// The crate.
        krate: String,
        /// The metric.
        metric: Metric,
        /// The base branch's count.
        base: u64,
        /// The change's count.
        head: u64,
    },
    /// A crate the base file does not list enters with a count above 0.
    NewCrate {
        /// The crate.
        krate: String,
        /// The metric.
        metric: Metric,
        /// The change's count.
        head: u64,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Rose {
                krate,
                metric,
                base,
                head,
            } => write!(
                f,
                "{krate}: missed {metric} rose from {base} on the base branch to {head}; cover the code instead of raising the count"
            ),
            Violation::NewCrate {
                krate,
                metric,
                head,
            } => write!(
                f,
                "{krate}: a new crate enters at zero, but it records {head} missed {metric}; cover its code"
            ),
        }
    }
}

/// Every count of `head` above the base branch's, and every new crate that does not
/// enter at zero; empty when `head` passes.
pub fn compare(base: &Baseline, head: &Baseline) -> Vec<Violation> {
    let mut out = Vec::new();
    for (krate, entry) in head {
        for metric in [Metric::Lines, Metric::Branches] {
            let now = entry.get(metric).unwrap_or(0);
            match base.get(krate) {
                Some(old) => {
                    let before = old.get(metric).unwrap_or(0);
                    if now > before {
                        out.push(Violation::Rose {
                            krate: krate.clone(),
                            metric,
                            base: before,
                            head: now,
                        });
                    }
                }
                None => {
                    if now > 0 {
                        out.push(Violation::NewCrate {
                            krate: krate.clone(),
                            metric,
                            head: now,
                        });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline;

    fn file(text: &str) -> Baseline {
        baseline::parse(text).expect("valid baseline")
    }

    /// Asserts that a file equal to the base branch's passes, as does one that lowers a
    /// count, removes a crate, or records `-` where the base has a count.
    #[test]
    fn equal_lower_and_removed_pass() {
        let base = file("a 3 2\nb 0 0\nc 1 -\n");
        assert!(compare(&base, &base).is_empty());
        assert!(compare(&base, &file("a 2 0\nb 0 0\nc - -\n")).is_empty());
        assert!(compare(&base, &file("a 3 2\n")).is_empty());
        assert!(compare(&base, &Baseline::new()).is_empty());
    }

    /// Asserts that a higher missed-line count, or a higher missed-branch count, of a
    /// crate the base lists is reported on the right crate and metric with both values,
    /// and that `-` on the base counts as 0.
    #[test]
    fn a_rise_in_either_metric_is_reported() {
        let base = file("a 3 2\nb - -\n");
        let got = compare(&base, &file("a 4 2\nb - 1\n"));
        assert_eq!(
            got,
            vec![
                Violation::Rose {
                    krate: "a".into(),
                    metric: Metric::Lines,
                    base: 3,
                    head: 4
                },
                Violation::Rose {
                    krate: "b".into(),
                    metric: Metric::Branches,
                    base: 0,
                    head: 1
                },
            ]
        );
        let got = compare(&base, &file("a 3 9\nb - -\n"));
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].to_string(),
            "a: missed branches rose from 2 on the base branch to 9; cover the code instead of raising the count"
        );
    }

    /// Asserts that a crate absent from the base file passes only at zero (`0 0`, `0 -`
    /// or `- -`), and is reported for each metric that records more.
    #[test]
    fn a_new_crate_enters_at_zero() {
        let base = file("a 3 2\n");
        assert!(compare(&base, &file("a 3 2\nn 0 0\nm 0 -\nk - -\n")).is_empty());
        let got = compare(&base, &file("a 3 2\nn 1 2\n"));
        assert_eq!(
            got,
            vec![
                Violation::NewCrate {
                    krate: "n".into(),
                    metric: Metric::Lines,
                    head: 1
                },
                Violation::NewCrate {
                    krate: "n".into(),
                    metric: Metric::Branches,
                    head: 2
                },
            ]
        );
        assert_eq!(
            got[0].to_string(),
            "n: a new crate enters at zero, but it records 1 missed lines; cover its code"
        );
    }
}
