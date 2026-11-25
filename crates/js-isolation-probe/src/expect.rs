//! The pinned answers (`expected.toml`, compiled in) and the verdict of a run against them.
//!
//! Each phase has a table of `name = "outcome"`. A run passes when every pinned name ran and
//! answered as pinned. A name that answered otherwise DIFFERS, a pinned name the run did not
//! make is NOT RUN (an argument the runbook gives was left out), and both fail the run by
//! name. A line with no pin is UNPINNED: printed, never failing, so an extra argument can be
//! tried without a change here.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::outcome::{Line, Outcome};

/// The pinned answers, as committed.
pub const PINNED: &str = include_str!("../expected.toml");

/// Which table of the pinned answers a run is judged by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// `probe`: writes, reads, network, privilege and resources.
    Probe,
    /// `leave`: the markers a job is allowed to make.
    Leave,
    /// `find` on the slot whose user left the markers.
    FindSameSlot,
    /// `find` on another slot.
    FindOtherSlot,
}

impl Section {
    /// The table's name in `expected.toml`.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Leave => "leave",
            Self::FindSameSlot => "find-same-slot",
            Self::FindOtherSlot => "find-other-slot",
        }
    }
}

/// Why the pinned answers could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PinError {
    /// Not TOML of the four tables.
    #[error("expected.toml: {0}")]
    Toml(String),
    /// A value that is not an outcome word.
    #[error("expected.toml: [{section}] {name} = {value:?} is not an outcome")]
    Outcome {
        /// The table.
        section: &'static str,
        /// The probe's name.
        name: String,
        /// What was written.
        value: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct File {
    probe: BTreeMap<String, String>,
    leave: BTreeMap<String, String>,
    find_same_slot: BTreeMap<String, String>,
    find_other_slot: BTreeMap<String, String>,
}

/// The four tables of pinned answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    tables: [(Section, BTreeMap<String, Outcome>); 4],
}

impl Pins {
    /// The pins in `text`.
    ///
    /// # Errors
    /// [`PinError`] when the text is not the four tables or a value is not an outcome.
    pub fn parse(text: &str) -> Result<Self, PinError> {
        let file: File = toml::from_str(text).map_err(|e| PinError::Toml(e.to_string()))?;
        Ok(Self {
            tables: [
                (Section::Probe, words(Section::Probe, file.probe)?),
                (Section::Leave, words(Section::Leave, file.leave)?),
                (
                    Section::FindSameSlot,
                    words(Section::FindSameSlot, file.find_same_slot)?,
                ),
                (
                    Section::FindOtherSlot,
                    words(Section::FindOtherSlot, file.find_other_slot)?,
                ),
            ],
        })
    }

    /// The committed pins.
    ///
    /// # Errors
    /// As [`Pins::parse`].
    pub fn committed() -> Result<Self, PinError> {
        Self::parse(PINNED)
    }

    /// The table of `section`.
    #[must_use]
    pub fn table(&self, section: Section) -> &BTreeMap<String, Outcome> {
        let (_, table) = &self.tables[section as usize];
        table
    }
}

fn words(
    section: Section,
    raw: BTreeMap<String, String>,
) -> Result<BTreeMap<String, Outcome>, PinError> {
    raw.into_iter()
        .map(|(name, value)| match Outcome::parse(&value) {
            Some(outcome) => Ok((name, outcome)),
            None => Err(PinError::Outcome {
                section: section.key(),
                name,
                value,
            }),
        })
        .collect()
}

/// How a run compares with its table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Lines that answered as pinned.
    pub matched: usize,
    /// `(name, pinned, got)` of each line that answered otherwise.
    pub differ: Vec<(String, Outcome, Outcome)>,
    /// `(name, pinned)` of each pin the run did not make.
    pub not_run: Vec<(String, Outcome)>,
    /// `(name, got)` of each line with no pin.
    pub unpinned: Vec<(String, Outcome)>,
}

impl Verdict {
    /// Whether the run passes: nothing differs and nothing pinned was left out.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.differ.is_empty() && self.not_run.is_empty()
    }

    /// The verdict's lines, the last one `verdict: PASS` or `verdict: FAIL ...`.
    #[must_use]
    pub fn lines(&self, section: Section) -> Vec<String> {
        let mut out = vec![format!(
            "pinned [{}]: {} as pinned, {} differ, {} not run, {} unpinned",
            section.key(),
            self.matched,
            self.differ.len(),
            self.not_run.len(),
            self.unpinned.len()
        )];
        for (name, pinned, got) in &self.differ {
            out.push(format!("DIFFERS {name}: pinned {pinned}, got {got}"));
        }
        for (name, pinned) in &self.not_run {
            out.push(format!("NOT RUN {name}: pinned {pinned}"));
        }
        for (name, got) in &self.unpinned {
            out.push(format!("UNPINNED {name}: {got}"));
        }
        if self.passed() {
            out.push("verdict: PASS".to_owned());
        } else {
            let names: Vec<&str> = self
                .differ
                .iter()
                .map(|(n, _, _)| n.as_str())
                .chain(self.not_run.iter().map(|(n, _)| n.as_str()))
                .collect();
            out.push(format!("verdict: FAIL {}", names.join(" ")));
        }
        out
    }
}

