//! Container platforms: what runs a job's OCI image (design section 3.5).
//!
//! A pool names container platforms in priority order. [`ContainerPlatform`] is the trait
//! every platform kind implements. The `default` kind is the only one built: it owns an
//! ordered list of machine providers (`js_elastic::CapacityProvider`), keeps the host table
//! and books each job's allocation on a host of ours. Hostless kinds (Kubernetes, Cloud
//! Run, ECS) would hand the image to a service that runs it; none is built.
//!
//! The trait has three required calls, which every platform answers from its own code and
//! records: [`ContainerPlatform::id`], [`ContainerPlatform::capabilities`] and
//! [`ContainerPlatform::can_ever_serve`]. The five calls of a job's life
//! ([`ContainerPlatform::submit`], [`ContainerPlatform::observe`],
//! [`ContainerPlatform::cancel`], [`ContainerPlatform::logs`] and
//! [`ContainerPlatform::result`]) are declared here and meaningful only for hostless kinds:
//! for `default` the job engine books the allocation, so `default` does not override them,
//! and each answers [`PlatformError::NotBuilt`] unless a kind overrides it.
//!
//! The design sketches the five as `async fn`. They are written as functions that return a
//! `Send` future, which is the same thing for an implementor (an `async fn` in an impl
//! matches them) and lets a caller hold the future across threads.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeSet;
use std::fmt;
use std::future::{Future, ready};
use std::time::Duration;

use js_types::{Arch, ImageDigestRef, JobNetwork, Model, Os, Requirements};

/// The name of a container platform: a `[[platform]]` table's `name`, or a name the
/// configuration loader made for a pool that names its provider directly.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlatformId(String);

impl PlatformId {
    /// The platform called `name`. The loader checks the grammar of a name the operator
    /// wrote; this takes any text.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PlatformId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlatformId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// What a platform could ever run, in the vocabulary of design section 3.8 as far as it
/// exists today: operating systems, architectures, accelerator models, and whether a job
/// may have a host whole. A job that needs something outside these is never served by
/// the platform, however long it waits.
///
/// Host ports and network levels are not here yet: the capability vocabulary has no term
/// for them, and the step that adds them there (design 3.8) adds them here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PlatformCaps {
    /// The operating systems.
    pub os: BTreeSet<Os>,
    /// The CPU architectures, in the one spelling.
    pub arch: BTreeSet<Arch>,
    /// The accelerator models, as the agent spells a GPU class (`nvidia-a100-sxm4-80gb`) or
    /// as a canonical family (`nvidia-a100`); both are compared by their family.
    pub accelerators: BTreeSet<String>,
    /// Whether a job may have one of the platform's hosts whole.
    pub whole_host: bool,
}

impl PlatformCaps {
    /// Capabilities of `os`, `arch` and `accelerators`, with a whole host when
    /// `whole_host`.
    pub fn new(
        os: BTreeSet<Os>,
        arch: BTreeSet<Arch>,
        accelerators: BTreeSet<String>,
        whole_host: bool,
    ) -> Self {
        Self {
            os,
            arch,
            accelerators,
            whole_host,
        }
    }

    /// Whether these capabilities admit `r`: the operating system and the architecture it
    /// names, when it names one, are in the sets; and a request for GPUs finds the family of
    /// its model (its `accelerator.model`, else its GPU class) among the families of the
    /// accelerators, through the alias table, so a request for one variant is admitted by a
    /// platform that lists another of the family or the family itself. A name
    /// that is not an operating system or an architecture is in no set. Sizes are not
    /// compared: capabilities say what kind of host exists, not how big.
    pub fn admits(&self, r: &Requirements) -> bool {
        let os =
            r.os.as_deref()
                .is_none_or(|os| os.parse().is_ok_and(|os: Os| self.os.contains(&os)));
        let arch = r.arch.as_deref().is_none_or(|arch| {
            arch.parse()
                .is_ok_and(|arch: Arch| self.arch.contains(&arch))
        });
        let model = r
            .accelerator
            .as_ref()
            .and_then(|need| need.model.as_ref())
            .map(Model::as_str)
            .or(r.resources.gpu_class());
        let gpus = model.is_none_or(|want| {
            let want = family(want);
            self.accelerators.iter().any(|own| family(own) == want)
        });
        [os, arch, gpus].into_iter().all(|met| met)
    }

