//! `js-vm-lease`: the lifecycle of the VM one run of a lease gets: a fresh clone of the base
//! image, booted, the run's command run in it through the guest agent (`js-guest`), its output
//! collected, then the VM stopped and its clone destroyed.
//!
//! [`VmDriver`] is the seam. The host agent (`js-agent`) drives it. The real driver is
//! `js-vmm`'s `SystemDriver` (it boots the VM through the `js-vmm vm-run` helper, macOS only);
//! it runs in the signed `js-vmm` binary, which the agent starts as `js-vmm lease` and drives
//! over the line protocol of [`protocol`], so the agent itself links nothing that starts a
//! program. The `fake` feature gives a recording driver ([`fake::FakeDriver`]) so the order
//! of the calls is tested without a Mac.
//!
//! A VM is named by its run ([`VmName`]) and every call names it, so one driver serves
//! every VM of the host from any thread: the agent runs each VM's steps on a thread of its
//! own and ends a command from another ([`VmDriver::kill`]).
//!
//! # The order of the calls
//!
//! For one VM: [`VmDriver::clone_base`], [`VmDriver::boot`], [`VmDriver::wait_ready`],
//! [`VmDriver::run_command`], [`VmDriver::collect`], then [`VmDriver::stop`] and
//! [`VmDriver::destroy`]. A step that fails skips to `stop` and `destroy`, which run
//! whatever came before: `stop` of a VM that never booted and `destroy` of one whose clone
//! was never made succeed and do nothing. A VM is never reused: after `destroy` the name
//! may be cloned again, and that is a new VM.

#[cfg(feature = "fake")]
pub mod fake;
pub mod protocol;

use std::fmt;

use serde::{Deserialize, Serialize};

/// The longest VM name: the longest run id the wire takes.
pub const MAX_NAME_LEN: usize = 128;

/// The name of one VM: the id of the run it is for. It names the clone's directory, so it
/// is 1 to [`MAX_NAME_LEN`] bytes of `[A-Za-z0-9._-]` beginning with a letter or digit
/// (never `.` or `..`, never a path).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VmName(String);

impl VmName {
    /// The name `name`, checked.
    ///
    /// # Errors
    ///
    /// [`VmError::Name`] when `name` is empty, longer than [`MAX_NAME_LEN`], holds a byte
    /// outside `[A-Za-z0-9._-]` or does not begin with a letter or digit.
    pub fn new(name: &str) -> Result<Self, VmError> {
        let ok_byte = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        let leads = name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
        match leads && name.len() <= MAX_NAME_LEN && name.bytes().all(|b| ok_byte(&b)) {
            true => Ok(Self(name.to_owned())),
            false => Err(VmError::Name(name.to_owned())),
        }
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for VmName {
    type Error = VmError;

    fn try_from(name: String) -> Result<Self, VmError> {
        Self::new(&name)
    }
}

impl From<VmName> for String {
    fn from(name: VmName) -> String {
        name.0
    }
}

impl fmt::Display for VmName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a VM boots with: whole CPUs and MiB of memory, fixed for its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSize {
    /// Virtual CPUs.
    pub cpus: u32,
    /// Memory, in MiB.
    pub memory_mib: u64,
}

/// The one command a VM runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// The program and its arguments; the guest runs it without a shell.
    pub argv: Vec<String>,
    /// The whole environment of the command, by name in order; nothing is inherited.
    pub env: Vec<(String, String)>,
    /// How long the guest lets the command run before it ends it, in milliseconds.
    pub timeout_ms: u64,
}

/// How the command ended, as the guest reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecEnd {
    /// It exited with this status.
    Exited(i32),
    /// A signal ended it.
    Signaled(i32),
    /// The guest ended it at [`ExecRequest::timeout_ms`].
    TimedOut,
    /// [`VmDriver::kill`] ended it.
    Killed,
}

/// What the command wrote to its standard output and standard error: the last bytes of
/// each, at most what [`VmDriver::collect`] was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collected {
    /// The end of the standard output.
    pub stdout: Vec<u8>,
    /// The end of the standard error.
    pub stderr: Vec<u8>,
    /// Whether either stream was longer than what is kept of it.
    pub truncated: bool,
}

/// A step of a VM's life, as a [`VmError`] names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Step {
    /// [`VmDriver::clone_base`].
    Clone,
    /// [`VmDriver::boot`].
    Boot,
    /// [`VmDriver::wait_ready`].
    Ready,
    /// [`VmDriver::run_command`].
    Exec,
    /// [`VmDriver::collect`].
    Collect,
    /// [`VmDriver::stop`].
    Stop,
    /// [`VmDriver::destroy`].
    Destroy,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clone => "clone",
            Self::Boot => "boot",
            Self::Ready => "wait for the guest",
            Self::Exec => "exec",
            Self::Collect => "collect",
            Self::Stop => "stop",
            Self::Destroy => "destroy",
        })
    }
}

/// Why a call of a [`VmDriver`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum VmError {
    /// The text is no [`VmName`].
    #[error(
        "{0:?} is not a VM name: 1 to 128 bytes of [A-Za-z0-9._-], beginning with a letter or digit"
    )]
    Name(String),
    /// A step of the VM failed; `why` is the driver's own text and stays on the host.
    #[error("VM {vm}: {step} failed: {why}")]
    Step {
        /// The VM.
        vm: VmName,
        /// What was being done.
        step: Step,
        /// Why it failed.
        why: String,
    },
    /// The store could not be listed ([`VmDriver::leftovers`]).
    #[error("the VM store cannot be listed: {0}")]
    List(String),
}

