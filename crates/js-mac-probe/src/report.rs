//! The report: a list of probes, each with a verdict and the facts it observed.

use std::collections::BTreeMap;
use std::io;

use serde::Serialize;

/// What a probe concluded. Only an asserted probe can fail; a probe whose answer is
/// genuinely unknown is [`Verdict::Info`] whatever it observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// An asserted probe whose expected outcome held.
    Pass,
    /// An asserted probe whose expected outcome did not hold.
    Fail,
    /// A reported probe: the facts are the answer.
    Info,
}

/// One probe: a name, its verdict and the facts it observed, in key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Probe {
    /// `area/what`, stable across runs.
    pub name: String,
    /// The conclusion.
    pub verdict: Verdict,
    /// What was observed (key to value).
    pub facts: BTreeMap<String, String>,
}

impl Probe {
    /// A probe with an expected outcome: [`Verdict::Pass`] when `held`, else [`Verdict::Fail`].
    #[must_use]
    pub fn asserted(name: &str, held: bool) -> Self {
        let verdict = [Verdict::Fail, Verdict::Pass][usize::from(held)];
        Self {
            name: name.to_owned(),
            verdict,
            facts: BTreeMap::new(),
        }
    }

    /// A probe that only reports what it saw.
    #[must_use]
    pub fn info(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            verdict: Verdict::Info,
            facts: BTreeMap::new(),
        }
    }

    /// Adds a fact; a later fact under the same key replaces the earlier one.
    #[must_use]
    pub fn fact(mut self, key: &str, value: impl ToString) -> Self {
        self.facts.insert(key.to_owned(), value.to_string());
        self
    }
}

/// All probes of one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Bumped when the shape of this document changes.
    pub schema: u32,
    /// The probes, in the order they ran.
    pub probes: Vec<Probe>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            schema: 1,
            probes: Vec::new(),
        }
    }
}

impl Report {
    /// Appends a probe.
    pub fn push(&mut self, probe: Probe) {
        self.probes.push(probe);
    }

    /// The names of the probes whose expected outcome did not hold.
    #[must_use]
    pub fn failed(&self) -> Vec<&str> {
        let failed = self.probes.iter().filter(|p| p.verdict == Verdict::Fail);
        failed.map(|p| p.name.as_str()).collect()
    }

    /// Writes the report as pretty JSON followed by a newline.
    ///
    /// # Errors
    /// The writer's error.
    pub fn write_json(&self, out: &mut impl io::Write) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *out, self).map_err(io::Error::from)?;
        out.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_asserted_probe_passes_or_fails_by_whether_it_held() {
        assert_eq!(Probe::asserted("a/b", true).verdict, Verdict::Pass);
        assert_eq!(Probe::asserted("a/b", false).verdict, Verdict::Fail);
    }

    #[test]
    fn a_reported_probe_never_fails() {
        let mut report = Report::default();
        report.push(Probe::info("a/unknown").fact("seen", "nothing"));
        report.push(Probe::asserted("a/known", true));
        assert!(report.failed().is_empty());
    }

    #[test]
    fn failed_names_the_failing_probes_in_order() {
        let mut report = Report::default();
        report.push(Probe::asserted("one", false));
        report.push(Probe::asserted("two", true));
        report.push(Probe::asserted("three", false));
        assert_eq!(report.failed(), ["one", "three"]);
    }

    #[test]
    fn a_later_fact_replaces_an_earlier_one_under_the_same_key() {
        let probe = Probe::info("p").fact("k", 1).fact("k", "two");
        assert_eq!(probe.facts["k"], "two");
    }

    #[test]
    fn the_json_is_stable() {
        let mut report = Report::default();
        report.push(Probe::asserted("sandbox/allow-default", true).fact("status", 0));
        report.push(Probe::info("net/dns").fact("resolved", false));
        let mut out = Vec::new();
        report.write_json(&mut out).unwrap();
        let expected = r#"{
  "schema": 1,
  "probes": [
    {
      "name": "sandbox/allow-default",
      "verdict": "pass",
      "facts": {
        "status": "0"
      }
    },
    {
      "name": "net/dns",
      "verdict": "info",
      "facts": {
        "resolved": "false"
      }
    }
  ]
}
"#;
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn a_failing_writer_is_an_error() {
        struct Broken;
        impl io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(Report::default().write_json(&mut Broken).is_err());
    }
}
