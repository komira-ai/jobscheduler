//! What a job is, said once: the container it runs and what it needs of a host.
//!
//! A job never names a cloud, an image of a machine or an instance type; those live in
//! the pool that serves it. [`ContainerSpec`] is the container, [`Requirements`] is the
//! host it needs. A lease request still carries resources and capabilities only, so
//! [`Requirements::from_request`] reads `os`, `arch`, `isa_level`, `disk-gib`, `exec` and
//! the `accelerator.*` keys from the capabilities the request requires.

use std::fmt;
use std::str::FromStr;

use crate::digest::{DigestRef, DigestRefError};
use crate::{AccelError, AcceleratorNeed, IsaLevel, Request, Resources, normalise_arch};

/// The capability key a request names its operating system by.
const OS_KEY: &str = "os";
/// The capability key a request names its CPU architecture by.
pub(crate) const ARCH_KEY: &str = "arch";
/// The capability key a request names its least ISA level by (`x86-64-v3`, `armv8.2-a`).
pub(crate) const ISA_LEVEL_KEY: &str = "isa_level";
/// The capability key a request names its least free disk by, in whole GiB. A host
/// reports the free space of its work volume under it.
pub const DISK_GIB_KEY: &str = "disk-gib";
/// The capability key a request names the mode its host runs jobs in by. A host reports
/// the mode it runs in under it.
pub const EXEC_KEY: &str = "exec";
/// Bytes in a GiB.
const GIB: u64 = 1 << 30;

/// How a host runs a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecMode {
    /// In a container, through the host's container runtime.
    Container,
    /// Natively, under a `sandbox-exec` profile (macOS).
    SandboxExec,
}

impl ExecMode {
    /// The text that names the mode in a pool's `exec` and in a hello.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

impl fmt::Display for ExecMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that names no execution mode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown execution mode {0:?}; known: container, sandbox-exec")]
pub struct UnknownExecMode(pub String);

impl FromStr for ExecMode {
    type Err = UnknownExecMode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "container" => Ok(Self::Container),
            "sandbox-exec" => Ok(Self::SandboxExec),
            _ => Err(UnknownExecMode(s.to_owned())),
        }
    }
}

/// Why text is not an image reference with a digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageRefError {
    /// There is no `@` and so no digest.
    #[error("an image reference needs a digest: name@sha256:<64 hex digits>")]
    NoDigest,
    /// The digest is not `sha256:` followed by 64 lower case hex digits.
    #[error("the digest must be sha256: followed by 64 lower case hex digits")]
    BadDigest,
    /// The name before the `@` is empty or holds a character that is not printable ASCII
    /// without spaces.
    #[error("the image name must be non-empty printable ASCII without spaces")]
    BadName,
}

/// An image reference that pins its content: `name[:tag]@sha256:<hex>`. The type holds
/// the digest, so a value of it cannot name a tag that moves. The full grammar of the
/// name (registry, path, tag) is checked where a run is started, not here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageDigestRef(DigestRef);

impl ImageDigestRef {
    /// The reference `text`.
    ///
    /// # Errors
    ///
    /// [`ImageRefError`] naming what is missing or malformed.
    pub fn parse(text: &str) -> Result<Self, ImageRefError> {
        match DigestRef::parse(text) {
            Ok(core) => Ok(Self(core)),
            Err(DigestRefError::NoDigest) => Err(ImageRefError::NoDigest),
            Err(DigestRefError::BadDigest) => Err(ImageRefError::BadDigest),
            Err(DigestRefError::BadName) => Err(ImageRefError::BadName),
        }
    }

    /// The name, with its tag if it has one.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// The digest, as `sha256:<hex>`.
    pub fn digest(&self) -> &str {
        self.0.digest()
    }
}

/// The container a job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    /// The image, pinned by digest.
    pub image: ImageDigestRef,
    /// The command and its arguments.
    pub command: Vec<String>,
}

