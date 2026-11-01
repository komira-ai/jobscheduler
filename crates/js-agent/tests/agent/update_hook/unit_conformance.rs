//! One suite for both unit managers. The hook's steps ask the [`Units`] port the same things on
//! a Linux host (`systemctl`) and on a Mac (`launchctl`): the state of the agent's unit, stop
//! it, start it. Each manager is stood in for by a fake of the tool that keeps the world of the
//! units in memory, and the real port runs over it, so a behaviour is checked in the port's
//! command lines, in its reading of what the tool prints and in the fake's own world at once.
//! A behaviour is written once, as a function of [`Manager`], and run against every fake.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use js_agent::CommandRunner;
use js_agent::update_hook::{LAUNCHCTL, LaunchUnits, PortError, SYSTEMCTL, SystemUnits, Units};

/// How a unit is when a test begins.
#[derive(Clone, Copy, Debug)]
pub enum Condition {
    /// Running.
    Running,
    /// The manager knows it and it is not running.
    Stopped,
    /// It died and the manager will start it again; it is not running now.
    Restarting,
}

/// A unit manager with a fake tool under the port.
pub trait Manager {
    /// The port, over the fake tool.
    fn units(&self) -> Box<dyn Units + '_>;
    /// Puts `unit` in `condition`.
    fn put(&self, unit: &str, condition: Condition);
    /// How many times the program of `unit` has been started.
    fn launches(&self, unit: &str) -> u32;
    /// The tool cannot be run from now on.
    fn break_tool(&self);
    /// The tool's absolute path, which an error names.
    fn tool(&self) -> &'static str;
    /// The command lines run so far.
    fn ran(&self) -> Vec<String>;
}

const UNIT: &str = "agent";

// ---- systemctl ----

/// A `systemctl` that keeps units: `(ActiveState, SubState)` and a count of starts.
#[derive(Default)]
pub struct Systemctl {
    units: RefCell<BTreeMap<String, (String, String, u32)>>,
    broken: Cell<bool>,
    ran: RefCell<Vec<String>>,
}

impl CommandRunner for Systemctl {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.ran
            .borrow_mut()
            .push(format!("{program} {}", args.join(" ")));
        if self.broken.get() || program != SYSTEMCTL {
            return Err(format!("{program}: not found"));
        }
        let mut units = self.units.borrow_mut();
        let exit = |code: u8| Err(format!("{SYSTEMCTL}: exit status: {code}"));
        match args {
            ["show", "-p", "ActiveState,SubState", "--", unit] => {
                // A unit never heard of shows as inactive, and the exit status is 0.
                let (active, sub, _) =
                    units
                        .get(*unit)
                        .cloned()
                        .unwrap_or(("inactive".into(), "dead".into(), 0));
                Ok(format!("ActiveState={active}\nSubState={sub}\n"))
            }
            ["stop", "--no-block", "--", unit] => match units.get_mut(*unit) {
                Some(state) => {
                    (state.0, state.1) = ("inactive".into(), "dead".into());
                    Ok(String::new())
                }
                None => exit(5),
            },
            ["start", "--", unit] => match units.get_mut(*unit) {
                Some(state) if state.0 == "active" => Ok(String::new()),
                Some(state) => {
                    (state.0, state.1) = ("active".into(), "running".into());
                    state.2 += 1;
                    Ok(String::new())
                }
                None => exit(5),
            },
            ["is-active", "--", unit] => match units.get(*unit) {
                Some(state) if state.0 == "active" => Ok("active\n".to_owned()),
                _ => exit(3),
            },
            other => Err(format!("{SYSTEMCTL}: no such command {other:?}")),
        }
    }

    fn run_in(&self, program: &str, args: &[&str], _: &[(&str, &str)]) -> Result<String, String> {
        self.run(program, args)
    }
}

pub struct SystemdManager(pub Systemctl);

impl SystemdManager {
    pub fn new() -> Self {
        Self(Systemctl::default())
    }
}

