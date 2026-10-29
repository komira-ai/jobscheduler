//! What a job is, said once: the container it runs and what it needs of a host.
//!
//! A job never names a cloud, an image of a machine or an instance type; those live in
//! the pool that serves it. [`ContainerSpec`] is the container, [`Requirements`] is the
//! host it needs. A lease request still carries resources and capabilities only, so
//! [`Requirements::from_request`] reads `os` and `arch` from the capabilities the request
//! requires and leaves disk and execution mode unset.

use crate::{Request, Resources};

/// The capability key a request names its operating system by.
const OS_KEY: &str = "os";
/// The capability key a request names its CPU architecture by.
const ARCH_KEY: &str = "arch";
/// The only digest algorithm an image reference may name.
const DIGEST_PREFIX: &str = "sha256:";
/// Hex digits in a sha256 digest.
const DIGEST_HEX_LEN: usize = 64;

/// How a host runs a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecMode {
    /// In a container, through the host's container runtime.
    Container,
    /// Natively, under a `sandbox-exec` profile (macOS).
    SandboxExec,
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
pub struct ImageDigestRef {
    name: String,
    digest: String,
}

impl ImageDigestRef {
    /// The reference `text`.
    ///
    /// # Errors
    ///
    /// [`ImageRefError`] naming what is missing or malformed.
    pub fn parse(text: &str) -> Result<Self, ImageRefError> {
        let (name, digest) = text.split_once('@').ok_or(ImageRefError::NoDigest)?;
        let hex = digest.strip_prefix(DIGEST_PREFIX).unwrap_or_default();
        let digest_ok = [
            hex.len() == DIGEST_HEX_LEN,
            hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        ];
        if !digest_ok.into_iter().all(|ok| ok) {
            return Err(ImageRefError::BadDigest);
        }
        let name_ok = [!name.is_empty(), name.bytes().all(|b| b.is_ascii_graphic())];
        if !name_ok.into_iter().all(|ok| ok) {
            return Err(ImageRefError::BadName);
        }
        Ok(Self {
            name: name.to_owned(),
            digest: digest.to_owned(),
        })
    }

    /// The name, with its tag if it has one.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The digest, as `sha256:<hex>`.
    pub fn digest(&self) -> &str {
        &self.digest
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
    /// The CPU architecture (`x86_64`, `aarch64`). `None`: any.
    pub arch: Option<String>,
    /// The mode the host runs jobs in. `None`: either.
    pub exec: Option<ExecMode>,
}

impl Requirements {
    /// The requirements of a lease request: its resources, and `os` and `arch` from the
    /// capabilities it requires. Disk and execution mode are `None`: a request cannot
    /// carry them yet.
    pub fn from_request(request: &Request) -> Self {
        Self {
            resources: request.resources.clone(),
            disk_bytes: None,
            os: request.capabilities.get(OS_KEY).cloned(),
            arch: request.capabilities.get(ARCH_KEY).cloned(),
            exec: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::*;
    use crate::{Capabilities, ClassId};

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
        }
    }

    /// Asserts that `os` and `arch` come from the capabilities a request requires, that
    /// the resources are the request's, and that disk and mode stay unset: the request
    /// cannot carry them.
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
        assert_eq!(arch_only.arch.as_deref(), Some("aarch64"));
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
}