/// What a job needs of the host it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirements {
    /// CPU, memory, GPUs and the GPU class.
    pub resources: Resources,
    /// The least disk, in bytes. `None`: not asked for.
    pub disk_bytes: Option<u64>,
    /// The operating system (`linux`, `macos`). `None`: any.
    pub os: Option<String>,
    /// The CPU architecture, canonical (`x86_64`, `arm64`; a request's `aarch64` is read
    /// as `arm64`, and a name no release knows is kept as sent). `None`: any.
    pub arch: Option<String>,
    /// The least ISA level, met by a level of the same family at least as high. `None`: not
    /// asked for.
    pub isa_level: Option<IsaLevel>,
    /// The mode the host runs jobs in. `None`: either.
    pub exec: Option<ExecMode>,
    /// The accelerators the job needs, as the request's `accelerator.*` keys state them, or
    /// else as its booked GPU count and class (`model` the class as spelt, `count` the
    /// count). `None`: none asked for.
    pub accelerator: Option<AcceleratorNeed>,
}

/// Why a request's `disk-gib`, `exec`, `isa_level` or `accelerator.*` capabilities are not a
/// requirement.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequirementsError {
    /// `disk-gib` is not a whole number of GiB that fits in bytes.
    #[error("disk-gib must be a whole number of GiB below 2^34, not {0:?}")]
    BadDisk(String),
    /// `exec` names no execution mode.
    #[error("exec must be container or sandbox-exec, not {0:?}")]
    BadExec(String),
    /// `isa_level` names no ISA level.
    #[error("isa_level must be x86-64-v1 to x86-64-v4 or armv8.0-a to armv8.4-a, not {0:?}")]
    BadIsaLevel(String),
    /// An `accelerator.*` key holds a value that is not valid.
    #[error(transparent)]
    BadAccelerator(#[from] AccelError),
    /// `accelerator.count` asks for devices without `accelerator.model`. A booked GPU
    /// count needs a class (a lease is stored with both), so a count names its model.
    #[error("accelerator.count above zero needs accelerator.model")]
    CountWithoutModel,
}

impl Requirements {
    /// The requirements of a lease request: its resources, and `os`, `arch` (through
    /// [`normalise_arch`]), `isa_level`, `disk-gib`, `exec` and the accelerator from the
    /// capabilities it requires. A capability the request does not name is `None`: any. A
    /// `disk-gib`, `exec`, `isa_level` or `accelerator.*` key that is not valid makes all
    /// four read as not asked here, the accelerator then as the booked GPU count and class;
    /// [`Request::validate`] refuses such a request, so none that was admitted has one
    /// ([`Self::try_from_request`] says why).
    pub fn from_request(request: &Request) -> Self {
        Self::try_from_request(request).unwrap_or_else(|_| Self {
            resources: request.resources.clone(),
            disk_bytes: None,
            os: request.capabilities.get(OS_KEY).cloned(),
            arch: arch_of(request),
            isa_level: None,
            exec: None,
            accelerator: AcceleratorNeed::booked(&request.resources),
        })
    }

