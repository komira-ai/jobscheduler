//! [`Units`] over `launchctl`, for a Mac: the unit is the label of a LaunchDaemon in the system
//! domain (`com.komira.js-agent`), and its plist is `<label>.plist` in `/Library/LaunchDaemons`,
//! where `js-mac-provision` puts it.
//!
//! | port call        | `launchctl`                                                         |
//! |------------------|---------------------------------------------------------------------|
//! | `show`           | `print system/<label>`: its `state`, `last exit code` and `last terminating signal` |
//! | `stop_no_block`  | `bootout system/<label>`, when `print` finds the service loaded      |
//! | `start`          | `bootstrap system <plist>` when nothing is loaded; `kickstart -k` when loaded and not running; then reads `print` until the service has begun |
//! | `is_active`      | `print system/<label>`: loaded and `state = running`                |
//!
//! # What `show` says
//!
//! The steps read a unit as systemd words (`active`, `inactive`, `failed`, `activating`) and
//! decide from [`UnitState::crashed`] whether a start that went wrong is the release's fault:
//! the binary ran and died. launchd has no `failed` and says "not running" for a daemon that
//! crashed and sits between two throttle windows as for one that never started, so the port
//! reads how the service last ended:
//!
//! | `state`                                  | last ended                       | `show`                    |
//! |------------------------------------------|----------------------------------|---------------------------|
//! | `running`                                | (any)                            | `active`, `running`       |
//! | `not running`, `exited`, `waiting`       | non-zero exit code or a signal   | `failed`, `failed`        |
//! | `not running`, `exited`, `waiting`       | exit 0, never, or not printed    | `inactive`, `dead`        |
//! | `spawn scheduled`                        | exit 0                           | `activating`, `start`     |
//! | `spawn scheduled`                        | anything else                    | `activating`, `auto-restart` |
//! | `spawning`                               | (any)                            | `activating`, `start`     |
//! | not loaded (never loaded, or booted out) | -                                | `inactive`, `dead`        |
//!
//! So a Mac agent that crashes ends `failed` (the release's fault) as it does on Linux, and the
//! failures that are not the program's end `inactive`: a plist launchd cannot load, or a
//! `bootstrap` that errors, leaves the label unloaded; a daemon whose `KeepAlive` gate file is
//! absent is loaded and has never run. The exit of an earlier run is ignored while the service
//! runs, and a clean exit (the agent exits 0 after it has drained) is not a crash.
//!
//! # Stopping and starting
//!
//! A stopped daemon is booted out, not only killed: the daemon's `KeepAlive` would start it
//! again at once, and a loaded service the hook did not stop would also be one the hook could
//! not tell from a stopped one. Booted out is what a stopped unit looks like here: `show` says
//! `inactive` for a label launchd does not hold, as `systemctl show` does for a unit it has not
//! run. `start` then loads the plist again, and launchd starts the daemon at once if its
//! `KeepAlive` condition holds (the package's gate file exists); if not, the unit stays
//! `inactive`, as a systemd unit whose `ConditionPathExists` fails does.
//!
//! `launchctl bootout` returns when the service has exited (launchd waits out `ExitTimeOut`
//! before it kills the process; the agent's plist sets 120 s, the default is 20 s), so unlike
//! `systemctl stop --no-block` the call blocks for as long as the agent takes to exit. The
//! default runner kills a helper after [`PROBE_LIMIT`](crate::PROBE_LIMIT) (10 s): the
//! stop is given a runner of its own ([`LaunchUnits::with_stop_runner`]), and [`StopRunner`]
//! builds its limit from the exit timeout it is made for, so it outlasts it by construction.
//!
//! `bootstrap` and `kickstart` return before the program has begun: `launchctl print` can say
//! "not running" for a moment after either, where systemd says `activating`. `start` therefore
//! reads the service again, at most [`SETTLE_READS`] times with a pause between the reads, until
//! it is anything but "not running" with the `runs` it had before the start, and then returns;
//! it does not wait for the program to be healthy (the steps do). A service that is still idle
//! after the reads is left to the steps to judge: it reads `inactive`, or `failed` if it has
//! ended badly.
//!
//! # `kickstart -k` and the token file
//!
//! `kickstart` runs the service "regardless of its configured launch conditions" (launchctl(1)),
//! so on a loaded, idle agent it **bypasses** the `KeepAlive` `PathState` condition on the
//! token file (`/etc/js-agent/token`) that `bootstrap` honours. An agent kicked without its
//! token would start, fail, and read `failed`: a fault of the host blamed on the release.
//! [`LaunchUnits::with_gate`] names the file, and `start` then refuses to kick an idle service
//! while the file is absent, with an error naming the file. `bootstrap` is not checked: launchd
//! applies the condition itself and leaves the daemon idle.
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

