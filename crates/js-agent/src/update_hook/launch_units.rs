//! [`Units`] over `launchctl`, for a Mac. Not written yet: every call fails.

use std::marker::PhantomData;

use crate::facts::CommandRunner;

use super::ports::{PortError, UnitState, Units};

/// The launchd client, by the absolute path it has on a stock system.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// [`Units`] over `launchctl`.
pub struct LaunchUnits<'a>(PhantomData<&'a dyn CommandRunner>);

impl<'a> LaunchUnits<'a> {
    /// Runs `launchctl` through `runner`.
    #[must_use]
    pub fn new(_runner: &'a dyn CommandRunner) -> Self {
        Self(PhantomData)
    }
}

fn unwritten() -> PortError {
    PortError::Command("the launchctl port is not written".to_owned())
}

impl Units for LaunchUnits<'_> {
    fn show(&self, _unit: &str) -> Result<UnitState, PortError> {
        Err(unwritten())
    }

    fn stop_no_block(&self, _unit: &str) -> Result<(), PortError> {
        Err(unwritten())
    }

    fn start(&self, _unit: &str) -> Result<(), PortError> {
        Err(unwritten())
    }

    fn is_active(&self, _unit: &str) -> Result<bool, PortError> {
        Ok(false)
    }
}
