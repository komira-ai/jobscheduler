//! The mount providers of the agent (`docs/design/mount-providers.md`) as the lifecycle keeps
//! them: the client it was given ([`Lifecycle::attach_providers`]), the sources each run
//! holds until the run's scratch is removed, the releases that are owed, and the sweep each
//! provider is asked once when the agent has started.
//!
//! - **A run's sources** are remembered when its start is admitted (`providing`) and taken
//!   off the map at `Stage::Scratch`, once its file system is taken apart, into the
//!   releases that are owed.
//! - **A release** is asked of the provider at every step until it answers, for a minute
//!   after the run ended. Then the sources left are named in a `provider-release-failed`
//!   finding and left to the provider's own collection. It never taints the host, and it
//!   holds no slot: the run's cleanup does not wait for it.
//! - **The sweep** (`Sweep(agent, keep)` with the runs the agent adopted) is asked of each
//!   provider when the agent has started, and again every ten seconds until answered, with
//!   one `provider-sweep-failed` finding for each provider that fails it. A start that names
//!   a provider whose sweep is not answered waits (`providing`): a sweep that came after
//!   its `Prepare` would release the start's source.
//! - **A restart** reads the records of the state root (`provider_record`): a run the agent
//!   adopted keeps its sources, to release them at its own end; every other run's sources
//!   are released now, and its record removed.

use std::collections::{BTreeMap, BTreeSet};

use super::Lifecycle;
use super::job::Stage;
use super::provider_record::{self, Source};
use crate::lifecycle::taint::StateRoot;
use crate::lifecycle::{Event, Finding, ProviderAnswer, ProviderCall, Providers, code};

/// How long after a run ended its release is tried at every step, before what is left is
/// reported and left to the provider's own collection.
const RELEASE_GIVE_UP_MS: u64 = 60_000;

/// How long after a failed sweep it is asked again.
const SWEEP_AGAIN_MS: u64 = 10_000;

/// The providers of an agent that was given none: no provider is there, so no start that
/// names one is admitted and no call is ever made.
struct NoProviders;

impl Providers for NoProviders {
    fn has(&self, _name: &str) -> bool {
        false
    }

    fn names(&self) -> Vec<String> {
        Vec::new()
    }

    fn ask(&mut self, provider: &str, _call: &ProviderCall) -> ProviderAnswer {
        ProviderAnswer::Unavailable(format!("this agent was not given the provider {provider}"))
    }
}

/// The sweep of one provider while it is not answered.
struct Sweep {
    /// The runs whose sources are kept.
    keep: Vec<String>,
    /// When it is asked again, after a failed answer; 0 until one failed.
    again_ms: u64,
    /// Whether a finding was made of it.
    told: bool,
}

/// The sources of a run that ended, whose release is owed.
struct Leaving {
    run_id: String,
    /// When the run's scratch was removed.
    since_ms: u64,
    /// The sources not yet released.
    left: Vec<Source>,
}

/// The providers the lifecycle was given and what it holds for them.
pub(super) struct Attached {
    providers: Box<dyn Providers>,
    /// How long a start may wait on its providers, from its admission.
    wait_ms: u64,
    /// The sweeps not answered yet, by provider.
    sweeps: BTreeMap<String, Sweep>,
    /// The sources of the runs that have not ended, by run id.
    runs: BTreeMap<String, Vec<Source>>,
    /// The releases that are owed.
    leaving: Vec<Leaving>,
}

impl Attached {
    /// An agent given no provider.
    pub(super) fn none() -> Self {
        Self::of(Box::new(NoProviders), 0)
    }

    fn of(providers: Box<dyn Providers>, wait_ms: u64) -> Self {
        Self {
            providers,
            wait_ms,
            sweeps: BTreeMap::new(),
            runs: BTreeMap::new(),
            leaving: Vec::new(),
        }
    }

    /// Whether the agent was given the provider `name`.
    pub(super) fn has(&self, name: &str) -> bool {
        self.providers.has(name)
    }

    /// How long a start may wait on its providers, in milliseconds.
    pub(super) fn wait_ms(&self) -> u64 {
        self.wait_ms
    }

    /// Whether the provider `name` has answered its sweep (so a start may be asked of it).
    pub(super) fn swept(&self, name: &str) -> bool {
        !self.sweeps.contains_key(name)
    }

    /// Asks the provider `provider` for `call`; see [`Providers::ask`].
    pub(super) fn ask(&mut self, provider: &str, call: &ProviderCall) -> ProviderAnswer {
        self.providers.ask(provider, call)
    }

    /// Remembers `sources` as the ones the run `run_id` holds.
    pub(super) fn keep(&mut self, run_id: &str, sources: Vec<Source>) {
        self.runs.insert(run_id.to_owned(), sources);
    }

    /// The run `run_id` has ended and its file system is taken apart: its sources, when it
    /// holds any, are to be released. A run is taken once, so a cleanup that goes through
    /// the same step again owes nothing twice.
    pub(super) fn leave(&mut self, run_id: &str, now_ms: u64) {
        if let Some(left) = self.runs.remove(run_id) {
            self.leaving.push(Leaving {
                run_id: run_id.to_owned(),
                since_ms: now_ms,
                left,
            });
        }
    }

