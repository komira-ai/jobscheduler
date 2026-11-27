//! The providers' part of a start (`docs/design/mount-providers.md`, section 4): a start
//! whose layers or mounts name a provider keeps its [`Providing`] in [`Phase::Resolving`],
//! beside its images and the lookups of its other host paths, until every such source has
//! the directory its provider made for it; then each source is replaced by that directory
//! as a host path, the step `provided` is marked, and the lookups of those paths are
//! followed with the rest.
//!
//! - **At admission**, once the start has its slot, the sources are remembered for the
//!   release at the run's end, each provider is asked to `Prepare` its source, and then the
//!   record of the sources is written ([`provider_record`], so a restart can release what
//!   may exist). The asks go before the start's images are asked for, so the provider
//!   fetches while the image is pulled and the container made, and before the record, so
//!   the record's syncs overlap the provider's first work. A crash between a `Prepare` and
//!   the record is covered by the sweep each provider is asked when the agent starts
//!   (`provided`). A start whose record cannot be written fails, and its cleanup releases
//!   what was asked.
//! - **At every step** each source that has no directory yet is asked again: `Prepare`
//!   until the provider has taken it, then `Mount` until it answers the directory. The
//!   provider client sends a source's first `Mount` itself as soon as its `Prepare` succeeds
//!   ([`crate::lifecycle::Providers::ask`]), so the step that takes the `Prepare`'s answer
//!   usually finds the `Mount` out or answered. A call that is out is asked at every step
//!   (that sends nothing), and is never sent twice. A
//!   provider that is unavailable (or answers a `Mount` with no directory) is asked again
//!   after a wait that grows with each failure in a row, per source, and a provider that
//!   fails `Prepare`s is sent one `Prepare` at a time until one succeeds (`pace`); each step
//!   in which a start's calls failed gives one `provider-backoff` line for it. One that
//!   refuses the request as wrong in itself (`INVALID_ARGUMENT`) fails the start
//!   `provider-refused`, the job's own fault; one that cannot serve it in its present state
//!   (`FAILED_PRECONDITION`) fails it `provider-failed`, the host's, so the server places the
//!   job again elsewhere; a directory that is not an absolute path in normal form beneath a
//!   mount root of the agent's (a root it may write, for a read-write mount) fails it
//!   (`input-not-allowed`). A provider whose sweep at start is not answered yet is not asked.
//! - **From the admission to `--mount-provider-wait`** a start may wait; past it the start
//!   fails `input-unavailable` naming the first source without a directory and the
//!   provider's last reason, once.
//! - **A stop** while providing ends the start at once, whatever a call in flight does: its
//!   sources are released at its cleanup like any run's (`provided`).

use std::collections::BTreeMap;

use js_agent_wire::{Layer, MountMode, MountSource, refusal};

use super::job::{Phase, Waiting};
use super::pace::{Pace, Step};
use super::provided::{Attached, Backed, seed};
use super::provider_record::{self, Source};
use super::{Failure, Lifecycle};
use crate::lifecycle::runtime::JobFs;
use crate::lifecycle::{Event, MountRoots, ProviderAnswer, ProviderCall, code};

/// One source a provider makes, as the start names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Provided<'a> {
    /// The source's place among the start's layers, then its mounts, from 0.
    pub(super) index: u32,
    /// The provider's name.
    pub(super) provider: &'a str,
    /// What the provider is asked for.
    pub(super) reference: &'a str,
    /// How the job uses the tree: read-only for a layer, the mount's mode for a mount.
    pub(super) mode: MountMode,
}

impl JobFs {
    /// The sources a provider makes, layers first and then mounts, each with its place among
    /// them (layers counted whatever their kind, mounts after the last layer).
    pub(super) fn providers(&self) -> Vec<Provided<'_>> {
        let layers = (0u32..)
            .zip(&self.layers)
            .filter_map(|(index, layer)| match layer {
                Layer::Provider {
                    provider,
                    reference,
                } => Some(Provided {
                    index,
                    provider,
                    reference,
                    mode: MountMode::ReadOnly,
                }),
                Layer::HostPath { .. } | Layer::Image { .. } => None,
            });
        let first = u32::try_from(self.layers.len()).unwrap_or(u32::MAX);
        let mounts = (first..)
            .zip(&self.mounts)
            .filter_map(|(index, mount)| match &mount.source {
                MountSource::Provider {
                    provider,
                    reference,
                } => Some(Provided {
                    index,
                    provider,
                    reference,
                    mode: mount.mode,
                }),
                MountSource::HostPath { .. } => None,
            });
        layers.chain(mounts).collect()
    }
}