    /// These capabilities with `other`'s added: the union of the sets, and a whole host
    /// when either offers one.
    #[must_use]
    pub fn union(mut self, other: &Self) -> Self {
        self.os.extend(other.os.iter().copied());
        self.arch.extend(other.arch.iter().copied());
        self.accelerators.extend(other.accelerators.iter().cloned());
        self.whole_host |= other.whole_host;
        self
    }
}

/// A job handed to a hostless platform: its image by digest, what it needs, its network
/// level, its wall limit and the id that makes a repeated submit a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubmitSpec {
    /// The id of the submit: a second submit with it starts nothing new.
    pub submit_id: String,
    /// The image, pinned by digest.
    pub image: ImageDigestRef,
    /// What the job needs of where it runs.
    pub requirements: Requirements,
    /// How much network the job has.
    pub network: JobNetwork,
    /// The longest the job may run.
    pub wall_limit: Duration,
}

impl SubmitSpec {
    /// The submit `submit_id` of `image`, needing `requirements`, with `network` and
    /// `wall_limit`.
    pub fn new(
        submit_id: impl Into<String>,
        image: ImageDigestRef,
        requirements: Requirements,
        network: JobNetwork,
        wall_limit: Duration,
    ) -> Self {
        Self {
            submit_id: submit_id.into(),
            image,
            requirements,
            network,
            wall_limit,
        }
    }
}

/// The platform's own reference to a job it took: what the job record keeps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobHandle(String);

impl JobHandle {
    /// The handle the platform wrote as `text`.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The handle as the platform wrote it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A platform took a submit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubmitAck {
    /// The platform's reference to the job.
    pub handle: JobHandle,
}

impl SubmitAck {
    /// The platform took the job as `handle`.
    pub fn new(handle: JobHandle) -> Self {
        Self { handle }
    }
}

/// What a platform says of a job it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobState {
    /// Taken, not running yet.
    Pending,
    /// Running.
    Running,
    /// Ended by itself, successfully.
    Succeeded,
    /// Ended by itself, unsuccessfully, or ended by the platform.
    Failed,
    /// The platform said it has no such job: an answer, given only after an explicit
    /// not-found.
    Absent,
    /// No answer: a failed, throttled or timed-out read. Never read as `Absent`.
    Unknown,
}

/// What a platform answered to a cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelAck {
    /// The platform took the cancel.
    Cancelled,
    /// No answer; the cancel may or may not have landed.
    Unknown,
}

/// The output a platform kept of a job, as bytes in the order it wrote them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogStream(Vec<u8>);

impl LogStream {
    /// The output `bytes`.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// How a job a platform ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobResult {
    /// The exit code of the job's process.
    pub exit_code: i32,
}

impl JobResult {
    /// A job whose process exited with `exit_code`.
    pub fn new(exit_code: i32) -> Self {
        Self { exit_code }
    }
}

/// Why a platform call has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlatformError {
    /// The platform kind does not do this: the call is declared for hostless kinds, and
    /// none is built.
    #[error("not built: this platform kind does not take jobs by this call")]
    NotBuilt,
}

/// A container platform: what runs a job's OCI image (design section 3.5).
///
/// [`ContainerPlatform::id`], [`ContainerPlatform::capabilities`] and
/// [`ContainerPlatform::can_ever_serve`] are required and answer from the platform's own
/// code and records, making no call. The other five are declared here and meaningful only
/// for hostless kinds; for `default` the job engine books the allocation, so `default`
/// does not override them, and each answers [`PlatformError::NotBuilt`] unless overridden.
pub trait ContainerPlatform {
    /// The platform's name.
    fn id(&self) -> PlatformId;

    /// What the platform could ever run.
    fn capabilities(&self) -> PlatformCaps;

