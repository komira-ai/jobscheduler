//! Whether the fake agent and the real one give the client the same outcomes: one
//! scenario run twice per seed, once with every agent a fake ([`crate::Scenario::hosts`])
//! and once with every agent real ([`crate::Scenario::real_hosts`]), and the client's
//! outcome of each item compared.
//!
//! An item's outcome ([`Outcome`]) is the terminal the client heard. The lease front folds
//! a run's result into it: an outcome of the run reads `succeeded`, `failed`, or
//! `cancelled` (a killed run), a run that ended with none reads `lost` (or `cancelled`
//! after a cancel), so two items whose terminals agree have results that agree as far as
//! the client can tell. Nothing else is compared: the traces differ by design, since the
//! two agents send different bodies, and the run ids the server gives are named in a
//! report but are not compared.
//!
//! [`compare`] runs both and returns every [`Disagreement`]: a seed whose items ended
//! differently, with each item, its outcome on each side and the last trace lines of both
//! runs; or a side whose scenario failed (a violation, an expect, a witness), which leaves
//! nothing to compare. [`differences`] is the comparison itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::observe::{ClientEvent, HowEnded, ItemId, Observations, RunId};
use crate::scenario::{Scenario, ended};

/// What the client heard of one item by the end of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The terminal it heard first, or `None` when the item never ended.
    pub terminal: Option<HowEnded>,
    /// The run the server gave the item, when the client learned one. Named in a report;
    /// not compared.
    pub run: Option<RunId>,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let terminal = self.terminal.map_or("none", HowEnded::name);
        let run = self.run.as_ref().map_or("none", RunId::as_str);
        write!(f, "terminal {terminal} (run {run})")
    }
}

/// The outcome of every item `observations` hold.
#[must_use]
pub fn outcomes(observations: &Observations) -> BTreeMap<ItemId, Outcome> {
    let queued = |event: &ClientEvent| match event {
        ClientEvent::Queued { run, .. } => Some(run.clone()),
        _ => None,
    };
    let each = observations.items.iter().map(|(item, record)| {
        let run = record.heard.iter().find_map(|heard| queued(&heard.event));
        let terminal = ended(record);
        (*item, Outcome { terminal, run })
    });
    each.collect()
}

/// The items whose terminals differ between `fake` and `real`, in order; an item only one
/// side has counts as one that never ended on the other.
#[must_use]
pub fn differences(
    fake: &BTreeMap<ItemId, Outcome>,
    real: &BTreeMap<ItemId, Outcome>,
) -> Vec<ItemId> {
    let terminal = |side: &BTreeMap<ItemId, Outcome>, item: &ItemId| {
        side.get(item).and_then(|outcome| outcome.terminal)
    };
    let items: BTreeSet<&ItemId> = fake.keys().chain(real.keys()).collect();
    let differ = items
        .into_iter()
        .filter(|item| terminal(fake, item) != terminal(real, item));
    differ.copied().collect()
}

/// Which agent a side of a comparison runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Every agent a fake.
    Fake,
    /// Every agent real.
    Real,
}

impl Side {
    /// `fake` or `real`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Side::Fake => "fake",
            Side::Real => "real",
        }
    }
}

/// One item that ended differently on the two sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDifference {
    /// The item.
    pub item: ItemId,
    /// Its outcome with fake agents.
    pub fake: Outcome,
    /// Its outcome with real agents.
    pub real: Outcome,
}

/// A way the two sides did not agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disagreement {
    /// One side's scenario failed, so its outcomes cannot be compared.
    Failed {
        /// The side.
        side: Side,
        /// The failure, as the scenario reports it.
        failure: String,
    },
    /// Items of one seed ended differently.
    Outcomes {
        /// The scenario.
        scenario: String,
        /// The seed.
        seed: u64,
        /// Each item that ended differently, in order.
        items: Vec<ItemDifference>,
        /// The last trace lines of the seed with fake agents, oldest first.
        fake_lines: Vec<String>,
        /// The last trace lines of the seed with real agents, oldest first.
        real_lines: Vec<String>,
    },
}

/// One heading line per failure or seed, a line per item, then the trace lines.
impl fmt::Display for Disagreement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Disagreement::Failed { side, failure } => {
                write!(
                    f,
                    "with {} agents the scenario failed: {failure}",
                    side.name()
                )
            }
            Disagreement::Outcomes {
                scenario,
                seed,
                items,
                fake_lines,
                real_lines,
            } => {
                writeln!(f, "scenario {scenario} seed {seed}: the agents disagree")?;
                for difference in items {
                    let ItemDifference { item, fake, real } = difference;
                    writeln!(f, "  item {}: fake {fake}, real {real}", item.0)?;
                }
                for (side, lines) in [(Side::Fake, fake_lines), (Side::Real, real_lines)] {
                    let heading =
                        format!("last {} trace lines, {} agents:", lines.len(), side.name());
                    writeln!(f, "{heading}")?;
                    for line in lines {
                        writeln!(f, "  {line}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

/// Runs seeds `0..seeds` of `fake` (every agent a fake) and of `real` (the same scenario
/// with every agent real), and returns every disagreement: a side that failed, or each
/// seed whose items ended differently. Empty when the two agree on every item of every
/// seed.
#[must_use]
pub fn compare(fake: &Scenario, real: &Scenario, seeds: u64) -> Vec<Disagreement> {
    let ran = [(Side::Fake, fake), (Side::Real, real)].map(|(side, scenario)| {
        let ran = scenario.try_run_seeds(seeds);
        ran.map_err(|failure| Disagreement::Failed {
            side,
            failure: failure.to_string(),
        })
    });
    let [fake_ran, real_ran] = ran;
    let (fake_runs, real_runs) = match (fake_ran, real_ran) {
        (Ok(fake_runs), Ok(real_runs)) => (fake_runs.runs, real_runs.runs),
        (fake_ran, real_ran) => {
            let failed = [fake_ran.err(), real_ran.err()];
            return failed.into_iter().flatten().collect();
        }
    };
    let pairs = fake_runs.iter().zip(&real_runs);
    let differing = pairs.filter_map(|(fake_run, real_run)| {
        let items = differences(&fake_run.outcomes, &real_run.outcomes);
        let item = |item: ItemId| ItemDifference {
            item,
            fake: side_outcome(&fake_run.outcomes, item),
            real: side_outcome(&real_run.outcomes, item),
        };
        let items: Vec<ItemDifference> = items.into_iter().map(item).collect();
        let seed = fake_run.seed;
        (!items.is_empty()).then(|| Disagreement::Outcomes {
            scenario: fake.name().to_owned(),
            seed,
            items,
            fake_lines: fake.last_lines(seed),
            real_lines: real.last_lines(seed),
        })
    });
    differing.collect()
}

/// The outcome of `item` on one side; one that side never had is an item that never ended.
fn side_outcome(side: &BTreeMap<ItemId, Outcome>, item: ItemId) -> Outcome {
    let none = Outcome {
        terminal: None,
        run: None,
    };
    side.get(&item).cloned().unwrap_or(none)
}
