//! How the provider is allowed to spend. Every default is the safe one.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

use js_elastic::SchedulerId;

use crate::launch::ImageId;
use crate::model::{InstanceType, Region};

/// Whether launches are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A launch is built and logged and not sent.
    DryRun,
    /// A launch is sent, if the provider is also enabled.
    Live,
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No machine image is named.
    #[error("images: must name an image for arm64 or x86_64")]
    NoImage,
}

/// The machine image to launch for each architecture. An architecture with no image is
/// one the provider does not launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    arm64: Option<ImageId>,
    x86_64: Option<ImageId>,
}

impl Images {
    /// The images for `arm64` and `x86_64` machines.
    ///
    /// # Errors
    ///
    /// [`ConfigError::NoImage`] when both are `None`.
    pub fn new(arm64: Option<ImageId>, x86_64: Option<ImageId>) -> Result<Self, ConfigError> {
        match arm64.is_none() & x86_64.is_none() {
            true => Err(ConfigError::NoImage),
            false => Ok(Self { arm64, x86_64 }),
        }
    }

    /// The image for the architecture `arch`, as an offer spells it: `arm64` or `x86_64`.
    /// `None` for any other text, and for an architecture with no image.
    pub fn get(&self, arch: &str) -> Option<&ImageId> {
        match arch {
            "arm64" => self.arm64.as_ref(),
            "x86_64" => self.x86_64.as_ref(),
            _ => None,
        }
    }
}

/// The settings of a provider. Launches happen only when it is [enabled](Self::enable) and
/// [live](Self::live); it starts disabled and in dry-run mode.
#[derive(Debug, Clone)]
pub struct Ec2Config {
    pub(crate) scheduler: SchedulerId,
    pub(crate) region: Region,
    pub(crate) images: Images,
    pub(crate) max_machines: NonZeroUsize,
    pub(crate) enabled: bool,
    pub(crate) mode: Mode,
    pub(crate) unknown_hold: Duration,
    pub(crate) allowed: Option<BTreeSet<InstanceType>>,
}

impl Ec2Config {
    /// A configuration for the scheduler `scheduler`, which launches into `region` from
    /// `images` and never holds more than `max_machines` machines, counting the ones it
    /// cannot place and the launches it has no answer for. There is no default.
    pub fn new(
        scheduler: SchedulerId,
        region: Region,
        images: Images,
        max_machines: NonZeroUsize,
    ) -> Self {
        Self {
            scheduler,
            region,
            images,
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

    /// Sends launches. Without this a launch is built and logged and refused.
    #[must_use]
    pub fn live(mut self) -> Self {
        self.mode = Mode::Live;
        self
    }

    /// Sets how long a launch that got no answer counts toward `max_machines` while no
    /// list shows it (ten minutes by default). After that a list is trusted to show it.
    #[must_use]
    pub fn unknown_hold(mut self, hold: Duration) -> Self {
        self.unknown_hold = hold;
        self
    }

    /// Limits launches to these instance types (any type by default).
    #[must_use]
    pub fn allowed(mut self, types: BTreeSet<InstanceType>) -> Self {
        self.allowed = Some(types);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(text: &str) -> ImageId {
        ImageId::new(text).unwrap()
    }

    /// Asserts that a configuration starts disabled, in dry-run mode, holding an unknown
    /// launch for ten minutes and allowing every type, and that each setting changes only
    /// what it names.
    #[test]
    fn a_configuration_starts_safe_and_each_setting_changes_one_thing() {
        let images = Images::new(None, Some(image("ami-01234567"))).unwrap();
        let base = Ec2Config::new(
            SchedulerId::new("one").unwrap(),
            Region::new("region-a").unwrap(),
            images,
            NonZeroUsize::new(3).unwrap(),
        );
        assert!(!base.enabled);
        assert_eq!(base.mode, Mode::DryRun);
        assert_eq!(base.unknown_hold, Duration::from_secs(600));
        assert_eq!(base.allowed, None);
        assert_eq!(base.max_machines.get(), 3);
        assert_eq!(base.region.as_str(), "region-a");
        assert_eq!(base.scheduler.as_str(), "one");
        let enabled = base.clone().enable();
        assert!(enabled.enabled);
        assert_eq!(enabled.mode, Mode::DryRun);
        let live = base.clone().live();
        assert!(!live.enabled);
        assert_eq!(live.mode, Mode::Live);
        let held = base.clone().unknown_hold(Duration::from_secs(5));
        assert_eq!(held.unknown_hold, Duration::from_secs(5));
        let only = BTreeSet::from([InstanceType("m7.large".to_owned())]);
        let allowed = base.allowed(only.clone());
        assert_eq!(allowed.allowed, Some(only));
        assert_ne!(Mode::Live, Mode::DryRun);
    }
}