impl VmError {
    /// The failure of `step` of `vm`, for `why`.
    pub fn step(vm: &VmName, step: Step, why: impl Into<String>) -> Self {
        Self::Step {
            vm: vm.clone(),
            step,
            why: why.into(),
        }
    }
}

/// What a lease's VMs need of the host. Every call blocks until its step is done and
/// names its VM; see the [module](self) for their order.
pub trait VmDriver: Send + Sync {
    /// Clones the base image into a new clone for `vm`. A name that already has a clone
    /// is refused, and that clone is left as it is.
    ///
    /// # Errors
    ///
    /// The clone could not be made; nothing of it is left.
    fn clone_base(&self, vm: &VmName) -> Result<(), VmError>;

    /// Boots `vm`'s clone with `size`.
    ///
    /// # Errors
    ///
    /// The VM did not start.
    fn boot(&self, vm: &VmName, size: VmSize) -> Result<(), VmError>;

    /// Waits until the guest agent of the booted `vm` answers, within the driver's own
    /// bound.
    ///
    /// # Errors
    ///
    /// The guest did not answer in time, or the VM stopped.
    fn wait_ready(&self, vm: &VmName) -> Result<(), VmError>;

    /// Runs `request` in the ready `vm` and waits for it to end. It runs at most once per
    /// VM. A `vm` that was killed ([`VmDriver::kill`]) before the call ends at once,
    /// [`ExecEnd::Killed`], having run nothing.
    ///
    /// # Errors
    ///
    /// The guest refused the command or the connection to it broke.
    fn run_command(&self, vm: &VmName, request: &ExecRequest) -> Result<ExecEnd, VmError>;

    /// Ends the command `vm` runs, from any thread, and marks `vm` killed so that a later
    /// [`VmDriver::run_command`] runs nothing. Killing a VM that runs nothing only marks it; one the
    /// driver does not know yet (its clone is not made) may be left unmarked, so a caller
    /// that can stop a VM that early keeps its own mark and checks it after the clone.
    fn kill(&self, vm: &VmName);

    /// The end of what the command of `vm` wrote, at most `tail_bytes` of each stream.
    ///
    /// # Errors
    ///
    /// The output could not be read.
    fn collect(&self, vm: &VmName, tail_bytes: usize) -> Result<Collected, VmError>;

    /// Stops `vm`: asks the guest to shut down and forces it off when it does not. A VM
    /// that is not running (never booted, or stopped) is left as it is.
    ///
    /// # Errors
    ///
    /// The VM could not be stopped.
    fn stop(&self, vm: &VmName) -> Result<(), VmError>;

    /// Removes everything of `vm`: its clone and what the driver kept for it. Destroying
    /// a VM that has nothing left succeeds.
    ///
    /// # Errors
    ///
    /// Something of the VM could not be removed.
    fn destroy(&self, vm: &VmName) -> Result<(), VmError>;

    /// The VMs whose clones exist, for an agent that starts after another process of it
    /// ended: each is stopped and destroyed, never adopted.
    ///
    /// # Errors
    ///
    /// The store could not be listed.
    fn leftovers(&self) -> Result<Vec<VmName>, VmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts which names are accepted: run ids of the wire's alphabet, and nothing that
    /// could name another directory than the clone's. Catches `..`, a path and an empty
    /// name reaching the store.
    #[test]
    fn a_name_is_a_run_id_and_never_a_path() {
        for good in ["run-1", "a", "Job_7.x", &"r".repeat(MAX_NAME_LEN)] {
            assert_eq!(
                VmName::new(good).map(|n| n.to_string()),
                Ok(good.to_owned())
            );
        }
        for bad in [
            "",
            ".",
            "..",
            "-a",
            "a/b",
            "a b",
            "é",
            &"r".repeat(MAX_NAME_LEN + 1),
        ] {
            assert_eq!(
                VmName::new(bad),
                Err(VmError::Name(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    /// Asserts the exact text of each error, which the agent logs: the VM, the step and the
    /// driver's reason. Catches a step named wrongly in a log an operator reads.
    #[test]
    fn errors_name_the_vm_and_the_step() {
        let vm = VmName::new("run-1").unwrap();
        let steps = [
            (Step::Clone, "clone"),
            (Step::Boot, "boot"),
            (Step::Ready, "wait for the guest"),
            (Step::Exec, "exec"),
            (Step::Collect, "collect"),
            (Step::Stop, "stop"),
            (Step::Destroy, "destroy"),
        ];
        for (step, said) in steps {
            assert_eq!(
                VmError::step(&vm, step, "no").to_string(),
                format!("VM run-1: {said} failed: no")
            );
        }
        assert_eq!(
            VmError::Name("a/b".to_owned()).to_string(),
            "\"a/b\" is not a VM name: 1 to 128 bytes of [A-Za-z0-9._-], beginning with a \
             letter or digit"
        );
        assert_eq!(
            VmError::List("gone".to_owned()).to_string(),
            "the VM store cannot be listed: gone"
        );
        assert_eq!(vm.as_str(), "run-1");
    }
}
