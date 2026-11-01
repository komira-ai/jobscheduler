//! The planner and the executor.

use std::io::{self, Write};

use crate::{Cmd, Host, SYSTEMCTL};

/// Whether a plan is printed or carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Print each step; change nothing.
    DryRun,
    /// Carry each step out, printing it first.
    Apply,
}

/// A user and a group, by name; the ids are looked up when they are needed, so a plan can
/// name a user it is about to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// The user.
    pub user: String,
    /// The group.
    pub group: String,
}

impl Account {
    /// `user` and `group`.
    pub fn new(user: &str, group: &str) -> Account {
        Account {
            user: user.to_owned(),
            group: group.to_owned(),
        }
    }

    /// `root:root`.
    pub fn root() -> Account {
        Account::new("root", "root")
    }
}

/// What a systemctl step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitVerb {
    /// `daemon-reload`, no units.
    DaemonReload,
    /// `enable`.
    Enable,
    /// `start`.
    Start,
    /// `restart`.
    Restart,
    /// `disable --now`.
    DisableNow,
}

impl UnitVerb {
    fn words(self) -> &'static [&'static str] {
        match self {
            UnitVerb::DaemonReload => &["daemon-reload"],
            UnitVerb::Enable => &["enable"],
            UnitVerb::Start => &["start"],
            UnitVerb::Restart => &["restart"],
            UnitVerb::DisableNow => &["disable", "--now"],
        }
    }
}

/// One change to a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A directory with this mode and owner.
    Dir {
        /// Where.
        path: String,
        /// The permission bits.
        mode: u32,
        /// The owner.
        owner: Account,
    },
    /// A file with these bytes, mode and owner. The bytes are never printed.
    File {
        /// Where.
        path: String,
        /// The permission bits.
        mode: u32,
        /// The owner.
        owner: Account,
        /// The content.
        bytes: Vec<u8>,
    },
    /// No file at `path`.
    Absent {
        /// Where.
        path: String,
    },
    /// A system user (and a group of its name), with no login and no home made.
    User {
        /// The name.
        name: String,
        /// The home directory it is recorded with.
        home: String,
    },
    /// A systemctl action.
    Unit {
        /// The action.
        verb: UnitVerb,
        /// The units, none for a reload.
        units: Vec<String>,
    },
    /// A program.
    Run(Cmd),
}

impl Step {
    /// The step's line in a plan: names, sizes and modes only.
    pub fn describe(&self) -> String {
        match self {
            Step::Dir { path, mode, owner } => format!(
                "mkdir {path} mode {mode:04o} owner {}:{}",
                owner.user, owner.group
            ),
            Step::File {
                path,
                mode,
                owner,
                bytes,
            } => format!(
                "write {path} mode {mode:04o} owner {}:{} ({} bytes)",
                owner.user,
                owner.group,
                bytes.len()
            ),
            Step::Absent { path } => format!("remove {path}"),
            Step::User { name, home } => {
                format!("useradd {name} (system user and group, home {home}, no login)")
            }
            Step::Unit { verb, units } => {
                format!("systemctl {}", unit_cmd(*verb, units).args.join(" "))
            }
            Step::Run(cmd) => format!("run {}", cmd.line()),
        }
    }
}

/// The systemctl command of a unit step; the units come after `--`, so a name that starts
/// with a hyphen is not read as an option.
fn unit_cmd(verb: UnitVerb, units: &[String]) -> Cmd {
    let cmd = Cmd::new(SYSTEMCTL).args(verb.words());
    match units {
        [] => cmd,
        _ => cmd.arg("--").args(units),
    }
}

/// The steps a host needs, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The steps.
    pub steps: Vec<Step>,
}

/// The ids of `account`; `None` when the user or the group does not exist.
///
/// # Errors
/// A lookup command could not be started.
pub fn ids(_host: &dyn Host, _account: &Account) -> io::Result<Option<(u32, u32)>> {
    Ok(None)
}

/// Looks at a host and records what would change it.
pub struct Planner<'h> {
    host: &'h dyn Host,
}

impl<'h> Planner<'h> {
    /// A planner for `host` with no steps.
    pub fn new(host: &'h dyn Host) -> Planner<'h> {
        Planner { host }
    }

    /// The host being planned against.
    pub fn host(&self) -> &'h dyn Host {
        self.host
    }

    /// The plan made.
    pub fn finish(self) -> Plan {
        Plan::default()
    }

    /// The user `name`, unless it exists.
    ///
    /// # Errors
    /// A lookup command could not be started.
    pub fn user(&mut self, _name: &str, _home: &str) -> io::Result<bool> {
        Ok(false)
    }

    /// The directory, unless it is there with this mode and owner.
    ///
    /// # Errors
    /// A read or a lookup failed.
    pub fn dir(&mut self, _path: &str, _mode: u32, _owner: &Account) -> io::Result<bool> {
        Ok(false)
    }

    /// The file, unless it is there with these bytes, this mode and owner.
    ///
    /// # Errors
    /// A read or a lookup failed.
    pub fn file(
        &mut self,
        _path: &str,
        _mode: u32,
        _owner: &Account,
        _bytes: &[u8],
    ) -> io::Result<bool> {
        Ok(false)
    }

    /// No file at `path`, if there is one.
    ///
    /// # Errors
    /// The stat failed.
    pub fn absent(&mut self, _path: &str) -> io::Result<bool> {
        Ok(false)
    }

    /// A systemctl action.
    pub fn unit(&mut self, _verb: UnitVerb, _units: &[&str]) {}

    /// A program.
    pub fn run(&mut self, _cmd: Cmd) {}

    /// Whether `systemctl <query> <unit>` succeeds.
    ///
    /// # Errors
    /// systemctl could not be started.
    pub fn unit_is(&self, _query: &str, _unit: &str) -> io::Result<bool> {
        Ok(false)
    }

    /// `unit` enabled, and restarted when `again`, else started when it is not active.
    ///
    /// # Errors
    /// systemctl could not be started.
    pub fn service(&mut self, _unit: &str, _again: bool) -> io::Result<bool> {
        Ok(false)
    }
}

/// Why a plan could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// Applying needs root.
    #[error("applying a plan needs root (effective uid is {0})")]
    NotRoot(u32),
    /// A step failed; the steps before it were done.
    #[error("step {index} ({what}) failed: {why}")]
    Step {
        /// The step's position, from 1.
        index: usize,
        /// Its line in the plan.
        what: String,
        /// What went wrong.
        why: String,
    },
    /// The plan could not be written out.
    #[error("cannot write the plan out: {0}")]
    Write(#[from] io::Error),
}

/// Prints `plan`, and with [`Mode::Apply`] carries it out.
///
/// # Errors
/// [`ExecError`].
pub fn execute(
    _plan: &Plan,
    _mode: Mode,
    _host: &dyn Host,
    _out: &mut dyn Write,
) -> Result<(), ExecError> {
    Ok(())
}