    /// [`Self::from_request`], refusing a `disk-gib`, `exec`, `isa_level` or
    /// `accelerator.*` key that is not valid.
    ///
    /// # Errors
    ///
    /// [`RequirementsError::BadDisk`] when `disk-gib` is not a whole number of GiB whose
    /// size in bytes fits 64 bits; [`RequirementsError::BadExec`] when `exec` is not a
    /// mode name; [`RequirementsError::BadIsaLevel`] when `isa_level` is not a level
    /// name; [`RequirementsError::BadAccelerator`] when an `accelerator.*` key is not
    /// valid; [`RequirementsError::CountWithoutModel`] when `accelerator.count` is above
    /// zero and `accelerator.model` is absent. `disk-gib=0` is valid and asks for nothing.
    pub fn try_from_request(request: &Request) -> Result<Self, RequirementsError> {
        let caps = &request.capabilities;
        let disk_bytes = caps
            .get(DISK_GIB_KEY)
            .map(|text| {
                let bad = || RequirementsError::BadDisk(text.clone());
                let gib: u64 = text.parse().map_err(|_| bad())?;
                gib.checked_mul(GIB).ok_or_else(bad)
            })
            .transpose()?;
        let exec = caps
            .get(EXEC_KEY)
            .map(|text| {
                text.parse::<ExecMode>()
                    .map_err(|_| RequirementsError::BadExec(text.clone()))
            })
            .transpose()?;
        let isa_level = caps
            .get(ISA_LEVEL_KEY)
            .map(|text| {
                text.parse::<IsaLevel>()
                    .map_err(|_| RequirementsError::BadIsaLevel(text.clone()))
            })
            .transpose()?;
        Ok(Self {
            resources: request.resources.clone(),
            disk_bytes,
            os: caps.get(OS_KEY).cloned(),
            arch: arch_of(request),
            isa_level,
            exec,
            accelerator: need_of(request)?,
        })
    }
}

/// The accelerator need of `request`: its `accelerator.*` keys when it names any, else
/// its booked GPU count and class.
fn need_of(request: &Request) -> Result<Option<AcceleratorNeed>, RequirementsError> {
    match AcceleratorNeed::from_capabilities(&request.capabilities)? {
        Some(need) if need.count > 0 && need.model.is_none() => {
            Err(RequirementsError::CountWithoutModel)
        }
        Some(need) => Ok(Some(need)),
        None => Ok(AcceleratorNeed::booked(&request.resources)),
    }
}

