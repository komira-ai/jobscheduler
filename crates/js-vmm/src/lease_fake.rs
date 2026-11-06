//! A [`VmDriver`] in memory, for the tests of what drives one (the `fake` feature): it
//! makes no clone and boots nothing, records every call in order, and answers as the test
//! scripted it. A test reads the calls back ([`FakeDriver::calls`]) to check the order of a
//! VM's life, and waits for a call ([`FakeDriver::wait_for`]) instead of for time.
//!
//! With [`FakeDriver::holding`] an exec does not end by itself: it waits until the test
//! lets it ([`FakeDriver::finish`]) or the VM is killed, so a test holds VMs in flight for
//! as long as it needs them.

use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmName, VmSize};

/// How long a wait of the fake lasts before it gives up: far above any test, so it ends a
/// test that would otherwise hang, and never decides one that passes.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// One call the fake was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// [`VmDriver::clone_base`].
    Clone(VmName),
    /// [`VmDriver::boot`], with the size.
    Boot(VmName, VmSize),
    /// [`VmDriver::wait_ready`].
    WaitReady(VmName),
    /// [`VmDriver::exec`], with the request.
    Exec(VmName, ExecRequest),
    /// [`VmDriver::kill`].
    Kill(VmName),
    /// [`VmDriver::collect`], with the bytes asked for.
    Collect(VmName, usize),
    /// [`VmDriver::stop`].
    Stop(VmName),
    /// [`VmDriver::destroy`].
    Destroy(VmName),
    /// [`VmDriver::leftovers`].
    Leftovers,
}

impl Call {
    /// The step of the call, `None` for [`Call::Kill`] and [`Call::Leftovers`].
    #[must_use]
    pub fn step(&self) -> Option<Step> {
        match self {
            Self::Clone(_) => Some(Step::Clone),
            Self::Boot(..) => Some(Step::Boot),
            Self::WaitReady(_) => Some(Step::Ready),
            Self::Exec(..) => Some(Step::Exec),
            Self::Collect(..) => Some(Step::Collect),
            Self::Stop(_) => Some(Step::Stop),
            Self::Destroy(_) => Some(Step::Destroy),
            Self::Kill(_) | Self::Leftovers => None,
        }
    }