/// A source a start waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    provider: String,
    index: u32,
    reference: String,
    mode: MountMode,
    /// The provider has taken the ref (`Prepare` answered).
    prepared: bool,
    /// The directory the provider answered, once it is one the job may be given.
    path: Option<String>,
    /// Why the last call did not get the directory, for the failure of a start that waited
    /// too long.
    why: String,
    /// The pace of its calls.
    pace: Pace,
}

impl Wanted {
    fn of(source: &Provided<'_>) -> Self {
        Self {
            provider: source.provider.to_owned(),
            index: source.index,
            reference: source.reference.to_owned(),
            mode: source.mode,
            prepared: false,
            path: None,
            why: "no answer".to_owned(),
            pace: Pace::default(),
        }
    }

    /// The failure of a start whose source the provider refused.
    fn refused(&self, why: &str) -> Failure {
        (
            refusal::PROVIDER_REFUSED,
            code::START_FAILED,
            format!(
                "the provider {} refused source {}: {why}",
                self.provider, self.index
            ),
            false,
        )
    }

    /// The failure of a start whose source the provider cannot serve in its present state:
    /// the host's, not the job's.
    fn failed(&self, why: &str) -> Failure {
        (
            refusal::PROVIDER_FAILED,
            code::START_FAILED,
            format!(
                "the provider {} cannot serve source {} now: {why}",
                self.provider, self.index
            ),
            false,
        )
    }

    /// What an answer that is not a success makes of the source: nothing yet (the call is
    /// out, or the provider is not available and is asked again), or the failure of the
    /// start (the provider refused the request, or cannot serve it here).
    fn settle(&mut self, answer: ProviderAnswer) -> Result<(), Failure> {
        match answer {
            ProviderAnswer::Pending | ProviderAnswer::Done(_) => Ok(()),
            ProviderAnswer::Refused(why) => Err(self.refused(&why)),
            ProviderAnswer::Failed(why) => Err(self.failed(&why)),
            ProviderAnswer::Unavailable(why) => {
                self.why = why;
                Ok(())
            }
        }
    }

    /// Takes the directory the provider answered when the job may be given it: an absolute
    /// path in normal form beneath a mount root of the agent's, a root it may write for a
    /// read-write mount. The provider's text is not trusted further: the runtime opens it
    /// beneath the root like any host path.
    fn accept(&mut self, path: String, roots: &MountRoots) -> Result<(), Failure> {
        let not_allowed = |why: String| -> Failure {
            (
                refusal::INPUT_NOT_ALLOWED,
                code::START_FAILED,
                format!(
                    "the provider {} gave source {} the directory {path:?}, which {why}",
                    self.provider, self.index
                ),
                false,
            )
        };
        let fault = js_types::path::absolute(&path);
        let writes = self.mode == MountMode::ReadWrite;
        let refused = match (fault, roots.beneath(&path)) {
            (Err(fault), _) => Some(not_allowed(format!("is not an absolute path: {fault}"))),
            (Ok(()), None) => Some(not_allowed(
                "lies beneath no --mount-root or --mount-root-ro".to_owned(),
            )),
            (Ok(()), Some(beneath)) if writes && beneath.read_only => Some(not_allowed(format!(
                "lies beneath the root {}, which jobs may mount read-only only",
                beneath.name
            ))),
            (Ok(()), Some(_)) => None,
        };
        match refused {
            Some(failure) => Err(failure),
            None => {
                self.path = Some(path);
                Ok(())
            }
        }
    }

