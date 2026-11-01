//! [`Units`] over `launchctl`, for a Mac: the unit is the label of a LaunchDaemon in the system
//! domain (`com.komira.js-agent`), and its plist is `<label>.plist` in `/Library/LaunchDaemons`,
//! where `js-mac-provision` puts it.
//!
//! | port call        | `launchctl`                                                         |
//! |------------------|---------------------------------------------------------------------|
//! | `show`           | `print system/<label>`, its `state` line                            |
//! | `stop_no_block`  | `bootout system/<label>`, when `print` finds the service loaded      |
//! | `start`          | `bootstrap system <plist>` when nothing is loaded; `kickstart -k` when loaded and not running |
//! | `is_active`      | `print system/<label>`: loaded and `state = running`                |
//!
//! A stopped daemon is booted out, not only killed: the daemon's `KeepAlive` would start it
//! again at once, and a loaded service the hook did not stop would also be one the hook could
//! not tell from a stopped one. Booted out is what a stopped unit looks like here: `show` says
//! `inactive` for a label launchd does not hold, as `systemctl show` does for a unit it has not
//! run. `start` then loads the plist again, and launchd starts the daemon at once if its
//! `KeepAlive` condition holds (the package's gate file exists); if not, the unit stays
//! `inactive`, as a systemd unit whose `ConditionPathExists` fails does.
//!
//! `launchctl bootout` returns when the service has exited (launchd waits out `ExitTimeOut`,
//! 20 s by default, before it kills the process), so unlike `systemctl stop --no-block` the call
//! blocks for as long as the agent takes to exit. The runner the port is given must allow that.
//!
//! The program is run by its absolute path with no shell, and the label is checked before any
//! command: it becomes part of a path and of an argument.

use std::path::Path;
use std::time::Duration;

use crate::facts::{CommandRunner, PROBE_OUTPUT_CAP, SystemCommands};

use super::ports::{Clock, PortError, SystemClock, UnitState, Units};

/// The launchd client, by the absolute path it has on a stock system.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// Where launchd reads the system domain's daemons from.
const LAUNCH_DAEMONS_DIR: &str = "/Library/LaunchDaemons";

/// How `launchctl print` ends for a service the domain does not hold ("Could not find service").
const NOT_FOUND: &str = "exit status: 113";

/// How many times `start` reads the service again to see it begin, at most.
pub const SETTLE_READS: u32 = 10;

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
        let limit = exit_timeout;
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

/// [`Units`] over `launchctl`.
pub struct LaunchUnits<'a> {
    runner: &'a dyn CommandRunner,
    #[allow(dead_code)]
    stopper: Option<&'a dyn CommandRunner>,
    #[allow(dead_code)]
    clock: &'a dyn Clock,
    #[allow(dead_code)]
    gate: Option<&'a Path>,
}

impl<'a> LaunchUnits<'a> {
    /// Runs `launchctl` through `runner`.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self {
            runner,
            stopper: None,
            clock: &SystemClock,
            gate: None,
        }
    }

    /// Runs `bootout` through `stopper` instead.
    #[must_use]
    pub fn with_stop_runner(mut self, stopper: &'a dyn CommandRunner) -> Self {
        self.stopper = Some(stopper);
        self
    }

    /// Waits for a started daemon with `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: &'a dyn Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The file whose existence lets launchd run the daemon.
    #[must_use]
    pub fn with_gate(mut self, gate: &'a Path) -> Self {
        self.gate = Some(gate);
        self
    }

    fn launchctl(&self, args: &[&str]) -> Result<String, String> {
        self.runner.run(LAUNCHCTL, args)
    }

    /// What `launchctl print` printed of the service, `None` if the domain does not hold it.
    fn print(&self, label: &str) -> Result<Option<String>, PortError> {
        let target = format!("system/{label}");
        match self.launchctl(&["print", &target]) {
            Ok(text) => Ok(Some(text)),
            Err(why) if why.ends_with(NOT_FOUND) => Ok(None),
            Err(why) => Err(PortError::Command(why)),
        }
    }
}

/// A label that is a plain file name: it is joined to the directory of the plists.
fn check_label(label: &str) -> Result<(), PortError> {
    if label.is_empty() || label.contains(['/', '\0']) {
        return Err(PortError::Command(format!(
            "{label:?} is not a launchd label"
        )));
    }
    Ok(())
}

/// The `state` of the service itself in `launchctl print` output: the property one tab in. The
/// endpoints and event triggers below it have `state` lines of their own, deeper in.
fn printed_state(text: &str) -> Result<&str, PortError> {
    text.lines()
        .filter_map(|line| line.strip_prefix('\t'))
        .filter(|line| !line.starts_with(char::is_whitespace))
        .find_map(|line| line.strip_prefix("state = "))
        .map(str::trim)
        .ok_or_else(|| PortError::Parse("launchctl print printed no state".to_owned()))
}

/// What the steps know of a unit, from the state launchd prints.
fn unit_state(state: &str) -> Result<UnitState, PortError> {
    let (active, sub) = match state {
        "running" => ("active", "running"),
        "not running" | "waiting" | "exited" => ("inactive", "dead"),
        // Between two starts of a `KeepAlive` daemon, the throttle running.
        "spawn scheduled" => ("activating", "auto-restart"),
        "spawning" => ("activating", "start"),
        other => {
            return Err(PortError::Parse(format!(
                "launchctl print printed the unknown state {other:?}"
            )));
        }
    };
    Ok(UnitState {
        active: active.to_owned(),
        sub: sub.to_owned(),
    })
}

impl Units for LaunchUnits<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        check_label(unit)?;
        match self.print(unit)? {
            Some(text) => unit_state(printed_state(&text)?),
            None => unit_state("not running"),
        }
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        check_label(unit)?;
        // `bootout` of a label that is not loaded fails, and a stop of a stopped unit is not an
        // error: look first.
        if self.print(unit)?.is_none() {
            return Ok(());
        }
        self.launchctl(&["bootout", &format!("system/{unit}")])
            .map(drop)
            .map_err(PortError::Command)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        check_label(unit)?;
        let result = match self.print(unit)? {
            None => {
                let plist = format!("{LAUNCH_DAEMONS_DIR}/{unit}.plist");
                self.launchctl(&["bootstrap", "system", &plist])
            }
            Some(text) if printed_state(&text)? == "running" => return Ok(()),
            Some(_) => self.launchctl(&["kickstart", "-k", &format!("system/{unit}")]),
        };
        result.map(drop).map_err(PortError::Command)
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        if check_label(unit).is_err() {
            return Ok(false);
        }
        Ok(self
            .print(unit)
            .ok()
            .flatten()
            .is_some_and(|text| printed_state(&text) == Ok("running")))
    }
}