/// How long `start` waits between two reads of a service that has not begun.
const SETTLE_PAUSE: Duration = Duration::from_millis(500);

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

/// [`Units`] over `launchctl`.
pub struct LaunchUnits<'a> {
    runner: &'a dyn CommandRunner,
    /// Runs `bootout`, which waits for the agent to exit; `runner` when none was given.
    stopper: Option<&'a dyn CommandRunner>,
    /// Paces the reads that follow a start.
    clock: &'a dyn Clock,
    /// The file `KeepAlive` waits for; `kickstart` ignores it, so `start` checks it.
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

    /// Runs `bootout` through `stopper` instead of the runner: it must allow the daemon's whole
    /// `ExitTimeOut` (see [`StopRunner`]).
    #[must_use]
    pub fn with_stop_runner(mut self, stopper: &'a dyn CommandRunner) -> Self {
        self.stopper = Some(stopper);
        self
    }

    /// Paces the reads after a start with `clock` (the machine's by default).
    #[must_use]
    pub fn with_clock(mut self, clock: &'a dyn Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The file whose existence lets launchd run the daemon (the `KeepAlive` path state). With
    /// it, `start` refuses to kick an idle service while the file is absent.
    #[must_use]
    pub fn with_gate(mut self, gate: &'a Path) -> Self {
        self.gate = Some(gate);
        self
    }

    fn launchctl(&self, args: &[&str]) -> Result<String, String> {
        self.runner.run(LAUNCHCTL, args)
    }

    fn stopper(&self) -> &dyn CommandRunner {
        self.stopper.unwrap_or(self.runner)
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

/// The longest label accepted. The plist is `<label>.plist` in one directory, and a file name
/// is at most 255 bytes.
const MAX_LABEL_LEN: usize = 128;

/// A character a label may hold: visible ASCII but for the two separators, so no control
/// character, space, non-ASCII character (a bidi override reads as another name), `/` or `\`.
fn is_label_char(c: char) -> bool {
    matches!(c, '!'..='.' | '0'..='[' | ']'..='~')
}

/// A label that is one plain file name: it is joined to the directory of the plists and
/// becomes part of an argument.
fn check_label(label: &str) -> Result<(), PortError> {
    let rules = [
        !label.is_empty(),
        label.len() <= MAX_LABEL_LEN,
        !matches!(label, "." | ".."),
        label.chars().all(is_label_char),
    ];
    if rules.iter().all(|holds| *holds) {
        return Ok(());
    }
    Err(PortError::Command(format!(
        "{label:?} is not a launchd label (1 to {MAX_LABEL_LEN} visible ASCII characters, \
         no / or \\, not . or ..)"
    )))
}

/// The lines of `launchctl print` output that are properties of the service itself, one tab
/// in. The endpoints and event triggers below it have `state` lines (and others) of their own,
/// deeper in.
fn service_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter_map(|line| line.strip_prefix('\t'))
        .filter(|line| !line.starts_with(char::is_whitespace))
}

/// The value of the service's own property `key` (`key = value`), if it prints one.
fn property<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    service_lines(text)
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(" = "))
        .map(str::trim)
}

/// The `state` of the service itself.
fn printed_state(text: &str) -> Result<&str, PortError> {
    property(text, "state")
        .ok_or_else(|| PortError::Parse("launchctl print printed no state".to_owned()))
}

/// The `runs` of the service: how many times launchd has started its program. Absent is 0.
fn runs_of(text: &str) -> u64 {
    property(text, "runs")
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(0)
}

