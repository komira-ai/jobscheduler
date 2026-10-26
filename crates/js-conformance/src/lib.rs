//! The provider conformance suite for the delegated flavour (design 13.2).
//!
//! A provider is put under test by implementing [`Subject`], then calling [`run`].
//! The suite starts leases, stops them, scripts Unknown answers and checks each
//! outcome against the provider's real state, which the subject reads through
//! [`Subject::booked`] from its own tables and not through the provider's answers: a
//! suite that asks the provider whether it did what it said cannot fail.
//!
//! Every check builds a fresh subject over [`standard_hosts`] and compares what it
//! saw with what the contract says it must see. A check a provider cannot take part in
//! (a static pool never answers Unknown, so it cannot be scripted to) reports
//! [`Verdict::NotApplicable`] and is listed by [`Report::not_applicable`], never
//! silently skipped.
//!
//! The suite is shown able to fail by the broken fakes of `js-provider-fake`: each of
//! the five checks that read provider behaviour has a defect that fails exactly that
//! check (see this crate's tests). The three refusal checks guard `StartSpec::new`, which
//! every provider shares, so they fail when a rule is dropped from it.

mod checks;

use std::collections::BTreeMap;

use js_provider::{Call, Provider};
use js_types::{Capabilities, ClassId, HostId, LeaseId, Request, Resources, Shape};

pub use checks::Check;

/// A provider under test, with the hooks that read its real state.
pub trait Subject {
    /// The provider, as the scheduler would call it.
    fn provider(&mut self) -> &mut dyn Provider;

    /// The booked hosts and the lease on each, read from the provider's own state.
    fn booked(&self) -> BTreeMap<HostId, LeaseId>;

    /// Arranges for the next `call` to answer Unknown, with the effect landing when
    /// `landed` is set. Returns `false` when the provider cannot be made to answer
    /// Unknown, as a provider that owns its state and always knows.
    fn script_unknown(&mut self, call: Call, landed: bool) -> bool;
}

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The provider did what the contract says.
    Passed,
    /// The provider differed from the contract; the text says how.
    Failed(String),
    /// The provider cannot take part in this check.
    NotApplicable,
}

/// The verdicts of one run, in [`Check::ALL`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Each check with its verdict.
    pub results: Vec<(Check, Verdict)>,
}

impl Report {
    /// The checks that failed.
    pub fn failed(&self) -> Vec<Check> {
        self.with(|verdict| matches!(verdict, Verdict::Failed(_)))
    }

    /// The checks the provider could not take part in.
    pub fn not_applicable(&self) -> Vec<Check> {
        self.with(|verdict| matches!(verdict, Verdict::NotApplicable))
    }

    fn with(&self, keep: impl Fn(&Verdict) -> bool) -> Vec<Check> {
        let kept = self.results.iter().filter(|(_, verdict)| keep(verdict));
        kept.map(|(check, _)| *check).collect()
    }
}

/// Runs every check, each on a fresh subject from `make`. The subject must hold the
/// hosts of [`standard_hosts`] and serve the `gpu-host` class.
pub fn run<S: Subject>(make: impl Fn() -> S) -> Report {
    let results = Check::ALL.iter().map(|check| (*check, check.run(make())));
    Report {
        results: results.collect(),
    }
}

/// The hosts every subject is built over: `h1` and `h2`, each with one
/// `conformance-gpu`, 8000 CPU millis, 1 GiB of memory and no capabilities.
pub fn standard_hosts() -> Vec<(HostId, Shape)> {
    let offered = Resources::new(8000, 1 << 30, 1, Some("conformance-gpu".to_string()));
    let shape = Shape::new(
        offered.expect("constant resources are valid"),
        Capabilities::new(),
    );
    let names = ["h1", "h2"];
    let ids = names
        .iter()
        .map(|name| HostId::new(name).expect("constant id is valid"));
    ids.map(|id| (id, shape.clone())).collect()
}

/// A request that fits one standard host: class `gpu-host`, one conformance GPU, a
/// ten minute wall time cap and no labels.
pub fn standard_request() -> Request {
    let wanted = Resources::new(1000, 1, 1, Some("conformance-gpu".to_string()));
    Request {
        class: ClassId::new("gpu-host").expect("constant id is valid"),
        resources: wanted.expect("constant resources are valid"),
        capabilities: Capabilities::new(),
        max_wall: std::time::Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
    }
}
