//! The platform a start names for its image, against the platform the image says it is
//! built for: [`ImagePlatform`] and [`check_platform`].
//!
//! A digest pins one platform's manifest, so a start that names its image also names the
//! `os` and `arch` of that manifest (`JobSpec::os`, `JobSpec::arch`). Whoever reads the
//! image's config (the agent, when it pulls) holds the two against what the config says
//! before it runs anything.

use js_types::{Arch, Os};

use crate::command::JobSpec;
use crate::error::{Rule, ValidationError};
use crate::refusal;

/// The platform an image's config says it is built for, in the closed set a start names it
/// by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePlatform {
    /// The operating system.
    pub os: Os,
    /// The CPU architecture.
    pub arch: Arch,
}

impl ImagePlatform {
    /// The platform of an image config's `os` and `architecture`, spelt as the OCI image
    /// specification spells them: `linux` or `darwin` (read as [`Os::Macos`]), and `amd64`
    /// (read as [`Arch::X86_64`]) or `arm64`. The names are compared exactly, with case.
    ///
    /// # Errors
    ///
    /// [`refusal::PLATFORM_MISMATCH`] naming `image.os` or `image.architecture` when either
    /// is outside that set: a job never runs on an image whose platform no start can name.
    pub fn from_oci(os: &str, architecture: &str) -> Result<Self, ValidationError> {
        let outside =
            |field| ValidationError::new(field, Rule::OutOfRange, refusal::PLATFORM_MISMATCH);
        let os = match os {
            "linux" => Os::Linux,
            "darwin" => Os::Macos,
            _ => return Err(outside("image.os")),
        };
        let arch = match architecture {
            "amd64" => Arch::X86_64,
            "arm64" => Arch::Arm64,
            _ => return Err(outside("image.architecture")),
        };
        Ok(Self { os, arch })
    }
}

/// Checks that the platform `job` names is the one its image is built for. A start that
/// names neither (one of the previous release) passes, and so does each one it leaves out;
/// one it names must be equal.
///
/// # Errors
///
/// [`refusal::PLATFORM_MISMATCH`] naming `os` or `arch` (`os` first) when the start names
/// another value than `image`.
pub fn check_platform(job: &JobSpec, image: &ImagePlatform) -> Result<(), ValidationError> {
    let clashes = [
        ("os", job.os.is_some_and(|os| os != image.os)),
        ("arch", job.arch.is_some_and(|arch| arch != image.arch)),
    ];
    match clashes.iter().find(|(_, clash)| *clash) {
        Some(&(field, _)) => Err(ValidationError::new(
            field,
            Rule::Inconsistent,
            refusal::PLATFORM_MISMATCH,
        )),
        None => Ok(()),
    }
}
