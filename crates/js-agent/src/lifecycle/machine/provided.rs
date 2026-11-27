//! The mount providers of the agent (`docs/design/mount-providers.md`) as the lifecycle keeps
//! them: the client it was given ([`Lifecycle::attach_providers`]), the sources each run
//! holds until the run's scratch is removed, the releases that are owed, and the sweep each
//! provider is asked once when the agent has started.
//!
//! - **A run's sources** are remembered when its start is admitted (`providing`) and taken
//!   off the map at `Stage::Scratch`, once its file system is taken apart, into the
//!   releases that are owed.
//! - **A release** is asked of the provider until it answers, for a minute after the run
//!   ended, each source paced as a start's calls are (`pace`): after a failure it waits 1 s,
//!   then 2, 4, 8 and 16 s, with one `provider-backoff` line for the run at each such wait.
//!   Then the sources left are named in a `provider-release-failed` finding and left to the
//!   provider's own collection. It never taints the host, and it holds no slot: the run's
//!   cleanup does not wait for it.
//! - **The calls that failed** are counted for the whole agent: the `Prepare`s a provider did
//!   not serve and the calls sent again after a failure, both named in every
//!   `provider-backoff` line.
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
use super::pace::{Gate, Pace, Step};
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

/// A source whose release is owed, with the pace of its tries.
struct Owed {
    source: Source,
    pace: Pace,
}

/// The sources of a run that ended, whose release is owed.
struct Leaving {
    run_id: String,
    /// When the run's scratch was removed.
    since_ms: u64,
    /// The sources not yet released.
    left: Vec<Owed>,
}

/// A call to a provider that failed and now waits ([`Attached::backoff`]): which source, which
/// call, what the pace made of it, and the provider's reason.
pub(super) struct Backed {
    /// The source's provider.
    pub(super) provider: String,
    /// The source's place.
    pub(super) index: u32,
    /// The call that failed: `Prepare`, `Mount` or `Release`.
    pub(super) call: &'static str,
    /// The failures in a row and the wait.
    pub(super) step: Step,
    /// What the provider (or the transport) said.
    pub(super) why: String,
}