impl Manager for SystemdManager {
    fn units(&self) -> Box<dyn Units + '_> {
        Box::new(SystemUnits::new(&self.0))
    }

    fn put(&self, unit: &str, condition: Condition) {
        let (active, sub) = match condition {
            Condition::Running => ("active", "running"),
            Condition::Stopped => ("inactive", "dead"),
            Condition::Restarting => ("activating", "auto-restart"),
        };
        let launches = u32::from(matches!(condition, Condition::Running));
        self.0
            .units
            .borrow_mut()
            .insert(unit.to_owned(), (active.into(), sub.into(), launches));
    }

    fn launches(&self, unit: &str) -> u32 {
        self.0.units.borrow().get(unit).map_or(0, |state| state.2)
    }

    fn break_tool(&self) {
        self.0.broken.set(true);
    }

    fn tool(&self) -> &'static str {
        SYSTEMCTL
    }

    fn ran(&self) -> Vec<String> {
        self.0.ran.borrow().clone()
    }
}

// ---- launchctl ----

/// What launchd holds of a loaded service.
#[derive(Clone)]
pub struct Service {
    /// The `state` line `launchctl print` shows.
    pub state: &'static str,
}

/// A `launchctl` that keeps services in the system domain, the plists in
/// `/Library/LaunchDaemons`, and a count of starts by label.
#[derive(Default)]
pub struct Launchctl {
    pub loaded: RefCell<BTreeMap<String, Service>>,
    pub plists: RefCell<BTreeSet<String>>,
    pub launches: RefCell<BTreeMap<String, u32>>,
    broken: Cell<bool>,
    ran: RefCell<Vec<String>>,
}

impl Launchctl {
    fn started(&self, label: &str) {
        *self
            .launches
            .borrow_mut()
            .entry(label.to_owned())
            .or_default() += 1;
    }

    /// The command lines run so far.
    pub fn calls(&self) -> Vec<String> {
        self.ran.borrow().clone()
    }

    /// A service launchd holds that is not running (a daemon whose gate file is absent, or
    /// one between two starts).
    pub fn load_idle(&self, label: &str, state: &'static str) {
        self.plists.borrow_mut().insert(label.to_owned());
        self.loaded
            .borrow_mut()
            .insert(label.to_owned(), Service { state });
    }
}

/// The plist path `launchctl bootstrap` is given for `label`.
pub fn plist_of(label: &str) -> String {
    format!("/Library/LaunchDaemons/{label}.plist")
}

/// What `launchctl print system/<label>` prints of a service: the properties of the service are
/// one tab in, and the endpoints and the event triggers below it have `state` lines of their
/// own.
fn print_of(label: &str, state: &str) -> String {
    format!(
        "system/{label} = {{\n\tactive count = 1\n\tpath = {path}\n\ttype = LaunchDaemon\n\
         \tstate = {state}\n\n\tprogram = /usr/local/js-agent/link/current/js-agent\n\
         \tendpoints = {{\n\t\t\"com.komira.x\" = {{\n\t\t\tport = 0x1\n\t\t\tstate = active\n\
         \t\t}}\n\t}}\n\n\tevent triggers = {{\n\t\tcom.komira.y => {{\n\t\t\tstate = waiting\n\
         \t\t}}\n\t}}\n}}\n",
        path = plist_of(label),
    )
}

impl CommandRunner for Launchctl {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.ran
            .borrow_mut()
            .push(format!("{program} {}", args.join(" ")));
        if self.broken.get() || program != LAUNCHCTL {
            return Err(format!("{program}: not found"));
        }
        let exit = |code: u8| Err(format!("{LAUNCHCTL}: exit status: {code}"));
        let domain_label = |target: &str| target.strip_prefix("system/").map(str::to_owned);
        match args {
            ["print", target] => {
                let label = domain_label(target).ok_or("not in the system domain")?;
                match self.loaded.borrow().get(&label) {
                    Some(service) => Ok(print_of(&label, service.state)),
                    // "Could not find service".
                    None => exit(113),
                }
            }
            ["bootout", target] => {
                let label = domain_label(target).ok_or("not in the system domain")?;
                match self.loaded.borrow_mut().remove(&label) {
                    Some(_) => Ok(String::new()),
                    None => exit(3),
                }
            }
            ["bootstrap", "system", plist] => {
                let label = plist
                    .strip_prefix("/Library/LaunchDaemons/")
                    .and_then(|name| name.strip_suffix(".plist"))
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{LAUNCHCTL}: bad plist path {plist}"))?;
                let known = self.plists.borrow().contains(&label);
                let mut loaded = self.loaded.borrow_mut();
                if !known || loaded.contains_key(&label) {
                    return exit(5);
                }
                // The gate file exists in this world: launchd starts the daemon at load.
                loaded.insert(label.clone(), Service { state: "running" });
                self.started(&label);
                Ok(String::new())
            }
            ["kickstart", "-k", target] => {
                let label = domain_label(target).ok_or("not in the system domain")?;
                match self.loaded.borrow_mut().get_mut(&label) {
                    Some(service) => {
                        service.state = "running";
                        self.started(&label);
                        Ok(String::new())
                    }
                    None => exit(113),
                }
            }
            other => Err(format!("{LAUNCHCTL}: no such command {other:?}")),
        }
    }

    fn run_in(&self, program: &str, args: &[&str], _: &[(&str, &str)]) -> Result<String, String> {
        self.run(program, args)
    }
}

