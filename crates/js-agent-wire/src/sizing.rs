//! Where a job's booking came from ([`StartSizing`], on [`crate::JobStartSpec::sizing`]) and
//! the throttled share of a job's CPU periods ([`CpuThrottle::throttled_permille`]; the counts
//! are [`crate::JobUsage::cpu_throttle`]). The agent books a job's CPU as a share of the node's
//! cores (its lease's `cpu.weight`), never as a ceiling; both fields are there so that a run's
//! CPU can be read for what it is: a run its host never throttled used what it wanted, and
//! one a host throttled (an agent from before the share, which wrote `cpu.max`) used at most
//! what it booked.
//!
//! Both are additive and never part of a message's acceptance: a value that does not decode
//! reads as absent, and an agent or a server from before them ignores them.

use serde::{Deserialize, Serialize};

use crate::check;
use crate::limits;
use crate::per_job::CpuThrottle;

/// Which history gave one dimension of a job's booking, as the server's placement recorded
/// it. The names are those the server's placement log gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BookedFrom {
    /// The job named it: the booking is what the job asked for.
    Named,
    /// The estimate its own size class learned from its runs.
    Class,
    /// The estimate of the fallback class the job names, standing in for the cold size.
    Fallback,
    /// The cold size: no class the job is in has an estimate.
    Cold,
    /// The whole of the most bookable memory a host has: the class's runs reach it.
    OverCap,
    /// The history sized it, and the server did not record which (a placement from before
    /// the record), or a name this agent does not know.
    #[serde(other)]
    Unknown,
}

impl BookedFrom {
    /// The name it has on the wire and in the agent's journal: `named`, `class`, `fallback`,
    /// `cold`, `over-cap` or `unknown`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Named => "named",
            Self::Class => "class",
            Self::Fallback => "fallback",
            Self::Cold => "cold",
            Self::OverCap => "over-cap",
            Self::Unknown => "unknown",
        }
    }
}

/// What a job's booking came from: its size class, and which history gave its CPU and its
/// memory ([`BookedFrom`]). The agent writes it in its journal at the job's start and its
/// result, beside the booking; it decides nothing by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSizing {
    /// The name of the job's size class, the key its runs are learned under: its
    /// `size-class` label, else `run-` and a hash of its image name and command. At most
    /// [`limits::MAX_ID_LEN`] bytes of `[A-Za-z0-9._-]`, leading alphanumeric
    /// ([`StartSizing::checked`]); absent when the server names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Where the CPU booking came from.
    pub cpu: BookedFrom,
    /// Where the memory booking came from.
    pub memory: BookedFrom,
}

impl StartSizing {
    /// The sizing as the agent keeps it: a class outside the grammar of an id is left out,
    /// so that the journal line it goes into stays one line of plain words; the rest is kept.
    #[must_use]
    pub fn checked(self) -> Self {
        let at = check::At::new("sizing", crate::refusal::INVALID_JOB).field("sizing.class");
        let valid =
            |class: &String| check::token(at, class, limits::MAX_ID_LEN, check::ident_byte).is_ok();
        Self {
            class: self.class.filter(valid),
            ..self
        }
    }
}

/// The throttled share of a job's CPU periods ([`CpuThrottle`], which the usage carries),
/// for the agent's journal.
impl CpuThrottle {
    /// The throttled share of the periods, in thousandths; 0 for a group with no period (no
    /// `cpu.max`).
    #[must_use]
    pub fn throttled_permille(&self) -> u64 {
        let share = u128::from(self.throttled_periods) * 1000 / u128::from(self.periods.max(1));
        u64::try_from(share).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a source the server adds later failing the start it rides on: an unknown name
    /// reads as [`BookedFrom::Unknown`], and each known name reads as itself, in the names the
    /// placement log gives.
    #[test]
    fn a_source_decodes_by_its_logged_name_and_an_unknown_one_as_unknown() {
        let read = |text: &str| serde_json::from_str::<BookedFrom>(text).expect("decodes");
        for (text, from) in [
            ("\"named\"", BookedFrom::Named),
            ("\"class\"", BookedFrom::Class),
            ("\"fallback\"", BookedFrom::Fallback),
            ("\"cold\"", BookedFrom::Cold),
            ("\"over-cap\"", BookedFrom::OverCap),
            ("\"learned-twice\"", BookedFrom::Unknown),
        ] {
            assert_eq!(read(text), from, "{text}");
            if from != BookedFrom::Unknown {
                assert_eq!(format!("\"{}\"", from.as_str()), text, "the logged name");
            }
        }
        assert_eq!(BookedFrom::Unknown.as_str(), "unknown");
        assert_eq!(
            serde_json::to_string(&BookedFrom::OverCap).expect("encodes"),
            "\"over-cap\""
        );
    }

    /// Catches a class that would break the journal line (a line break, a space, a name over
    /// the bound) kept, and a valid one or the sources dropped with it.
    #[test]
    fn a_class_outside_the_id_grammar_is_left_out_and_the_rest_kept() {
        let sizing = |class: &str| StartSizing {
            class: Some(class.to_owned()),
            cpu: BookedFrom::Class,
            memory: BookedFrom::Cold,
        };
        let long = "a".repeat(limits::MAX_ID_LEN + 1);
        for bad in ["two\nlines", "a b", "", "-lead", long.as_str()] {
            assert_eq!(
                sizing(bad).checked(),
                StartSizing {
                    class: None,
                    cpu: BookedFrom::Class,
                    memory: BookedFrom::Cold,
                },
                "{bad:?}"
            );
        }
        let edge = "a".repeat(limits::MAX_ID_LEN);
        for good in [
            "root.crates.js-sched.js-sched",
            "run-00ff00ff00ff00ff",
            edge.as_str(),
        ] {
            assert_eq!(sizing(good).checked(), sizing(good), "{good}");
        }
    }

    /// Catches a share computed against the wrong count, or a division by the zero periods
    /// of a group with no `cpu.max`.
    #[test]
    fn the_throttled_share_is_thousandths_of_the_periods_and_zero_without_any() {
        let share = |periods, throttled_periods| {
            CpuThrottle {
                periods,
                throttled_periods,
            }
            .throttled_permille()
        };
        assert_eq!(share(0, 0), 0);
        assert_eq!(share(5597, 5585), 997);
        assert_eq!(share(4, 1), 250);
        assert_eq!(share(4, 4), 1000);
        assert_eq!(share(u64::MAX, u64::MAX), 1000);
    }
}