    /// Whether the platform could ever serve a job that needs `r`, now or after any wait:
    /// admission refuses a job no platform of its pool could ever serve.
    fn can_ever_serve(&self, r: &Requirements) -> bool;

    /// Hands the job `s` to the platform.
    ///
    /// # Errors
    ///
    /// [`PlatformError::NotBuilt`] unless the kind overrides it.
    fn submit(
        &self,
        s: SubmitSpec,
    ) -> impl Future<Output = Result<SubmitAck, PlatformError>> + Send {
        drop(s);
        ready(Err(PlatformError::NotBuilt))
    }

    /// Reads the state of the job `h`.
    ///
    /// # Errors
    ///
    /// [`PlatformError::NotBuilt`] unless the kind overrides it.
    fn observe(
        &self,
        h: &JobHandle,
    ) -> impl Future<Output = Result<JobState, PlatformError>> + Send {
        let _ = h;
        ready(Err(PlatformError::NotBuilt))
    }

    /// Cancels the job `h`.
    ///
    /// # Errors
    ///
    /// [`PlatformError::NotBuilt`] unless the kind overrides it.
    fn cancel(
        &self,
        h: &JobHandle,
    ) -> impl Future<Output = Result<CancelAck, PlatformError>> + Send {
        let _ = h;
        ready(Err(PlatformError::NotBuilt))
    }

    /// The output the platform kept of the job `h`.
    ///
    /// # Errors
    ///
    /// [`PlatformError::NotBuilt`] unless the kind overrides it.
    fn logs(&self, h: &JobHandle) -> impl Future<Output = Result<LogStream, PlatformError>> + Send {
        let _ = h;
        ready(Err(PlatformError::NotBuilt))
    }

    /// How the job `h` ended, once it has; `None` before.
    ///
    /// # Errors
    ///
    /// [`PlatformError::NotBuilt`] unless the kind overrides it.
    fn result(
        &self,
        h: &JobHandle,
    ) -> impl Future<Output = Result<Option<JobResult>, PlatformError>> + Send {
        let _ = h;
        ready(Err(PlatformError::NotBuilt))
    }
}