pub struct LaunchdManager(pub Launchctl);

impl LaunchdManager {
    pub fn new() -> Self {
        Self(Launchctl::default())
    }
}

impl Manager for LaunchdManager {
    fn units(&self) -> Box<dyn Units + '_> {
        Box::new(LaunchUnits::new(&self.0))
    }

    fn put(&self, unit: &str, condition: Condition) {
        self.0.plists.borrow_mut().insert(unit.to_owned());
        match condition {
            Condition::Running => {
                self.0
                    .loaded
                    .borrow_mut()
                    .insert(unit.to_owned(), Service { state: "running" });
                *self
                    .0
                    .launches
                    .borrow_mut()
                    .entry(unit.to_owned())
                    .or_default() += 1;
            }
            // A daemon the hook stopped is booted out: launchd holds nothing of it.
            Condition::Stopped => {
                self.0.loaded.borrow_mut().remove(unit);
            }
            Condition::Restarting => self.0.load_idle(unit, "spawn scheduled"),
        }
    }

    fn launches(&self, unit: &str) -> u32 {
        self.0.launches.borrow().get(unit).copied().unwrap_or(0)
    }

    fn break_tool(&self) {
        self.0.broken.set(true);
    }

    fn tool(&self) -> &'static str {
        LAUNCHCTL
    }

    fn ran(&self) -> Vec<String> {
        self.0.calls()
    }
}

// ---- the behaviours ----

fn state(m: &dyn Manager) -> (String, String) {
    let state = m.units().show(UNIT).unwrap();
    (state.active, state.sub)
}

fn running_unit_is_active(m: &dyn Manager) {
    m.put(UNIT, Condition::Running);
    assert_eq!(state(m), ("active".to_owned(), "running".to_owned()));
    assert_eq!(m.units().is_active(UNIT), Ok(true));
}

fn stopped_unit_is_inactive(m: &dyn Manager) {
    m.put(UNIT, Condition::Stopped);
    assert_eq!(state(m), ("inactive".to_owned(), "dead".to_owned()));
    assert_eq!(m.units().is_active(UNIT), Ok(false));
}

fn unit_never_heard_of_is_inactive(m: &dyn Manager) {
    assert_eq!(state(m), ("inactive".to_owned(), "dead".to_owned()));
    assert_eq!(m.units().is_active(UNIT), Ok(false));
}

fn unit_waiting_to_restart_is_neither_active_nor_stopped(m: &dyn Manager) {
    m.put(UNIT, Condition::Restarting);
    assert_eq!(
        state(m),
        ("activating".to_owned(), "auto-restart".to_owned())
    );
    assert_eq!(m.units().is_active(UNIT), Ok(false));
}

fn stop_ends_a_running_unit(m: &dyn Manager) {
    m.put(UNIT, Condition::Running);
    m.units().stop_no_block(UNIT).unwrap();
    assert_eq!(state(m), ("inactive".to_owned(), "dead".to_owned()));
    assert_eq!(m.units().is_active(UNIT), Ok(false));
}

fn stopping_a_stopped_unit_is_not_an_error_and_starts_nothing(m: &dyn Manager) {
    m.put(UNIT, Condition::Stopped);
    let before = m.launches(UNIT);
    m.units().stop_no_block(UNIT).unwrap();
    m.units().stop_no_block(UNIT).unwrap();
    assert_eq!(state(m), ("inactive".to_owned(), "dead".to_owned()));
    assert_eq!(m.launches(UNIT), before);
}