    /// Asks again for every release that is owed. A run whose sources are all released has
    /// its record removed; one that is not, a minute after it ended, is reported with the
    /// sources left, its record removed, and dropped.
    fn release(&mut self, state: &StateRoot, now_ms: u64, events: &mut Vec<Event>) {
        let Self {
            providers, leaving, ..
        } = self;
        leaving.retain_mut(|leave| {
            let run_id = leave.run_id.as_str();
            leave.left.retain(|source| {
                let call = ProviderCall::Release {
                    run_id: run_id.to_owned(),
                    index: source.index,
                };
                !matches!(
                    providers.ask(&source.provider, &call),
                    ProviderAnswer::Done(_)
                )
            });
            let late = now_ms.saturating_sub(leave.since_ms) >= RELEASE_GIVE_UP_MS;
            match (leave.left.is_empty(), late) {
                (true, _) => {
                    provider_record::remove(state, run_id);
                    false
                }
                (false, true) => {
                    let left: Vec<String> = leave
                        .left
                        .iter()
                        .map(|source| format!("{} source {}", source.provider, source.index))
                        .collect();
                    events.push(Event::Finding(Finding {
                        run_id: Some(run_id.to_owned()),
                        code: code::PROVIDER_RELEASE_FAILED,
                        message: format!(
                            "not released within a minute: {}; left to the providers' own \
                             collection",
                            left.join(", ")
                        ),
                    }));
                    provider_record::remove(state, run_id);
                    false
                }
                (false, false) => true,
            }
        });
    }

    /// Asks every sweep that is not answered and is due; one that failed is asked again
    /// ten seconds later, and reported the first time.
    fn sweep(&mut self, now_ms: u64, events: &mut Vec<Event>) {
        let Self {
            providers, sweeps, ..
        } = self;
        sweeps.retain(|name, sweep| {
            if now_ms < sweep.again_ms {
                return true;
            }
            let call = ProviderCall::Sweep {
                keep: sweep.keep.clone(),
            };
            match providers.ask(name, &call) {
                ProviderAnswer::Done(_) => false,
                ProviderAnswer::Pending => true,
                ProviderAnswer::Refused(why)
                | ProviderAnswer::Failed(why)
                | ProviderAnswer::Unavailable(why) => {
                    sweep.again_ms = now_ms.saturating_add(SWEEP_AGAIN_MS);
                    if !sweep.told {
                        sweep.told = true;
                        events.push(Event::Finding(Finding {
                            run_id: None,
                            code: code::PROVIDER_SWEEP_FAILED,
                            message: format!(
                                "the provider {name} has not answered the sweep at start: {why}; \
                                 asked again every ten seconds, and a start that names it waits"
                            ),
                        }));
                    }
                    true
                }
            }
        });
    }
}

impl Lifecycle {
    /// Gives the lifecycle the mount providers the agent was given (`--mount-provider`), once
    /// it has adopted what an earlier process left, and asks each provider for its sweep:
    /// every source of an earlier process that is not a run this lifecycle holds is released.
    /// `wait_ms` is how long a start may wait on its providers, from its admission.
    ///
    /// The runs held are the adopted jobs and the containers that could not be removed: they
    /// keep their sources, which are released when their scratch is removed. The record of
    /// every other run in the state root is read and its sources are released now (a record
    /// that names a provider this agent was not given is reported and its source left; one
    /// that cannot be read is removed and reported). The sweeps are asked at once and again
    /// at every step until answered; a start that names a provider waits for its sweep.
    ///
    /// Without this a start that names a provider is refused (`input-not-allowed`): the
    /// agent was not given one.
    pub fn attach_providers(
        &mut self,
        mut providers: Box<dyn Providers>,
        wait_ms: u64,
    ) -> Vec<Event> {
        providers.set_wake(self.wake.clone());
        let mut events = Vec::new();
        let now = self.clock.now_ms();
        let stuck = self.stuck.iter().filter_map(|held| held.run_id.clone());
        let jobs = self.jobs.iter().filter(|job| job.stage != Stage::Freed);
        let kept: BTreeSet<String> = jobs.map(|job| job.run_id.clone()).chain(stuck).collect();
        let mut attached = Attached::of(providers, wait_ms);
        for (run_id, sources) in provider_record::load(&self.config.state, &mut events) {
            let (known, unknown): (Vec<Source>, Vec<Source>) = sources
                .into_iter()
                .partition(|source| attached.has(&source.provider));
            if !unknown.is_empty() {
                let names: Vec<&str> = unknown.iter().map(|s| s.provider.as_str()).collect();
                events.push(Event::Finding(Finding {
                    run_id: None,
                    code: code::PROVIDER_RECORD_FAILED,
                    message: format!(
                        "the provider record of run {run_id} names {}, which this agent was not \
                         given: those sources are left to the provider's own collection",
                        names.join(", ")
                    ),
                }));
            }
            attached.keep(&run_id, known);
            if !kept.contains(&run_id) {
                attached.leave(&run_id, now);
            }
        }
        let keep: Vec<String> = kept.into_iter().collect();
        for name in attached.providers.names() {
            let sweep = Sweep {
                keep: keep.clone(),
                again_ms: 0,
                told: false,
            };
            attached.sweeps.insert(name, sweep);
        }
        self.providers = attached;
        self.retry_providers(&mut events);
        events
    }

    /// At every step: asks again for the sweeps that are not answered and the releases that
    /// are owed.
    pub(super) fn retry_providers(&mut self, events: &mut Vec<Event>) {
        let now = self.clock.now_ms();
        self.providers.sweep(now, events);
        self.providers.release(&self.config.state, now, events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the providers of an agent given none have no provider by any name, name
    /// none, and answer a call `unavailable` naming the provider. Catches a stand-in that
    /// admits a provider the agent was never given.
    #[test]
    fn an_agent_given_no_provider_has_none() {
        let mut none = Attached::none();
        assert!(!none.has("tierstore"));
        assert!(none.providers.names().is_empty());
        assert_eq!(
            none.ask("tierstore", &ProviderCall::Sweep { keep: Vec::new() }),
            ProviderAnswer::Unavailable(
                "this agent was not given the provider tierstore".to_owned()
            )
        );
    }
}