    /// The VM the call names, `None` for [`Call::Leftovers`].
    #[must_use]
    pub fn vm(&self) -> Option<&VmName> {
        match self {
            Self::Clone(vm)
            | Self::Boot(vm, _)
            | Self::WaitReady(vm)
            | Self::Exec(vm, _)
            | Self::Kill(vm)
            | Self::Collect(vm, _)
            | Self::Stop(vm)
            | Self::Destroy(vm) => Some(vm),
            Self::Leftovers => None,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    calls: Vec<Call>,
    /// The steps whose next call fails, each once.
    failing: BTreeSet<Step>,
    /// The store cannot be listed.
    list_fails: bool,
    /// Whether an exec waits for [`FakeDriver::finish`] or a kill.
    holding: bool,
    /// Whether a held exec ignores a kill and ends only when its VM is stopped.
    deaf: bool,
    /// The VMs stopped since they were cloned.
    stopped: BTreeSet<VmName>,
    /// The VMs a held exec may end for.
    finished: BTreeSet<VmName>,
    killed: BTreeSet<VmName>,
    end: Option<ExecEnd>,
    output: Collected,
    /// The VMs booted and not yet destroyed, and the most there were at once.
    live: BTreeSet<VmName>,
    peak: usize,
    /// The clones that exist, and those an earlier process left.
    clones: BTreeSet<VmName>,
    /// How long a wait lasts; [`HANG_GUARD`] unless a test set it.
    guard: Option<Duration>,
}

impl State {
    fn guard(&self) -> Duration {
        self.guard.unwrap_or(HANG_GUARD)
    }
}

/// The recording driver; see the [module](self).
#[derive(Debug, Default)]
pub struct FakeDriver {
    state: Mutex<State>,
    changed: Condvar,
}

impl FakeDriver {
    /// A driver whose every step works and whose command exits 0 having written nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same, whose command ends with `end` and wrote `stdout` and `stderr`.
    #[must_use]
    pub fn answering(self, end: ExecEnd, stdout: &str, stderr: &str) -> Self {
        {
            let mut state = self.lock();
            state.end = Some(end);
            state.output = Collected {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                truncated: false,
            };
        }
        self
    }

    /// The same, whose next call of `step` fails (once).
    #[must_use]
    pub fn failing(self, step: Step) -> Self {
        self.lock().failing.insert(step);
        self
    }

    /// The same, whose store cannot be listed.
    #[must_use]
    pub fn unlistable(self) -> Self {
        self.lock().list_fails = true;
        self
    }

    /// The same, whose exec waits until [`FakeDriver::finish`] or a kill ends it.
    #[must_use]
    pub fn holding(self) -> Self {
        self.lock().holding = true;
        self
    }

    /// The same, whose held exec ignores a kill (a guest that does not answer) and ends,
    /// failed, only when its VM is stopped.
    #[must_use]
    pub fn deaf(self) -> Self {
        {
            let mut state = self.lock();
            state.holding = true;
            state.deaf = true;
        }
        self
    }

    /// The same, with clones of `names` left by an earlier process.
    #[must_use]
    pub fn leaving(self, names: &[VmName]) -> Self {
        self.lock().clones.extend(names.iter().cloned());
        self
    }

    /// The same, whose waits give up after `guard` instead of [`HANG_GUARD`]: for a test of
    /// what a wait that gives up does.
    #[must_use]
    pub fn guarded(self, guard: Duration) -> Self {
        self.lock().guard = Some(guard);
        self
    }

    /// Lets the held exec of `vm` end as scripted.
    pub fn finish(&self, vm: &VmName) {
        self.lock().finished.insert(vm.clone());
        self.changed.notify_all();
    }

    /// Every call so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The steps of the calls that named `vm`, in order (a kill is not a step).
    #[must_use]
    pub fn steps_of(&self, vm: &VmName) -> Vec<Step> {
        let state = self.lock();
        let named = state.calls.iter().filter(|call| call.vm() == Some(vm));
        named.filter_map(Call::step).collect()
    }

    /// How many VMs are booted and not destroyed.
    #[must_use]
    pub fn live(&self) -> usize {
        self.lock().live.len()
    }

    /// The most VMs that were booted and not destroyed at once.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.lock().peak
    }

    /// The clones that exist.
    #[must_use]
    pub fn clones(&self) -> Vec<VmName> {
        self.lock().clones.iter().cloned().collect()
    }

    /// Waits until a call of `step` names `vm`.
    ///
    /// # Panics
    ///
    /// When none comes within [`HANG_GUARD`]: the test would hang otherwise.
    pub fn wait_for(&self, vm: &VmName, step: Step) {
        let seen = |state: &State| {
            let mut calls = state.calls.iter();
            calls.any(|call| call.vm() == Some(vm) && call.step() == Some(step))
        };
        let state = self.lock();
        let guard = state.guard();
        let (_state, waited) = self
            .changed
            .wait_timeout_while(state, guard, |state| !seen(state))
            .unwrap_or_else(PoisonError::into_inner);
        assert!(!waited.timed_out(), "VM {vm}: no {step} call came");
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `call` and answers whether its step is to fail.
    fn record(&self, call: Call) -> Result<(), VmError> {
        let mut state = self.lock();
        let step = call.step();
        let vm = call.vm().cloned();
        state.calls.push(call);
        self.changed.notify_all();
        match (step, vm) {
            (Some(step), Some(vm)) if state.failing.remove(&step) => {
                Err(VmError::step(&vm, step, "the fake was told to fail"))
            }
            _ => Ok(()),
        }
    }
}

impl VmDriver for FakeDriver {
    fn clone_base(&self, vm: &VmName) -> Result<(), VmError> {
        self.record(Call::Clone(vm.clone()))?;
        let mut state = self.lock();
        match state.clones.insert(vm.clone()) {
            true => Ok(()),
            false => Err(VmError::step(vm, Step::Clone, "the clone exists")),
        }
    }

    fn boot(&self, vm: &VmName, size: VmSize) -> Result<(), VmError> {
        self.record(Call::Boot(vm.clone(), size))?;
        let mut state = self.lock();
        state.live.insert(vm.clone());
        state.peak = state.peak.max(state.live.len());
        Ok(())
    }

    fn wait_ready(&self, vm: &VmName) -> Result<(), VmError> {
        self.record(Call::WaitReady(vm.clone()))
    }

    fn exec(&self, vm: &VmName, request: &ExecRequest) -> Result<ExecEnd, VmError> {
        self.record(Call::Exec(vm.clone(), request.clone()))?;
        let state = self.lock();
        let (holding, deaf, guard) = (state.holding, state.deaf, state.guard());
        let (state, waited) = self
            .changed
            .wait_timeout_while(state, guard, |state| {
                let heard = !deaf && state.killed.contains(vm);
                holding && !heard && !state.finished.contains(vm) && !state.stopped.contains(vm)
            })
            .unwrap_or_else(PoisonError::into_inner);
        if waited.timed_out() {
            return Err(VmError::step(
                vm,
                Step::Exec,
                "the held exec was never let end",
            ));
        }
        if state.stopped.contains(vm) {
            return Err(VmError::step(vm, Step::Exec, "the VM was stopped"));
        }
        match state.killed.contains(vm) {
            true => Ok(ExecEnd::Killed),
            false => Ok(state.end.unwrap_or(ExecEnd::Exited(0))),
        }
    }

    fn kill(&self, vm: &VmName) {
        let _ = self.record(Call::Kill(vm.clone()));
        self.lock().killed.insert(vm.clone());
        self.changed.notify_all();
    }

    fn collect(&self, vm: &VmName, tail_bytes: usize) -> Result<Collected, VmError> {
        self.record(Call::Collect(vm.clone(), tail_bytes))?;
        let state = self.lock();
        let end = |bytes: &[u8]| bytes[bytes.len().saturating_sub(tail_bytes)..].to_vec();
        let (stdout, stderr) = (&state.output.stdout, &state.output.stderr);
        Ok(Collected {
            stdout: end(stdout),
            stderr: end(stderr),
            truncated: stdout.len().max(stderr.len()) > tail_bytes,
        })
    }

    fn stop(&self, vm: &VmName) -> Result<(), VmError> {
        self.record(Call::Stop(vm.clone()))?;
        self.lock().stopped.insert(vm.clone());
        self.changed.notify_all();
        Ok(())
    }

    fn destroy(&self, vm: &VmName) -> Result<(), VmError> {
        self.record(Call::Destroy(vm.clone()))?;
        let mut state = self.lock();
        state.live.remove(vm);
        state.clones.remove(vm);
        state.killed.remove(vm);
        state.finished.remove(vm);
        state.stopped.remove(vm);
        self.changed.notify_all();
        Ok(())
    }

    fn leftovers(&self) -> Result<Vec<VmName>, VmError> {
        let _ = self.record(Call::Leftovers);
        let state = self.lock();
        match state.list_fails {
            true => Err(VmError::List("the fake was told to fail".to_owned())),
            false => Ok(state.clones.iter().cloned().collect()),
        }
    }
}