fn start_runs_a_stopped_unit_once(m: &dyn Manager) {
    m.put(UNIT, Condition::Stopped);
    let before = m.launches(UNIT);
    m.units().start(UNIT).unwrap();
    assert_eq!(state(m), ("active".to_owned(), "running".to_owned()));
    assert_eq!(m.launches(UNIT), before + 1);
}

fn start_leaves_a_running_unit_alone(m: &dyn Manager) {
    m.put(UNIT, Condition::Running);
    let before = m.launches(UNIT);
    m.units().start(UNIT).unwrap();
    assert_eq!(state(m), ("active".to_owned(), "running".to_owned()));
    assert_eq!(
        m.launches(UNIT),
        before,
        "a start of a running unit restarts nothing"
    );
}

fn start_runs_a_unit_waiting_to_restart(m: &dyn Manager) {
    m.put(UNIT, Condition::Restarting);
    m.units().start(UNIT).unwrap();
    assert_eq!(state(m), ("active".to_owned(), "running".to_owned()));
}

fn stop_then_start_is_a_restart(m: &dyn Manager) {
    m.put(UNIT, Condition::Running);
    let before = m.launches(UNIT);
    m.units().stop_no_block(UNIT).unwrap();
    m.units().start(UNIT).unwrap();
    assert_eq!(state(m), ("active".to_owned(), "running".to_owned()));
    assert_eq!(m.launches(UNIT), before + 1);
}

fn start_of_a_unit_the_manager_cannot_run_is_an_error_naming_the_tool(m: &dyn Manager) {
    let Err(PortError::Command(text)) = m.units().start(UNIT) else {
        panic!("start of an unknown unit must be a command error");
    };
    assert!(text.contains(m.tool()), "{text}");
    assert_eq!(m.launches(UNIT), 0);
}

fn a_tool_that_cannot_run_is_an_error_and_reads_as_not_active(m: &dyn Manager) {
    m.put(UNIT, Condition::Running);
    m.break_tool();
    let units = m.units();
    for (call, got) in [
        ("show", units.show(UNIT).map(drop)),
        ("stop", units.stop_no_block(UNIT)),
        ("start", units.start(UNIT)),
    ] {
        let Err(PortError::Command(text)) = got else {
            panic!("{call} with no tool must be a command error: {got:?}");
        };
        assert!(text.contains(m.tool()), "{call}: {text}");
    }
    assert_eq!(units.is_active(UNIT), Ok(false));
}

fn every_command_is_the_tool_by_its_absolute_path(m: &dyn Manager) {
    m.put(UNIT, Condition::Running);
    let units = m.units();
    let _ = (
        units.show(UNIT),
        units.stop_no_block(UNIT),
        units.start(UNIT),
    );
    let _ = units.is_active(UNIT);
    for line in m.ran() {
        assert!(line.starts_with(m.tool()), "{line}");
    }
}

macro_rules! suite {
    ($name:ident, $make:expr, [$($behaviour:ident),* $(,)?]) => {
        mod $name {
            use super::*;
            $(
                #[test]
                fn $behaviour() {
                    super::$behaviour(&$make);
                }
            )*
        }
    };
}

macro_rules! both {
    ($($behaviour:ident),* $(,)?) => {
        suite!(systemd, SystemdManager::new(), [$($behaviour),*]);
        suite!(launchd, LaunchdManager::new(), [$($behaviour),*]);
    };
}

both!(
    running_unit_is_active,
    stopped_unit_is_inactive,
    unit_never_heard_of_is_inactive,
    unit_waiting_to_restart_is_neither_active_nor_stopped,
    stop_ends_a_running_unit,
    stopping_a_stopped_unit_is_not_an_error_and_starts_nothing,
    start_runs_a_stopped_unit_once,
    start_leaves_a_running_unit_alone,
    start_runs_a_unit_waiting_to_restart,
    stop_then_start_is_a_restart,
    start_of_a_unit_the_manager_cannot_run_is_an_error_naming_the_tool,
    a_tool_that_cannot_run_is_an_error_and_reads_as_not_active,
    every_command_is_the_tool_by_its_absolute_path,
);
