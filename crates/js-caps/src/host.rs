//! The host contract: the capabilities an agent reports about what its host can run
//! jobs with, and the check a pool makes of them at admission.
//!
//! An agent reports these three keys in its hello, from what it actually finds, and an
//! operator cannot set them ([`DETECTED_KEYS`]):
//!
//! | Key | Value | Reported when |
//! |---|---|---|
//! | [`RUNTIME`] | an [`ExecMode`] name: `container` or `sandbox-exec` | the program the agent runs jobs with was found and ran |
//! | [`GPU_TOOLKIT`] | [`FLAG_SET`] | the host offers GPUs and the toolkit check passed |
//! | [`EXEC`] | an [`ExecMode`] name | the agent runs jobs, in that mode |
//!
//! A pool that declares an execution mode makes a [`HostContract`] of it: `runtime` must
//! equal the mode, and a pool with GPUs also needs `gpu-toolkit`. The check is
//! [`HostContract::check`]; it answers with the [`Breach`] that decides which refusal
//! the registry sends. Keys missing from a report are never filled in: an agent that
//! predates the contract reports none of them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// The capability key of the program the agent runs jobs with.
pub const RUNTIME: &str = "runtime";

/// The capability key of a working GPU container toolkit.
pub const GPU_TOOLKIT: &str = "gpu-toolkit";

/// The capability key of the mode the agent runs jobs in.
pub const EXEC: &str = "exec";

/// The value of a capability that is a flag: present means set.
pub const FLAG_SET: &str = "true";

/// The keys an agent detects and an operator cannot set with a flag.
pub const DETECTED_KEYS: [&str; 3] = [RUNTIME, GPU_TOOLKIT, EXEC];

/// How a host runs jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecMode {
    /// In a container, with Docker or Podman.
    Container,
    /// As a process under a `sandbox-exec` profile (macOS).
    SandboxExec,
}

impl ExecMode {
    /// The text that names the mode in a pool's `exec` and in a hello.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

impl fmt::Display for ExecMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that names no execution mode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown execution mode {0:?}; known: container, sandbox-exec")]
pub struct UnknownExecMode(pub String);

impl FromStr for ExecMode {
    type Err = UnknownExecMode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "container" => Ok(Self::Container),
            "sandbox-exec" => Ok(Self::SandboxExec),
            _ => Err(UnknownExecMode(s.to_owned())),
        }
    }
}

/// How a report fails a [`HostContract`]. Each names the capability at fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Breach {
    /// A capability the contract needs is not in the report (or, for the toolkit, is not
    /// set): the host has no runtime, or no working toolkit.
    #[error("the host contract needs the capability {field}")]
    Missing {
        /// The capability key.
        field: &'static str,
    },
    /// A capability names a mode other than the pool's.
    #[error("the capability {field} names another execution mode than the pool's")]
    Mismatch {
        /// The capability key.
        field: &'static str,
    },
}

impl Breach {
    /// The capability key at fault.
    #[must_use]
    pub fn field(self) -> &'static str {
        match self {
            Self::Missing { field } | Self::Mismatch { field } => field,
        }
    }
}

/// What a pool requires of the hosts that join it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostContract {
    exec: ExecMode,
    gpus: bool,
}

impl HostContract {
    /// The contract of a pool whose hosts run jobs in `exec`; `gpus` is whether the pool
    /// offers GPUs, which makes the toolkit part of the contract.
    #[must_use]
    pub fn new(exec: ExecMode, gpus: bool) -> Self {
        Self { exec, gpus }
    }