    /// Asks the provider for what this source still lacks, at `now_ms`: `Prepare` until it
    /// is taken, then `Mount` until the directory is answered. A provider that has not
    /// answered its sweep is not asked; nor is a source whose pace has it wait, nor (for a
    /// `Prepare` not out yet) one whose provider's gate is shut. A call that failed and now
    /// waits is given back, for the start's backoff line.
    fn ask(
        &mut self,
        run_id: &str,
        attached: &mut Attached,
        roots: &MountRoots,
        now_ms: u64,
    ) -> Result<Option<Backed>, Failure> {
        if !attached.swept(&self.provider) {
            self.why = format!(
                "the provider {} has not answered its sweep since the agent started",
                self.provider
            );
            return Ok(None);
        }
        if !self.pace.due(now_ms) {
            return Ok(None);
        }
        let seed = seed(&self.provider, run_id, self.index);
        if !self.prepared {
            if self.pace.idle()
                && let Some(why) = attached.gate_shut(&self.provider, now_ms)
            {
                why.clone_into(&mut self.why);
                return Ok(None);
            }
            let call = ProviderCall::Prepare {
                run_id: run_id.to_owned(),
                index: self.index,
                reference: self.reference.clone(),
                mode: self.mode,
            };
            let (answer, step) = attached.paced(
                (self.provider.as_str(), &call),
                &mut self.pace,
                now_ms,
                &seed,
            );
            match answer {
                ProviderAnswer::Done(_) => self.prepared = true,
                other => {
                    self.settle(other)?;
                    return Ok(step.map(|step| self.backed("Prepare", step)));
                }
            }
        }
        let call = ProviderCall::Mount {
            run_id: run_id.to_owned(),
            index: self.index,
        };
        let (answer, step) = attached.paced(
            (self.provider.as_str(), &call),
            &mut self.pace,
            now_ms,
            &seed,
        );
        match answer {
            ProviderAnswer::Done(Some(path)) => self.accept(path, roots).map(|()| None),
            ProviderAnswer::Done(None) => {
                self.why = "the provider answered no directory".to_owned();
                Ok(step.map(|step| self.backed("Mount", step)))
            }
            other => {
                self.settle(other)?;
                Ok(step.map(|step| self.backed("Mount", step)))
            }
        }
    }

    /// The call `call` of this source failed and now waits as `step` says.
    fn backed(&self, call: &'static str, step: Step) -> Backed {
        Backed {
            provider: self.provider.clone(),
            index: self.index,
            call,
            step,
            why: self.why.clone(),
        }
    }
}

/// What a start waits for from its providers: each source, and the moment the wait ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Providing {
    /// When the start has waited as long as `--mount-provider-wait` allows, on the clock.
    deadline_ms: u64,
    sources: Vec<Wanted>,
}

impl Providing {
    /// Asks for what each source lacks, and says whether every source has its directory now.
    ///
    /// # Errors
    ///
    /// A provider refused a source, one answered a directory the job may not be given, or
    /// the wait is over and a source still has none.
    fn poll(
        &mut self,
        run_id: &str,
        attached: &mut Attached,
        (roots, now_ms): (&MountRoots, u64),
        events: &mut Vec<Event>,
    ) -> Result<bool, Failure> {
        let mut backed = Vec::new();
        for source in self
            .sources
            .iter_mut()
            .filter(|source| source.path.is_none())
        {
            match source.ask(run_id, attached, roots, now_ms) {
                Ok(waits) => backed.extend(waits),
                Err(failure) => {
                    events.extend(attached.backoff(run_id, &backed));
                    return Err(failure);
                }
            }
        }
        events.extend(attached.backoff(run_id, &backed));
        match self.sources.iter().find(|source| source.path.is_none()) {
            None => Ok(true),
            Some(late) if now_ms >= self.deadline_ms => Err((
                refusal::INPUT_UNAVAILABLE,
                code::START_FAILED,
                format!(
                    "the provider {} did not mount source {} within {} s: {}",
                    late.provider,
                    late.index,
                    attached.wait_ms() / 1000,
                    late.why
                ),
                false,
            )),
            Some(_) => Ok(false),
        }
    }

