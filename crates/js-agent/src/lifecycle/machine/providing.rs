//! The providers' part of a start (`docs/design/mount-providers.md`, section 4): a start
//! whose layers or mounts name a provider waits in [`Phase::Providing`], beside its images,
//! until every such source has the directory its provider made for it; then each source is
//! replaced by that directory as a host path and the start goes on to the lookups of its
//! host paths as any start does.
//!
//! - **At admission**, once the start has its slot, the record of its sources is written
//!   ([`provider_record`], so a restart can release what may exist), the sources are
//!   remembered for the release at the run's end, and each provider is asked to `Prepare`
//!   its source: before the start's images are asked for, so the provider fetches while the
//!   image is pulled and the container made.
//! - **At every step** each source that has no directory yet is asked again: `Prepare`
//!   until the provider has taken it, then `Mount` until it answers the directory. A
//!   provider that is unavailable is asked again; one that refuses the ref fails the start
//!   (`provider-refused`); a directory that is not an absolute path in normal form beneath a
//!   mount root of the agent's (a root it may write, for a read-write mount) fails it
//!   (`input-not-allowed`). A provider whose sweep at start is not answered yet is not asked,
//!   nor is any while the release of an earlier run of the same id is owed.
//! - **From the admission to `--mount-provider-wait`** a start may wait; past it the start
//!   fails `input-unavailable` naming the first source without a directory.
//! - **A stop** while providing ends the start at once, whatever a call in flight does: its
//!   sources are released at its cleanup like any run's (`provided`).

use std::collections::BTreeMap;

use js_agent_wire::{Layer, MountMode, MountSource, refusal};

use super::job::{Phase, Waiting};
use super::provided::Attached;
use super::provider_record::{self, Source};
use super::{Failure, Lifecycle};
use crate::lifecycle::runtime::JobFs;
use crate::lifecycle::{MountRoots, ProviderAnswer, ProviderCall, code};

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

    /// What an answer that is not a success makes of the source: nothing yet (the call is
    /// out, or the provider is not available and is asked again), or the failure of the
    /// start (the provider refused).
    fn settle(&mut self, answer: ProviderAnswer) -> Result<(), Failure> {
        match answer {
            ProviderAnswer::Pending | ProviderAnswer::Done(_) => Ok(()),
            ProviderAnswer::Refused(why) => Err(self.refused(&why)),
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

    /// Asks the provider for what this source still lacks: `Prepare` until it is taken, then
    /// `Mount` until the directory is answered. A provider that has not answered its sweep
    /// is not asked.
    fn ask(
        &mut self,
        run_id: &str,
        attached: &mut Attached,
        roots: &MountRoots,
    ) -> Result<(), Failure> {
        if !attached.swept(&self.provider) {
            self.why = format!(
                "the provider {} has not answered its sweep since the agent started",
                self.provider
            );
            return Ok(());
        }
        // A run of the same id that ended on this host (a job queued again and placed here
        // again) may still owe its release: one that came after this `Prepare` would release
        // the new source, which has the same agent, run and index.
        if attached.owes(run_id) {
            self.why = "the release of the run's earlier sources is not answered".to_owned();
            return Ok(());
        }
        if !self.prepared {
            let call = ProviderCall::Prepare {
                run_id: run_id.to_owned(),
                index: self.index,
                reference: self.reference.clone(),
                mode: self.mode,
            };
            match attached.ask(&self.provider, &call) {
                ProviderAnswer::Done(_) => self.prepared = true,
                other => return self.settle(other),
            }
        }
        let call = ProviderCall::Mount {
            run_id: run_id.to_owned(),
            index: self.index,
        };
        match attached.ask(&self.provider, &call) {
            ProviderAnswer::Done(Some(path)) => self.accept(path, roots),
            ProviderAnswer::Done(None) => {
                self.why = "the provider answered no directory".to_owned();
                Ok(())
            }
            other => self.settle(other),
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
        roots: &MountRoots,
        now_ms: u64,
    ) -> Result<bool, Failure> {
        for source in self
            .sources
            .iter_mut()
            .filter(|source| source.path.is_none())
        {
            source.ask(run_id, attached, roots)?;
        }
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
    /// The start has no providers' part (or it is done: the start is in the lookups of its
    /// host paths now): the step goes on as any start's.
    Go,
    /// Some source has no directory yet.
    Wait,
    /// The start fails.
    Fail(Failure),
}

/// Follows the providers' part of `waiting`, if it has one: asks for what is lacking, and
/// when every source has its directory replaces the sources by them and moves the start on
/// to the lookups of its host paths.
pub(super) fn follow(
    waiting: &mut Waiting,
    (attached, roots): (&mut Attached, &MountRoots),
    now_ms: u64,
) -> Follow {
    let Phase::Providing(providing) = &mut waiting.phase else {
        return Follow::Go;
    };
    match providing.poll(&waiting.spec.run_id, attached, roots, now_ms) {
        Err(failure) => Follow::Fail(failure),
        Ok(false) => Follow::Wait,
        Ok(true) => {
            providing.bind(&mut waiting.given.fs);
            waiting.phase = Phase::Resolving;
            Follow::Go
        }
    }
}

impl Lifecycle {
    /// Begins the providers' part of a start admitted at `now_ms` whose file system `fs`
    /// holds sources a provider makes: writes the record of them, remembers them for the
    /// release, and asks each provider for what its source lacks. `None` for a start with no
    /// such source.
    ///
    /// # Errors
    ///
    /// The record could not be written (nothing was asked of any provider), or a provider
    /// refused a source at once.
    pub(super) fn provide(
        &mut self,
        run_id: &str,
        fs: &JobFs,
        now_ms: u64,
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
        provider_record::write(&self.config.state, run_id, &sources).map_err(|why| {
            (
                refusal::INPUT_UNAVAILABLE,
                code::PROVIDER_RECORD_FAILED,
                format!("the record of the run's provider sources cannot be written: {why}"),
                false,
            )
        })?;
        self.providers.keep(run_id, sources);
        let deadline_ms = now_ms.saturating_add(self.providers.wait_ms());
        let mut providing = Providing {
            deadline_ms,
            sources: provided.iter().map(Wanted::of).collect(),
        };
        providing.poll(run_id, &mut self.providers, &self.config.mounts, now_ms)?;
        Ok(Some(providing))
    }
}