    /// Checks the capabilities a hello reported, in this order, and answers with the
    /// first breach:
    ///
    /// 1. `exec`, when reported, differs from the pool's mode: [`Breach::Mismatch`] on `exec`.
    /// 2. `runtime` is not reported: [`Breach::Missing`] on `runtime`.
    /// 3. `runtime` differs from the pool's mode: [`Breach::Mismatch`] on `runtime`.
    /// 4. The pool has GPUs and `gpu-toolkit` is not [`FLAG_SET`]: [`Breach::Missing`] on
    ///    `gpu-toolkit`.
    ///
    /// # Errors
    ///
    /// The first [`Breach`] of the list above.
    pub fn check(&self, reported: &BTreeMap<String, String>) -> Result<(), Breach> {
        let mode = self.exec.name();
        let differs = |key: &str| reported.get(key).is_some_and(|value| value != mode);
        if differs(EXEC) {
            return Err(Breach::Mismatch { field: EXEC });
        }
        if !reported.contains_key(RUNTIME) {
            return Err(Breach::Missing { field: RUNTIME });
        }
        if differs(RUNTIME) {
            return Err(Breach::Mismatch { field: RUNTIME });
        }
        let toolkit = reported.get(GPU_TOOLKIT).map(String::as_str) == Some(FLAG_SET);
        if self.gpus && !toolkit {
            return Err(Breach::Missing { field: GPU_TOOLKIT });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Asserts that both modes parse from and print as the names a pool and a hello use,
    /// and that any other text is refused naming what is known.
    #[test]
    fn modes_round_trip_and_unknown_is_refused() {
        for mode in [ExecMode::Container, ExecMode::SandboxExec] {
            assert_eq!(mode.to_string().parse::<ExecMode>(), Ok(mode));
        }
        let err = "Container".parse::<ExecMode>().unwrap_err();
        assert_eq!(err, UnknownExecMode("Container".to_owned()));
        assert!(err.to_string().contains("known: container, sandbox-exec"));
    }

    /// Asserts each step of the check on its own, so the order of the list in
    /// [`HostContract::check`] is the order that decides: a report that breaks two steps
    /// gets the first one's breach.
    #[test]
    fn the_first_breach_decides() {
        let pool = HostContract::new(ExecMode::Container, true);
        let whole = [
            (RUNTIME, "container"),
            (EXEC, "container"),
            (GPU_TOOLKIT, "true"),
        ];
        assert_eq!(pool.check(&report(&whole)), Ok(()));

        let none = report(&[]);
        assert_eq!(pool.check(&none), Err(Breach::Missing { field: RUNTIME }));

        let other_exec = report(&[(RUNTIME, "container"), (EXEC, "sandbox-exec")]);
        let breach = Breach::Mismatch { field: EXEC };
        assert_eq!(pool.check(&other_exec), Err(breach));
        assert_eq!(breach.field(), "exec");

        let other_runtime = report(&[(RUNTIME, "sandbox-exec"), (GPU_TOOLKIT, "true")]);
        let breach = Breach::Mismatch { field: RUNTIME };
        assert_eq!(pool.check(&other_runtime), Err(breach));
        assert_eq!(breach.field(), "runtime");

        let no_toolkit = report(&[(RUNTIME, "container"), (EXEC, "container")]);
        let breach = Breach::Missing { field: GPU_TOOLKIT };
        assert_eq!(pool.check(&no_toolkit), Err(breach));
        assert!(breach.to_string().contains("gpu-toolkit"));
        assert_eq!(breach.field(), "gpu-toolkit");
        let said = Breach::Mismatch { field: RUNTIME }.to_string();
        assert!(said.contains("another execution mode"), "{said}");
        let unset = report(&[(RUNTIME, "container"), (GPU_TOOLKIT, "false")]);
        assert_eq!(pool.check(&unset), Err(breach));
    }

    /// Asserts that a pool without GPUs does not ask for the toolkit, and that a report
    /// with no `exec` passes when `runtime` is right (an agent need not name both).
    #[test]
    fn a_pool_without_gpus_needs_no_toolkit() {
        let pool = HostContract::new(ExecMode::SandboxExec, false);
        let mac = report(&[(RUNTIME, "sandbox-exec")]);
        assert_eq!(pool.check(&mac), Ok(()));
        let container = report(&[(RUNTIME, "container")]);
        let breach = Breach::Mismatch { field: RUNTIME };
        assert_eq!(pool.check(&container), Err(breach));
    }
}