    /// Replaces each source by the directory its provider answered, in place, so that the
    /// start's sources keep their order: a layer becomes a host path layer, a mount a mount of
    /// a host path.
    fn bind(&self, fs: &mut JobFs) {
        let paths: BTreeMap<u32, &str> = self
            .sources
            .iter()
            .filter_map(|source| source.path.as_deref().map(|path| (source.index, path)))
            .collect();
        for (index, layer) in (0u32..).zip(fs.layers.iter_mut()) {
            if let Some(path) = paths.get(&index) {
                *layer = Layer::HostPath {
                    path: (*path).to_owned(),
                };
            }
        }
        let first = u32::try_from(fs.layers.len()).unwrap_or(u32::MAX);
        for (index, mount) in (first..).zip(fs.mounts.iter_mut()) {
            if let Some(path) = paths.get(&index) {
                mount.source = MountSource::HostPath {
                    path: (*path).to_owned(),
                };
            }
        }
    }
}

/// What the providers' part of a waiting start comes to at a step.
pub(super) enum Follow {
    /// The start has no providers' part, or some source has no directory yet: the step goes
    /// on with the images and the lookups.
    Go,
    /// Every source has its directory now, bound into the start's file system, and the
    /// providers' part is dropped: the step goes on, and `provided` is marked.
    Bound,
    /// The start fails.
    Fail(Failure),
}

/// Follows the providers' part of `waiting`, if it has one: asks for what is lacking, and
/// when every source has its directory replaces the sources by them and drops the part.
pub(super) fn follow(
    waiting: &mut Waiting,
    (attached, roots): (&mut Attached, &MountRoots),
    now_ms: u64,
    events: &mut Vec<Event>,
) -> Follow {
    let Phase::Resolving { providing, .. } = &mut waiting.phase else {
        return Follow::Go;
    };
    let Some(part) = providing.as_mut() else {
        return Follow::Go;
    };
    match part.poll(&waiting.spec.run_id, attached, (roots, now_ms), events) {
        Err(failure) => Follow::Fail(failure),
        Ok(false) => Follow::Go,
        Ok(true) => {
            part.bind(&mut waiting.given.fs);
            *providing = None;
            Follow::Bound
        }
    }
}

impl Lifecycle {
    /// Begins the providers' part of a start admitted at `now_ms` whose file system `fs`
    /// holds sources a provider makes: remembers them for the release, asks each provider
    /// for what its source lacks, and then writes the record of them. `None` for a start
    /// with no such source.
    ///
    /// The asks go first: a provider begins making its tree in the background when it has
    /// `Prepare`, so the record's syncs (the file, its rename, the directory) are spent while
    /// it works and not before it begins. The record is still written in this call, before
    /// the start is held and before the worker steps again. A crash between a `Prepare` and
    /// the record leaves a source no record names, and the sweep each provider is asked when
    /// the agent starts ([`Lifecycle::attach_providers`]) releases it: it keeps only the runs
    /// the agent adopted, and a start that had no container is not one.
    ///
    /// # Errors
    ///
    /// A provider refused a source at once (the record is not written then; the sources are
    /// remembered, so the failed start's cleanup releases them), or the record could not be
    /// written (the providers were asked already, and the same cleanup releases what they
    /// were asked for).
    pub(super) fn provide(
        &mut self,
        run_id: &str,
        fs: &JobFs,
        now_ms: u64,
        events: &mut Vec<Event>,
    ) -> Result<Option<Providing>, Failure> {
        let provided = fs.providers();
        if provided.is_empty() {
            return Ok(None);
        }
        let sources: Vec<Source> = provided
            .iter()
            .map(|source| Source {
                provider: source.provider.to_owned(),
                index: source.index,
            })
            .collect();
        self.providers.keep(run_id, sources.clone());
        let deadline_ms = now_ms.saturating_add(self.providers.wait_ms());
        let mut providing = Providing {
            deadline_ms,
            sources: provided.iter().map(Wanted::of).collect(),
        };
        let at = (&self.config.mounts, now_ms);
        providing.poll(run_id, &mut self.providers, at, events)?;
        provider_record::write(&self.config.state, run_id, &sources).map_err(|why| {
            (
                refusal::INPUT_UNAVAILABLE,
                code::PROVIDER_RECORD_FAILED,
                format!("the record of the run's provider sources cannot be written: {why}"),
                false,
            )
        })?;
        Ok(Some(providing))
    }
}