/// How the service last ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Exit code 0.
    Cleanly,
    /// A non-zero exit code (`78: EX_CONFIG` is one), or a terminating signal.
    Badly,
    /// It never exited, or launchd printed no exit code or one that is not a number.
    Unknown,
}

fn ended(text: &str) -> Ended {
    let code = property(text, "last exit code")
        .and_then(|code| code.split(':').next())
        .and_then(|code| code.trim().parse::<i64>().ok());
    match (code, property(text, "last terminating signal")) {
        (Some(0), None) => Ended::Cleanly,
        (Some(_), _) | (None, Some(_)) => Ended::Badly,
        (None, None) => Ended::Unknown,
    }
}

/// What the steps know of a unit, from the state launchd prints and how the service last ended.
fn unit_state(state: &str, ended: Ended) -> Result<UnitState, PortError> {
    let (active, sub) = match (state, ended) {
        ("running", _) => ("active", "running"),
        // The program ran and died: systemd's `failed`, which is the release's fault.
        ("not running" | "waiting" | "exited", Ended::Badly) => ("failed", "failed"),
        ("not running" | "waiting" | "exited", _) => ("inactive", "dead"),
        // A start that is coming after a clean exit is only a start.
        ("spawn scheduled", Ended::Cleanly) | ("spawning", _) => ("activating", "start"),
        // Between two starts of a `KeepAlive` daemon that died, the throttle running.
        ("spawn scheduled", _) => ("activating", "auto-restart"),
        (other, _) => {
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

/// Whether the service has not begun the start that was made: still "not running", with the
/// `runs` it had before.
fn still_idle(text: &str, runs_before: u64) -> bool {
    matches!(
        (printed_state(text), runs_of(text) > runs_before),
        (Ok("not running"), false)
    )
}

impl LaunchUnits<'_> {
    /// Reads the service again, at most [`SETTLE_READS`] times, until it has begun the start
    /// that was made (see the module documentation). A service that cannot be read ends the
    /// wait: `show` will say why.
    fn settle(&self, unit: &str, runs_before: u64) {
        for _ in 0..SETTLE_READS {
            match self.print(unit) {
                Ok(Some(text)) if still_idle(&text, runs_before) => {
                    self.clock.pause(SETTLE_PAUSE);
                }
                _ => return,
            }
        }
    }

    /// `kickstart` ignores the `KeepAlive` condition on the token file: refuse it while the
    /// file is absent.
    fn allow_kick(&self, unit: &str) -> Result<(), PortError> {
        match self.gate {
            Some(gate) if !gate.exists() => Err(PortError::Command(format!(
                "not kickstarting {unit}: {} is absent, KeepAlive holds the agent back until it \
                 exists, and kickstart would run it without",
                gate.display()
            ))),
            _ => Ok(()),
        }
    }
}

impl Units for LaunchUnits<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        check_label(unit)?;
        match self.print(unit)? {
            Some(text) => unit_state(printed_state(&text)?, ended(&text)),
            None => unit_state("not running", Ended::Unknown),
        }
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        check_label(unit)?;
        // `bootout` of a label that is not loaded fails, and a stop of a stopped unit is not an
        // error: look first.
        if self.print(unit)?.is_none() {
            return Ok(());
        }
        // It returns when the agent has exited: through the runner that allows the whole
        // `ExitTimeOut`.
        self.stopper()
            .run(LAUNCHCTL, &["bootout", &format!("system/{unit}")])
            .map(drop)
            .map_err(PortError::Command)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        check_label(unit)?;
        let (result, runs_before) = match self.print(unit)? {
            None => {
                let plist = format!("{LAUNCH_DAEMONS_DIR}/{unit}.plist");
                (self.launchctl(&["bootstrap", "system", &plist]), 0)
            }
            Some(text) if printed_state(&text)? == "running" => return Ok(()),
            Some(text) => {
                self.allow_kick(unit)?;
                let kick = self.launchctl(&["kickstart", "-k", &format!("system/{unit}")]);
                (kick, runs_of(&text))
            }
        };
        result.map_err(PortError::Command)?;
        self.settle(unit, runs_before);
        Ok(())
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