/// The canonical family of a model name through the alias table, or the text itself when
/// it is not a model name.
fn family(text: &str) -> String {
    Model::parse(text).map_or_else(
        |_| text.to_owned(),
        |model| model.canonical().model.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use js_types::{AcceleratorNeed, Resources};

    use super::*;

    fn wanting(
        os: Option<&str>,
        arch: Option<&str>,
        gpus: u64,
        class: Option<&str>,
    ) -> Requirements {
        Requirements {
            resources: Resources::new(1000, 1 << 30, gpus, class.map(str::to_owned)).unwrap(),
            disk_bytes: None,
            os: os.map(str::to_owned),
            arch: arch.map(str::to_owned),
            isa_level: None,
            exec: None,
            accelerator: None,
        }
    }

    fn linux_a10() -> PlatformCaps {
        PlatformCaps::new(
            BTreeSet::from([Os::Linux]),
            BTreeSet::from([Arch::X86_64]),
            BTreeSet::from(["nvidia-a10".to_owned()]),
            false,
        )
    }

    /// Asserts that capabilities admit a request only when each named part is inside them:
    /// no os, arch or GPU named admits anything; a named os, arch and GPU class must each be
    /// in the sets; and a name that is no os or arch is admitted by none. It catches an
    /// admission that skips one of the three checks.
    #[test]
    fn capabilities_admit_only_what_is_inside_them() {
        let caps = linux_a10();
        assert!(caps.admits(&wanting(None, None, 0, None)));
        assert!(caps.admits(&wanting(
            Some("linux"),
            Some("x86_64"),
            1,
            Some("nvidia-a10")
        )));
        assert!(caps.admits(&wanting(None, None, 2, Some("nvidia-a10"))));
        assert!(!caps.admits(&wanting(Some("macos"), None, 0, None)));
        assert!(!caps.admits(&wanting(Some("plan9"), None, 0, None)));
        assert!(!caps.admits(&wanting(None, Some("arm64"), 0, None)));
        assert!(!caps.admits(&wanting(None, Some("sparc"), 0, None)));
        assert!(!caps.admits(&wanting(None, None, 1, Some("nvidia-h100"))));
        assert!(!PlatformCaps::default().admits(&wanting(None, None, 1, Some("nvidia-a10"))));
    }

    /// Asserts that GPU models are compared by their family through the alias table: a
    /// request for a variant is admitted by a platform that lists another variant of its
    /// family or the family itself, a request for another family is not, the model of the
    /// request's accelerator need is used before its booked class, and a class that is not
    /// a model name is compared as written. Catches a platform refused for a request that
    /// spells its model differently from the platform's listing.
    #[test]
    fn gpu_models_are_compared_by_their_family() {
        let a100 = |listed: &str| {
            PlatformCaps::new(
                BTreeSet::from([Os::Linux]),
                BTreeSet::from([Arch::X86_64]),
                BTreeSet::from([listed.to_owned()]),
                false,
            )
        };
        let variant = wanting(None, None, 1, Some("nvidia-a100-sxm4-40gb"));
        assert!(a100("nvidia-a100-sxm4-80gb").admits(&variant));
        assert!(a100("nvidia-a100").admits(&variant));
        assert!(!a100("nvidia-h100").admits(&variant));
        let mut by_need = wanting(None, None, 0, None);
        by_need.accelerator = Some(AcceleratorNeed {
            model: Some(Model::parse("nvidia-a100").unwrap()),
            ..AcceleratorNeed::default()
        });
        assert!(a100("nvidia-a100-sxm4-80gb").admits(&by_need));
        assert!(!a100("nvidia-h100").admits(&by_need));
        let odd = wanting(None, None, 1, Some("A100_X"));
        assert!(a100("A100_X").admits(&odd));
        assert!(!a100("A100_Y").admits(&odd));
    }

    /// Asserts that a union holds every set member of both sides and a whole host when
    /// either has one, and that the union with nothing changes nothing.
    #[test]
    fn a_union_holds_both_sides() {
        let mac = PlatformCaps::new(
            BTreeSet::from([Os::Macos]),
            BTreeSet::from([Arch::Arm64]),
            BTreeSet::new(),
            true,
        );
        let both = linux_a10().union(&mac);
        assert_eq!(both.os, BTreeSet::from([Os::Linux, Os::Macos]));
        assert_eq!(both.arch, BTreeSet::from([Arch::X86_64, Arch::Arm64]));
        assert_eq!(both.accelerators, linux_a10().accelerators);
        assert!(both.whole_host);
        assert_eq!(linux_a10().union(&PlatformCaps::default()), linux_a10());
    }

    /// Asserts that the ids, handles, acks, results and log streams carry what they were
    /// made with, and that the error says it is not built.
    #[test]
    fn the_values_carry_what_they_were_made_with() {
        let id = PlatformId::new("ours");
        assert_eq!(id.as_str(), "ours");
        assert_eq!(id.to_string(), "ours");
        assert_eq!(id.as_ref(), "ours");
        let handle = JobHandle::new("job-1");
        assert_eq!(handle.as_str(), "job-1");
        assert_eq!(SubmitAck::new(handle.clone()).handle, handle);
        assert_eq!(JobResult::new(3).exit_code, 3);
        assert_eq!(LogStream::new(b"out".to_vec()).bytes(), b"out");
        assert_eq!(
            PlatformError::NotBuilt.to_string(),
            "not built: this platform kind does not take jobs by this call"
        );
        let image =
            ImageDigestRef::parse(&format!("team/app:2@sha256:{}", "0".repeat(64))).unwrap();
        let spec = SubmitSpec::new(
            "s-1",
            image.clone(),
            wanting(None, None, 0, None),
            JobNetwork::None,
            Duration::from_secs(60),
        );
        assert_eq!(spec.submit_id, "s-1");
        assert_eq!(spec.image, image);
        assert_eq!(spec.network, JobNetwork::None);
        assert_eq!(spec.wall_limit, Duration::from_secs(60));
        assert_eq!(spec.requirements, wanting(None, None, 0, None));
    }
}