/// The `arch` a request requires, in the one spelling.
fn arch_of(request: &Request) -> Option<String> {
    let arch = request.capabilities.get(ARCH_KEY)?;
    Some(normalise_arch(arch).to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::*;
    use crate::{Capabilities, ClassId, Model};

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn request(caps: &[(&str, &str)]) -> Request {
        let capabilities: Capabilities = caps
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Request {
            class: ClassId::new("whole-node").unwrap(),
            resources: Resources::new(4000, 8, 1, Some("nvidia-a10".into())).unwrap(),
            capabilities,
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        }
    }

    /// The need the test request's booked GPU states: one `nvidia-a10`.
    fn a10() -> Option<AcceleratorNeed> {
        Some(AcceleratorNeed {
            model: Some(Model::parse("nvidia-a10").unwrap()),
            count: 1,
            ..AcceleratorNeed::default()
        })
    }

    /// Asserts that both modes parse from and print as the names a pool and a hello use,
    /// and that any other text is refused naming what is known.
    #[test]
    fn modes_round_trip_and_unknown_is_refused() {
        for mode in [ExecMode::Container, ExecMode::SandboxExec] {
            assert_eq!(mode.to_string().parse::<ExecMode>(), Ok(mode));
        }
        let err = "Container".parse::<ExecMode>().unwrap_err();
        assert_eq!(err, UnknownExecMode("Container".to_owned()));
        assert!(err.to_string().contains("known: container, sandbox-exec"));
    }

    /// Asserts that `os` and `arch` come from the capabilities a request requires, that
    /// the resources are the request's, and that a request that names no disk or mode
    /// leaves both unset.
    #[test]
    fn requirements_read_os_and_arch_from_the_request() {
        let wanted = Requirements::from_request(&request(&[
            ("os", "linux"),
            ("arch", "x86_64"),
            ("driver", "550"),
        ]));
        assert_eq!(wanted.os.as_deref(), Some("linux"));
        assert_eq!(wanted.arch.as_deref(), Some("x86_64"));
        assert_eq!(wanted.resources.gpus(), 1);
        assert_eq!(wanted.resources.gpu_class(), Some("nvidia-a10"));
        assert_eq!((wanted.disk_bytes, wanted.exec), (None, None));
    }

    /// Asserts that a request that names neither leaves both unset (any host), and that
    /// each is read on its own.
    #[test]
    fn a_request_without_platform_capabilities_asks_for_any_platform() {
        let none = Requirements::from_request(&request(&[("driver", "550")]));
        assert_eq!((none.os, none.arch), (None, None));
        let os_only = Requirements::from_request(&request(&[("os", "macos")]));
        assert_eq!(os_only.os.as_deref(), Some("macos"));
        assert_eq!(os_only.arch, None);
        let arch_only = Requirements::from_request(&request(&[("arch", "aarch64")]));
        assert_eq!(arch_only.os, None);
        assert_eq!(arch_only.arch.as_deref(), Some("arm64"));
        assert_eq!(arch_only.isa_level, None);
    }

    /// Asserts that a request's `arch` is held in the one spelling: `aarch64` becomes
    /// `arm64` through both readers, `arm64` and `x86_64` stay, and a name no release
    /// knows is kept as sent rather than refused or turned into another. Catches a
    /// requirement that keeps `aarch64`, which `Offer::fit` compares by equality.
    #[test]
    fn the_requested_arch_is_normalised() {
        for (sent, held) in [
            ("aarch64", "arm64"),
            ("arm64", "arm64"),
            ("x86_64", "x86_64"),
            ("sparc", "sparc"),
        ] {
            let caps = [("arch", sent)];
            let lenient = Requirements::from_request(&request(&caps));
            let strict = Requirements::try_from_request(&request(&caps)).unwrap();
            assert_eq!(lenient.arch.as_deref(), Some(held), "{sent}");
            assert_eq!(strict.arch.as_deref(), Some(held), "{sent}");
        }
        let refused = request(&[("arch", "aarch64"), (EXEC_KEY, "native")]);
        let lenient = Requirements::from_request(&refused);
        assert_eq!(lenient.arch.as_deref(), Some("arm64"));
    }

    /// Asserts that the capabilities a request of the previous release names (none, `os`
    /// and `arch` as `x86_64` or `arm64`, `disk-gib`, `exec`) read as the requirements they
    /// did, every field compared, with no ISA level. Catches a new field that a request
    /// which does not name it fails, or reads as asking for something.
    #[test]
    fn previous_request_bodies_parse_unchanged() {
        let resources = request(&[]).resources;
        let read = |caps: &[(&str, &str)]| Requirements::try_from_request(&request(caps));
        let none = Requirements {
            resources,
            disk_bytes: None,
            os: None,
            arch: None,
            isa_level: None,
            exec: None,
            accelerator: a10(),
        };
        assert_eq!(read(&[]), Ok(none.clone()));
        for arch in ["x86_64", "arm64"] {
            let want = Requirements {
                os: Some("linux".to_owned()),
                arch: Some(arch.to_owned()),
                ..none.clone()
            };
            assert_eq!(read(&[("os", "linux"), ("arch", arch)]), Ok(want));
        }
        let want = Requirements {
            disk_bytes: Some(40 << 30),
            exec: Some(ExecMode::Container),
            ..none.clone()
        };
        assert_eq!(
            read(&[(DISK_GIB_KEY, "40"), (EXEC_KEY, "container")]),
            Ok(want)
        );
    }

    /// Asserts that one bad field among `disk-gib`, `exec` and `isa_level` is refused by
    /// name while the others are good, and that the lenient read then keeps the resources,
    /// the operating system and the architecture (in the one spelling) and reads all three
    /// of disk, mode and level as not asked, as its doc says. Catches a fallback that keeps
    /// a valid level beside a bad disk, or loses the platform.
    #[test]
    fn one_bad_field_clears_disk_mode_and_level_together() {
        let platform = [("os", "linux"), ("arch", "aarch64")];
        let cases = [
            (
                [
                    (DISK_GIB_KEY, "lots"),
                    (EXEC_KEY, "container"),
                    (ISA_LEVEL_KEY, "x86-64-v3"),
                ],
                RequirementsError::BadDisk("lots".to_owned()),
            ),
            (
                [
                    (DISK_GIB_KEY, "40"),
                    (EXEC_KEY, "native"),
                    (ISA_LEVEL_KEY, "x86-64-v3"),
                ],
                RequirementsError::BadExec("native".to_owned()),
            ),
            (
                [
                    (DISK_GIB_KEY, "40"),
                    (EXEC_KEY, "container"),
                    (ISA_LEVEL_KEY, "x86-64-v9"),
                ],
                RequirementsError::BadIsaLevel("x86-64-v9".to_owned()),
            ),
        ];
        for (fields, refusal) in cases {
            let caps: Vec<(&str, &str)> = platform.iter().chain(&fields).copied().collect();
            let asked = request(&caps);
            assert_eq!(Requirements::try_from_request(&asked), Err(refusal.clone()));
            let lenient = Requirements::from_request(&asked);
            let want = Requirements {
                resources: asked.resources.clone(),
                disk_bytes: None,
                os: Some("linux".to_owned()),
                arch: Some("arm64".to_owned()),
                isa_level: None,
                exec: None,
                accelerator: a10(),
            };
            assert_eq!(lenient, want, "{refusal}");
        }
    }

    /// Asserts that `isa_level` is read as a level of either family, that a request
    /// without it asks for none, and that a name that is not a level is refused naming
    /// the text, and read as not asked by `from_request`. Catches a requirement that drops
    /// the level, or that admits a level no host can state.
    #[test]
    fn the_isa_level_comes_from_the_request() {
        use crate::{ArmVersion, X86Level};
        let x86 = Requirements::from_request(&request(&[(ISA_LEVEL_KEY, "x86-64-v3")]));
        assert_eq!(x86.isa_level, Some(IsaLevel::X86_64(X86Level::V3)));
        let arm = Requirements::try_from_request(&request(&[(ISA_LEVEL_KEY, "armv8.2-a")]));
        assert_eq!(
            arm.map(|r| r.isa_level),
            Ok(Some(IsaLevel::Arm64(ArmVersion::V8_2)))
        );
        let none = Requirements::try_from_request(&request(&[("os", "linux")])).unwrap();
        assert_eq!(none.isa_level, None);
        for bad in ["x86-64-v9", "", "armv8.2", "X86-64-V3"] {
            let err = Requirements::try_from_request(&request(&[(ISA_LEVEL_KEY, bad)]));
            assert_eq!(err, Err(RequirementsError::BadIsaLevel(bad.to_owned())));
            let lenient = Requirements::from_request(&request(&[(ISA_LEVEL_KEY, bad)]));
            assert_eq!(lenient.isa_level, None, "{bad:?}");
        }
        assert_eq!(
            RequirementsError::BadIsaLevel("x86-64-v9".into()).to_string(),
            "isa_level must be x86-64-v1 to x86-64-v4 or armv8.0-a to armv8.4-a, not \"x86-64-v9\""
        );
    }

    /// Asserts that `disk-gib` becomes bytes and `exec` a mode, each on its own, that an
    /// exact edge of the byte range is accepted, and that `disk-gib=0` is a requirement
    /// of nothing rather than an absence.
    #[test]
    fn disk_and_mode_come_from_the_request() {
        let both = Requirements::from_request(&request(&[
            (DISK_GIB_KEY, "50"),
            (EXEC_KEY, "sandbox-exec"),
        ]));
        assert_eq!(both.disk_bytes, Some(50 << 30));
        assert_eq!(both.exec, Some(ExecMode::SandboxExec));
        let disk = Requirements::from_request(&request(&[(DISK_GIB_KEY, "1")]));
        assert_eq!((disk.disk_bytes, disk.exec), (Some(1 << 30), None));
        let mode = Requirements::from_request(&request(&[(EXEC_KEY, "container")]));
        assert_eq!(
            (mode.disk_bytes, mode.exec),
            (None, Some(ExecMode::Container))
        );
        let zero = Requirements::from_request(&request(&[(DISK_GIB_KEY, "0")]));
        assert_eq!(zero.disk_bytes, Some(0));
        let edge = (u64::MAX >> 30).to_string();
        let top = Requirements::try_from_request(&request(&[(DISK_GIB_KEY, &edge)])).unwrap();
        assert_eq!(top.disk_bytes, Some((u64::MAX >> 30) << 30));
    }

    /// Asserts that a disk that is not a whole number, is negative, or overflows bytes,
    /// and a mode that is not named, are refused with an error that names the value, and
    /// that `from_request` then reads them as not asked while keeping the platform.
    #[test]
    fn a_bad_disk_or_mode_is_refused() {
        let over = ((u64::MAX >> 30) + 1).to_string();
        for text in ["", "-1", "1.5", "ten", over.as_str()] {
            let err = Requirements::try_from_request(&request(&[(DISK_GIB_KEY, text)]));
            assert_eq!(err, Err(RequirementsError::BadDisk(text.to_owned())));
        }
        for text in ["", "Container", "native"] {
            let err = Requirements::try_from_request(&request(&[(EXEC_KEY, text)]));
            assert_eq!(err, Err(RequirementsError::BadExec(text.to_owned())));
        }
        let said = RequirementsError::BadExec("x".into()).to_string();
        assert!(said.contains("container or sandbox-exec"), "{said}");
        let said = RequirementsError::BadDisk("x".into()).to_string();
        assert!(said.contains("disk-gib"), "{said}");
        let lenient = Requirements::from_request(&request(&[
            (DISK_GIB_KEY, "ten"),
            (EXEC_KEY, "native"),
            ("os", "macos"),
        ]));
        assert_eq!((lenient.disk_bytes, lenient.exec), (None, None));
        assert_eq!(lenient.os.as_deref(), Some("macos"));
    }

    /// Asserts that a reference with a digest parses, keeps the name (tag included) and
    /// the digest apart, and builds a container spec.
    #[test]
    fn an_image_with_a_digest_parses() {
        let image =
            ImageDigestRef::parse(&format!("registry.example/team/app:1.2@sha256:{HEX}")).unwrap();
        assert_eq!(image.name(), "registry.example/team/app:1.2");
        assert_eq!(image.digest(), format!("sha256:{HEX}"));
        let spec = ContainerSpec {
            image: image.clone(),
            command: vec!["run".into(), "--fast".into()],
        };
        assert_eq!(spec.image, image);
        assert_eq!(spec.command.len(), 2);
    }

    /// Asserts that each way a reference can fail to pin its content is refused with its
    /// own error: no digest, a digest of another algorithm, of the wrong length or with
    /// a character that is not lower case hex, and a bad name.
    #[test]
    fn an_image_that_does_not_pin_its_content_is_refused() {
        let parse = ImageDigestRef::parse;
        assert_eq!(parse("app:latest"), Err(ImageRefError::NoDigest));
        assert_eq!(
            parse(&format!("app@sha512:{HEX}")),
            Err(ImageRefError::BadDigest)
        );
        assert_eq!(parse("app@sha256:abcd"), Err(ImageRefError::BadDigest));
        let upper = HEX.to_uppercase();
        assert_eq!(
            parse(&format!("app@sha256:{upper}")),
            Err(ImageRefError::BadDigest)
        );
        assert_eq!(
            parse(&format!("@sha256:{HEX}")),
            Err(ImageRefError::BadName)
        );
        assert_eq!(
            parse(&format!("my app@sha256:{HEX}")),
            Err(ImageRefError::BadName)
        );
        assert!(ImageRefError::NoDigest.to_string().contains("digest"));
    }

    /// Asserts that the `accelerator.*` keys are read as the need (memory in GiB, the model
    /// as spelt, every field), that a vendor-only need books nothing and is valid, and that
    /// a request with no keys reads its booked GPU count and class as the need. Catches a
    /// requirement that drops a key, or one that loses today's GPU class when no key is
    /// named.
    #[test]
    fn the_accelerator_comes_from_its_keys_or_the_booked_gpus() {
        use crate::{FormFactor, Interconnect, Vendor};
        let read = |caps: &[(&str, &str)]| Requirements::try_from_request(&request(caps));
        let full = read(&[
            ("accelerator.vendor", "nvidia"),
            ("accelerator.model", "nvidia-a100-sxm4-80gb"),
            ("accelerator.count", "4"),
            ("accelerator.memory-gib", "80"),
            ("accelerator.interconnect", "nvlink-domain>=72"),
            ("accelerator.form-factor", "sxm"),
        ])
        .unwrap();
        let want = AcceleratorNeed {
            vendor: Some(Vendor::parse("nvidia").unwrap()),
            model: Some(Model::parse("nvidia-a100-sxm4-80gb").unwrap()),
            count: 4,
            memory_per_device: Some(80 << 30),
            interconnect: Some(Interconnect::new("nvlink-domain", 72).unwrap()),
            form_factor: Some(FormFactor::Sxm),
        };
        assert_eq!(full.accelerator, Some(want));
        let vendor_only = read(&[("accelerator.vendor", "nvidia")]).unwrap();
        let want = AcceleratorNeed {
            vendor: Some(Vendor::parse("nvidia").unwrap()),
            ..AcceleratorNeed::default()
        };
        assert_eq!(vendor_only.accelerator, Some(want));
        assert_eq!(read(&[]).unwrap().accelerator, a10());
        let mut cpu_only = request(&[]);
        cpu_only.resources = Resources::new(1000, 1, 0, None).unwrap();
        assert_eq!(Requirements::from_request(&cpu_only).accelerator, None);
        let mut odd_class = request(&[]);
        odd_class.resources = Resources::new(1000, 1, 2, Some("A100_X".into())).unwrap();
        assert_eq!(
            Requirements::from_request(&odd_class).accelerator,
            None,
            "a class that is not a model name is matched by the class alone"
        );
    }

    /// Asserts that a count without a model, and each key holding a value that is not
    /// valid, is refused naming what is wrong, and that the lenient read then falls back to
    /// the booked GPU count and class. Catches a count-only need reaching a lease, which
    /// would be stored as GPUs of no class.
    #[test]
    fn a_count_without_a_model_or_a_bad_key_is_refused() {
        let read = |caps: &[(&str, &str)]| Requirements::try_from_request(&request(caps));
        let count_only = [("accelerator.count", "4")];
        assert_eq!(read(&count_only), Err(RequirementsError::CountWithoutModel));
        assert_eq!(
            RequirementsError::CountWithoutModel.to_string(),
            "accelerator.count above zero needs accelerator.model"
        );
        assert!(read(&[("accelerator.count", "0")]).is_ok());
        let bad = [
            (
                "accelerator.vendor",
                "NVIDIA",
                AccelError::BadVendor("NVIDIA".into()),
            ),
            (
                "accelerator.model",
                "a--b",
                AccelError::BadModel("a--b".into()),
            ),
            (
                "accelerator.count",
                "four",
                AccelError::BadNumber {
                    key: "accelerator.count",
                    value: "four".into(),
                },
            ),
            (
                "accelerator.memory-gib",
                "-1",
                AccelError::BadNumber {
                    key: "accelerator.memory-gib",
                    value: "-1".into(),
                },
            ),
            (
                "accelerator.interconnect",
                "nvlink",
                AccelError::BadInterconnect("nvlink".into()),
            ),
            (
                "accelerator.form-factor",
                "oam",
                AccelError::BadFormFactor("oam".into()),
            ),
        ];
        for (key, value, why) in bad {
            assert_eq!(
                read(&[(key, value)]),
                Err(RequirementsError::BadAccelerator(why)),
                "{key}"
            );
            let lenient = Requirements::from_request(&request(&[(key, value)]));
            assert_eq!(lenient.accelerator, a10(), "{key}");
        }
        let lenient = Requirements::from_request(&request(&count_only));
        assert_eq!(lenient.accelerator, a10());
    }
}
