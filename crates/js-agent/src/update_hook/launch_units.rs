//! The runner of `launchctl bootout` for [`LaunchUnits`] (js-update-hook's
//! [`Units`](super::Units) over `launchctl`).
//!
//! `launchctl bootout` returns when the service has exited (launchd waits out `ExitTimeOut`
//! before it kills the process; the agent's plist sets 120 s, the default is 20 s), so unlike
//! `systemctl stop --no-block` the call blocks for as long as the agent takes to exit. The
//! default runner kills a helper after [`PROBE_LIMIT`](crate::PROBE_LIMIT) (10 s): the
//! stop is given a runner of its own ([`LaunchUnits::with_stop_runner`]), and [`StopRunner`]
//! builds its limit from the exit timeout it is made for, so it outlasts it by construction.

use std::time::Duration;

pub use js_update_hook::{LAUNCHCTL, LaunchUnits, SETTLE_READS};

use crate::facts::{CommandRunner, PROBE_OUTPUT_CAP, SystemCommands};

/// How much longer than the daemon's `ExitTimeOut` the runner of `bootout` waits.
pub const BOOTOUT_MARGIN: Duration = Duration::from_secs(30);

/// The runner of `launchctl bootout`: a [`SystemCommands`] whose limit is made from the
/// `ExitTimeOut` of the daemon being stopped, so that it outlasts it by construction.
pub struct StopRunner {
    runner: SystemCommands,
    limit: Duration,
}

impl StopRunner {
    /// A runner for the daemon whose plist says `exit_timeout`.
    #[must_use]
    pub fn new(exit_timeout: Duration) -> Self {
        let limit = exit_timeout + BOOTOUT_MARGIN;
        Self {
            runner: SystemCommands::bounded(limit, PROBE_OUTPUT_CAP),
            limit,
        }
    }

    /// How long it waits for the output of a call, and again for its exit.
    #[must_use]
    pub fn limit(&self) -> Duration {
        self.limit
    }
}

impl CommandRunner for StopRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.runner.run(program, args)
    }

    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        self.runner.run_in(program, args, env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::SystemCommands;

    /// The runner that `bootout` goes through is the one made with the limit the daemon's exit
    /// timeout asks for: read from the command runner inside, not from the field the stop runner
    /// keeps for `limit()`, so a runner built with another limit is seen whatever it reports.
    #[test]
    fn the_command_runner_of_a_stop_has_the_exit_timeout_plus_the_margin_as_its_limit() {
        for secs in [0, 20, 120] {
            let exit = Duration::from_secs(secs);
            let stop = StopRunner::new(exit);
            let wanted = SystemCommands::bounded(exit + BOOTOUT_MARGIN, PROBE_OUTPUT_CAP);
            assert_eq!(
                format!("{:?}", stop.runner),
                format!("{wanted:?}"),
                "{secs}s"
            );
            assert_eq!(stop.limit(), exit + BOOTOUT_MARGIN, "{secs}s");
        }
        let probe = SystemCommands::bounded(crate::PROBE_LIMIT, PROBE_OUTPUT_CAP);
        let stop = StopRunner::new(Duration::from_secs(120));
        assert_ne!(format!("{:?}", stop.runner), format!("{probe:?}"));
    }
}