/// The verdict of `lines` against `table`. When a name is printed twice, its first line is
/// judged and the second is unpinned.
#[must_use]
pub fn judge(table: &BTreeMap<String, Outcome>, lines: &[Line]) -> Verdict {
    let mut verdict = Verdict::default();
    let mut seen = std::collections::BTreeSet::new();
    for line in lines {
        let first = seen.insert(line.name.as_str());
        match table.get(&line.name) {
            Some(pinned) if first => {
                if *pinned == line.outcome {
                    verdict.matched += 1;
                } else {
                    verdict
                        .differ
                        .push((line.name.clone(), *pinned, line.outcome));
                }
            }
            _ => verdict.unpinned.push((line.name.clone(), line.outcome)),
        }
    }
    for (name, pinned) in table {
        if !seen.contains(name.as_str()) {
            verdict.not_run.push((name.clone(), *pinned));
        }
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = r#"
[probe]
"write:run-dir" = "allowed"
"write:etc" = "denied"
"connect:job-service" = "denied"

[leave]
"leave:named-semaphore" = "allowed"

[find-same-slot]
"find:named-semaphore" = "found"

[find-other-slot]
"find:named-semaphore" = "found"
"#;

    #[test]
    fn the_committed_pins_parse_and_fill_every_table() {
        let pins = Pins::committed().unwrap();
        for section in [
            Section::Probe,
            Section::Leave,
            Section::FindSameSlot,
            Section::FindOtherSlot,
        ] {
            assert!(!pins.table(section).is_empty(), "{}", section.key());
        }
    }

    #[test]
    fn a_run_that_answers_every_pin_passes() {
        let pins = Pins::parse(SMALL).unwrap();
        let lines = [
            Line::new("write:run-dir", Outcome::Allowed),
            Line::new("write:etc", Outcome::Denied),
            Line::new("connect:job-service", Outcome::Denied),
        ];
        let verdict = judge(pins.table(Section::Probe), &lines);
        assert!(verdict.passed());
        assert_eq!(
            verdict.lines(Section::Probe),
            [
                "pinned [probe]: 3 as pinned, 0 differ, 0 not run, 0 unpinned",
                "verdict: PASS"
            ]
        );
    }

    #[test]
    fn a_different_answer_and_a_missing_probe_fail_by_name() {
        let pins = Pins::parse(SMALL).unwrap();
        let lines = [
            Line::new("write:run-dir", Outcome::Allowed),
            Line::new("write:etc", Outcome::Allowed),
            Line::new("read:extra", Outcome::Allowed),
        ];
        let verdict = judge(pins.table(Section::Probe), &lines);
        assert!(!verdict.passed());
        assert_eq!(
            verdict.lines(Section::Probe),
            [
                "pinned [probe]: 1 as pinned, 1 differ, 1 not run, 1 unpinned",
                "DIFFERS write:etc: pinned denied, got allowed",
                "NOT RUN connect:job-service: pinned denied",
                "UNPINNED read:extra: allowed",
                "verdict: FAIL write:etc connect:job-service",
            ]
        );
    }

    #[test]
    fn an_unpinned_line_alone_does_not_fail_and_a_repeat_is_unpinned() {
        let pins = Pins::parse(SMALL).unwrap();
        let lines = [
            Line::new("find:named-semaphore", Outcome::Found),
            Line::new("find:named-semaphore", Outcome::Gone),
        ];
        let verdict = judge(pins.table(Section::FindOtherSlot), &lines);
        assert!(verdict.passed());
        assert_eq!(
            verdict.unpinned,
            [("find:named-semaphore".to_owned(), Outcome::Gone)]
        );
    }

    #[test]
    fn a_value_that_is_no_outcome_is_refused_by_name() {
        let text = SMALL.replace(r#""write:etc" = "denied""#, r#""write:etc" = "nope""#);
        assert_eq!(
            Pins::parse(&text),
            Err(PinError::Outcome {
                section: "probe",
                name: "write:etc".to_owned(),
                value: "nope".to_owned()
            })
        );
        let err = Pins::parse(&text).unwrap_err();
        assert_eq!(
            err.to_string(),
            "expected.toml: [probe] write:etc = \"nope\" is not an outcome"
        );
    }

    #[test]
    fn a_bad_value_in_any_table_is_refused_with_that_table() {
        for (from, to, section) in [
            (
                r#""leave:named-semaphore" = "allowed""#,
                r#""leave:named-semaphore" = "x""#,
                "leave",
            ),
            (
                "[find-same-slot]\n\"find:named-semaphore\" = \"found\"",
                "[find-same-slot]\n\"find:named-semaphore\" = \"x\"",
                "find-same-slot",
            ),
            (
                "[find-other-slot]\n\"find:named-semaphore\" = \"found\"",
                "[find-other-slot]\n\"find:named-semaphore\" = \"x\"",
                "find-other-slot",
            ),
        ] {
            let text = SMALL.replace(from, to);
            let name = if section == "leave" {
                "leave:named-semaphore"
            } else {
                "find:named-semaphore"
            };
            assert_eq!(
                Pins::parse(&text),
                Err(PinError::Outcome {
                    section,
                    name: name.to_owned(),
                    value: "x".to_owned()
                })
            );
        }
    }

    #[test]
    fn text_that_is_not_the_four_tables_is_refused() {
        let err = Pins::parse("[probe]\n[other]\n").unwrap_err();
        assert!(matches!(err, PinError::Toml(_)), "{err:?}");
        assert!(err.to_string().starts_with("expected.toml: "), "{err}");
    }
}
