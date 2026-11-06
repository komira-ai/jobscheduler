//! How the provider is allowed to spend. Every default is the safe one.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

use js_elastic::SchedulerId;

use crate::launch::{ImagePath, NetworkInterface};
use crate::model::{MachineTypeName, Project};

/// Whether inserts are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// An insert is built and logged and not sent.
    DryRun,
    /// An insert is sent, if the provider is also enabled.
    Live,
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No machine image is named.
    #[error("images: must name an image for arm64 or x86_64")]
    NoImage,
}

/// The machine image to boot for each architecture. An architecture with no image is one
/// the provider does not launch: an acquire of it is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    arm64: Option<ImagePath>,
    x86_64: Option<ImagePath>,
}

impl Images {
    /// The images for `arm64` and `x86_64` machines.
    ///
    /// # Errors
    ///
    /// [`ConfigError::NoImage`] when both are `None`.
    pub fn new(arm64: Option<ImagePath>, x86_64: Option<ImagePath>) -> Result<Self, ConfigError> {
        match arm64.is_none() & x86_64.is_none() {
            true => Err(ConfigError::NoImage),
            false => Ok(Self { arm64, x86_64 }),
        }
    }

    /// The image for the architecture `arch`, as an offer spells it: `arm64` or `x86_64`.
    /// `None` for any other text, and for an architecture with no image.
    pub fn get(&self, arch: &str) -> Option<&ImagePath> {
        match arch {
            "arm64" => self.arm64.as_ref(),
            "x86_64" => self.x86_64.as_ref(),
            _ => None,
        }
    }
}

/// The settings of a provider. Inserts happen only when it is [enabled](Self::enable) and
/// [live](Self::live); it starts disabled and in dry-run mode.
#[derive(Debug, Clone)]
pub struct GceConfig {
    pub(crate) scheduler: SchedulerId,
    pub(crate) project: Project,
    pub(crate) images: Images,
    pub(crate) network: NetworkInterface,
    pub(crate) max_machines: NonZeroUsize,
    pub(crate) enabled: bool,
    pub(crate) mode: Mode,
    pub(crate) unknown_hold: Duration,
    pub(crate) allowed: Option<BTreeSet<MachineTypeName>>,
}

impl GceConfig {
    /// A configuration for the scheduler `scheduler`, which inserts into `project` from
    /// `images`, attaches every instance to `network`, and never holds more than
    /// `max_machines` machines, counting the ones it cannot place and the inserts it has no
    /// answer for. There is no default.
    pub fn new(
        scheduler: SchedulerId,
        project: Project,
        images: Images,
        network: NetworkInterface,
        max_machines: NonZeroUsize,
    ) -> Self {
        Self {
            scheduler,
            project,
            images,
            network,
            max_machines,
            enabled: false,
            mode: Mode::DryRun,
            unknown_hold: Duration::from_secs(600),
            allowed: None,
        }
    }

    /// Enables the provider. Without this every acquire is refused.
    #[must_use]
    pub fn enable(mut self) -> Self {
        self.enabled = true;
        self
    }

    /// Sends inserts. Without this an insert is built and logged and refused.
    #[must_use]
    pub fn live(mut self) -> Self {
        self.mode = Mode::Live;
        self
    }

    /// Sets how long an insert that got no answer counts toward `max_machines` while no
    /// list shows it (ten minutes by default). After that a list is trusted to show it.
    #[must_use]
    pub fn unknown_hold(mut self, hold: Duration) -> Self {
        self.unknown_hold = hold;
        self
    }

    /// Limits launches to these machine types (any type by default).
    #[must_use]
    pub fn allowed(mut self, types: BTreeSet<MachineTypeName>) -> Self {
        self.allowed = Some(types);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(text: &str) -> ImagePath {
        ImagePath::new(text).unwrap()
    }

    /// Asserts that a configuration starts disabled, in dry-run mode, holding an unknown
    /// insert for ten minutes and allowing every type, and that each setting changes only
    /// what it names; and that the images answer the architecture each is for and nothing
    /// for another.
    #[test]
    fn a_configuration_starts_safe_and_each_setting_changes_one_thing() {
        let x86 = image("projects/image-project/global/images/js-agent-x86");
        let images = Images::new(None, Some(x86.clone())).unwrap();
        assert_eq!(images.get("x86_64"), Some(&x86));
        assert_eq!(images.get("arm64"), None);
        assert_eq!(images.get("aarch64"), None);
        assert_eq!(Images::new(None, None), Err(ConfigError::NoImage));
        let base = GceConfig::new(
            SchedulerId::new("one").unwrap(),
            Project::new("sample-project").unwrap(),
            images,
            NetworkInterface {
                network: "global/networks/default".to_owned(),
                subnetwork: "regions/region-one/subnetworks/default".to_owned(),
            },
            NonZeroUsize::new(3).unwrap(),
        );
        assert!(!base.enabled);
        assert_eq!(base.mode, Mode::DryRun);
        assert_eq!(base.unknown_hold, Duration::from_secs(600));
        assert_eq!(base.allowed, None);
        assert_eq!(base.max_machines.get(), 3);
        assert_eq!(base.project.as_str(), "sample-project");
        assert_eq!(base.scheduler.as_str(), "one");
        let enabled = base.clone().enable();
        assert!(enabled.enabled);
        assert_eq!(enabled.mode, Mode::DryRun);
        let live = base.clone().live();
        assert!(!live.enabled);
        assert_eq!(live.mode, Mode::Live);
        let held = base.clone().unknown_hold(Duration::from_secs(5));
        assert_eq!(held.unknown_hold, Duration::from_secs(5));
        let only = BTreeSet::from([MachineTypeName("n2-standard-2".to_owned())]);
        let allowed = base.allowed(only.clone());
        assert_eq!(allowed.allowed, Some(only));
        assert_ne!(Mode::Live, Mode::DryRun);
    }
}