/// What tells a source apart for the jitter of its pace.
pub(super) fn seed(provider: &str, run_id: &str, index: u32) -> String {
    format!("{provider}/{run_id}#{index}")
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
    /// The pace of the `Prepare`s of each provider that failed one, by name, with its last
    /// reason; a provider that serves them has none.
    gates: BTreeMap<String, (Gate, String)>,
    /// The `Prepare`s a provider did not serve (unavailable or refused), since the agent
    /// started.
    turned_away: u64,
    /// The calls sent again after a failure of the same source's call, since the agent
    /// started.
    retries: u64,
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
            gates: BTreeMap::new(),
            turned_away: 0,
            retries: 0,
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

    /// The reason the provider `name` gave for the last `Prepare` it failed, while it fails
    /// them and its gate is shut at `now_ms`: a `Prepare` that is not out yet is not sent.
    pub(super) fn gate_shut(&self, name: &str, now_ms: u64) -> Option<&str> {
        self.gates
            .get(name)
            .filter(|(gate, _)| !gate.open(now_ms))
            .map(|(_, why)| why.as_str())
    }

    /// Asks the provider `provider` for the call `call` of a source whose pace is `pace`
    /// (`seed` names the source, [`seed`]), at `now_ms`, and keeps the count: a call sent
    /// after the source's last call failed is a retry; a `Prepare` moves the provider's gate
    /// and, unless it succeeds or is still out, counts as one turned away. A `Mount` answered
    /// with no directory counts as a failure for the pace, so it is not asked at every step.
    /// The step is the pace's, when the answer was a failure.
    pub(super) fn paced(
        &mut self,
        (provider, call): (&str, &ProviderCall),
        pace: &mut Pace,
        now_ms: u64,
        seed: &str,
    ) -> (ProviderAnswer, Option<Step>) {
        let prepare = matches!(call, ProviderCall::Prepare { .. });
        if pace.idle() {
            if pace.retrying() {
                self.retries = self.retries.saturating_add(1);
            }
            if prepare && let Some((gate, _)) = self.gates.get_mut(provider) {
                gate.sent(now_ms, provider);
            }
        }
        pace.asked(now_ms);
        let sent_ms = pace.sent_at().unwrap_or(now_ms);
        let answer = self.providers.ask(provider, call);
        if prepare {
            match &answer {
                ProviderAnswer::Done(_) => {
                    self.gates.remove(provider);
                }
                ProviderAnswer::Unavailable(said) => {
                    let (gate, why) = self.gates.entry(provider.to_owned()).or_default();
                    gate.failed(sent_ms, provider);
                    said.clone_into(why);
                    self.turned_away = self.turned_away.saturating_add(1);
                }
                ProviderAnswer::Refused(_) | ProviderAnswer::Failed(_) => {
                    self.turned_away = self.turned_away.saturating_add(1);
                }
                ProviderAnswer::Pending => {}
            }
        }
        let empty = ProviderAnswer::Unavailable("the provider answered no directory".to_owned());
        let paced = match (call, &answer) {
            (ProviderCall::Mount { .. }, ProviderAnswer::Done(None)) => &empty,
            _ => &answer,
        };
        let step = pace.answered(paced, now_ms, seed);
        (answer, step)
    }

    /// The one `provider-backoff` line of the run `run_id` for the calls of it that failed in
    /// one step and now wait (`backed`, at least one): the first, how many more wait with it,
    /// and the agent's counts.
    pub(super) fn backoff(&self, run_id: &str, backed: &[Backed]) -> Option<Event> {
        let first = backed.first()?;
        let more = match backed.len() - 1 {
            0 => String::new(),
            1 => "; 1 more source waits too".to_owned(),
            n => format!("; {n} more sources wait too"),
        };
        let message = format!(
            "the provider {} did not serve the {} of source {} ({} in a row): {}; asked again in \
             {} ms{more}; this agent: {} Prepares not served, {} calls sent again",
            first.provider,
            first.call,
            first.index,
            first.step.failures,
            first.why,
            first.step.wait_ms,
            self.turned_away,
            self.retries,
        );
        Some(Event::Finding(Finding {
            run_id: Some(run_id.to_owned()),
            code: code::PROVIDER_BACKOFF,
            message,
        }))
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
            let left = left
                .into_iter()
                .map(|source| Owed {
                    source,
                    pace: Pace::default(),
                })
                .collect();
            self.leaving.push(Leaving {
                run_id: run_id.to_owned(),
                since_ms: now_ms,
                left,
            });
        }
    }

    /// Asks again for every release that is owed and whose pace allows it. A run whose
    /// sources are all released has its record removed; one that is not, a minute after it
    /// ended, is reported with the sources left, its record removed, and dropped. A run with
    /// a release that failed in this step has one `provider-backoff` line.
    fn release(&mut self, state: &StateRoot, now_ms: u64, events: &mut Vec<Event>) {
        let mut leaving = std::mem::take(&mut self.leaving);
        leaving.retain_mut(|leave| {
            let run_id = leave.run_id.clone();
            let mut backed = Vec::new();
            leave.left.retain_mut(|owed| {
                if !owed.pace.due(now_ms) {
                    return true;
                }
                let index = owed.source.index;
                let call = ProviderCall::Release {
                    run_id: run_id.clone(),
                    index,
                };
                let provider = owed.source.provider.as_str();
                let seed = seed(provider, &run_id, index);
                let (answer, step) = self.paced((provider, &call), &mut owed.pace, now_ms, &seed);
                if let (
                    Some(step),
                    ProviderAnswer::Refused(why)
                    | ProviderAnswer::Failed(why)
                    | ProviderAnswer::Unavailable(why),
                ) = (step, &answer)
                {
                    backed.push(Backed {
                        provider: provider.to_owned(),
                        index,
                        call: "Release",
                        step,
                        why: why.clone(),
                    });
                }
                !matches!(answer, ProviderAnswer::Done(_))
            });
            events.extend(self.backoff(&run_id, &backed));
            let late = now_ms.saturating_sub(leave.since_ms) >= RELEASE_GIVE_UP_MS;
            match (leave.left.is_empty(), late) {
                (true, _) => {
                    provider_record::remove(state, &run_id);
                    false
                }
                (false, true) => {
                    let left: Vec<String> = leave
                        .left
                        .iter()
                        .map(|owed| {
                            format!("{} source {}", owed.source.provider, owed.source.index)
                        })
                        .collect();
                    events.push(Event::Finding(Finding {
                        run_id: Some(run_id.clone()),
                        code: code::PROVIDER_RELEASE_FAILED,
                        message: format!(
                            "not released within a minute: {}; left to the providers' own \
                             collection",
                            left.join(", ")
                        ),
                    }));
                    provider_record::remove(state, &run_id);
                    false
                }
                (false, false) => true,
            }
        });
        self.leaving = leaving;
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
            none.providers
                .ask("tierstore", &ProviderCall::Sweep { keep: Vec::new() }),
            ProviderAnswer::Unavailable(
                "this agent was not given the provider tierstore".to_owned()
            )
        );
    }
}
