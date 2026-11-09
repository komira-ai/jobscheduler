//! [`VmRuntime`]: the [`Runtime`] of VM runs. Each run gets a fresh VM of its own on a
//! thread of its own: the thread clones the base image, boots the clone, waits for the
//! guest, runs the command, collects its output, and reports how it ended; then it stops
//! and destroys the VM, gives back the VM slot and reports that too. The lifecycle's
//! worker waits on a VM in two calls only: [`Runtime::stop`], for at most its grace before it
//! forces the VM off, and [`Runtime::remove`], for the destroy.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use js_agent_wire::limits::{MAX_LOG_STREAM_LEN, MAX_WALL_MS};
use js_agent_wire::{LogTail, refusal};
use js_vm_lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmName, VmSize};

use super::{VmSlot, VmSlots};
use crate::cli_runtime::tail_of;
use crate::facts::Streams;
use crate::lifecycle::{
    Body, Exit, Found, NativeRefusal, RunHandle, RunSpec, Runtime, RuntimeError, Status,
};

/// What a VM's thread reports, in order: how the command ended, then how the VM's end
/// went.
enum Report {
    /// The command's end and its output, or the step that failed before it ended.
    Ended(Result<(ExecEnd, Option<Collected>), VmError>),
    /// The VM is stopped and destroyed (or a step of that failed), and its slot is free.
    Destroyed(Result<(), VmError>),
}

/// One run's VM, as the worker sees it.
struct Run {
    name: VmName,
    /// Set by a stop: the VM's thread runs no command after it (the driver cannot mark a VM
    /// it has not cloned yet).
    cancel: Arc<AtomicBool>,
    reports: Receiver<Report>,
    /// How the run ended, once its thread said.
    ended: Option<Status>,
    /// The tail of the command's output, once collected.
    tail: Option<LogTail>,
    /// How the destroy went, once its thread said.
    destroyed: Option<Result<(), VmError>>,
}

impl Run {
    /// Takes what the thread has reported so far, without waiting. (The thread always
    /// reports the end before it stops and destroys the VM, so a thread that died after that
    /// is found by the removal.)
    fn take_reports(&mut self) {
        while let Ok(report) = self.reports.try_recv() {
            self.note(report);
        }
    }

    fn note(&mut self, report: Report) {
        match report {
            Report::Ended(Ok((end, collected))) => {
                self.ended = Some(Status::Exited(Exit {
                    code: code_of(end),
                    oom_killed: false,
                }));
                self.tail = collected.map(|out| {
                    tail_of(&Streams {
                        stdout: out.stdout,
                        stderr: out.stderr,
                        dropped: out.truncated,
                    })
                });
            }
            Report::Ended(Err(why)) => {
                tracing::warn!(vm = %self.name, "the VM's run failed: {why}");
                self.ended = Some(Status::Gone);
            }
            Report::Destroyed(destroyed) => self.destroyed = Some(destroyed),
        }
    }

    /// The thread is gone without saying that the VM was destroyed (it died in the stop or
    /// the destroy): the VM may be left, which the removal reports.
    fn lost(&mut self) {
        self.ended.get_or_insert(Status::Gone);
        self.destroyed = Some(Err(VmError::step(
            &self.name,
            Step::Destroy,
            "the VM's thread ended before it destroyed the VM",
        )));
    }
}

/// The exit status a command's end reads as: its own status, or 128 plus the signal that
/// ended it (a kill is SIGKILL's, 9), and 124 for the guest's own timeout, as `timeout(1)`.
fn code_of(end: ExecEnd) -> i32 {
    match end {
        ExecEnd::Exited(code) => code,
        ExecEnd::Signaled(signal) => 128_i32.saturating_add(signal),
        ExecEnd::Killed => 128 + 9,
        ExecEnd::TimedOut => 124,
    }
}

/// The size of a run's VM: its CPU booking rounded up to whole CPUs (at least one) and its
/// memory booking rounded up to whole MiB.
fn size_of(spec: &RunSpec) -> VmSize {
    let cpus = spec.cpu_millis.div_ceil(1000).max(1);
    VmSize {
        cpus: u32::try_from(cpus).unwrap_or(u32::MAX),
        memory_mib: spec.memory_bytes.div_ceil(1 << 20),
    }
}

/// The life of one VM, on its own thread; see the [module](self).
fn life(
    driver: &dyn VmDriver,
    (name, size, request): (VmName, VmSize, ExecRequest),
    (slot, cancel): (VmSlot, Arc<AtomicBool>),
    reports: &Sender<Report>,
) {
    let steps = || {
        driver.clone_base(&name)?;
        // A stop that came before the clone existed is the run's own mark: nothing boots.
        if cancel.load(Ordering::SeqCst) {
            return Ok((ExecEnd::Killed, None));
        }
        driver.boot(&name, size)?;
        driver.wait_ready(&name)?;
        let end = driver.run_command(&name, &request)?;
        // Output that cannot be read costs the result its tail, never its end.
        let collected = driver.collect(&name, MAX_LOG_STREAM_LEN);
        if let Err(why) = &collected {
            tracing::warn!(vm = %name, "the VM's output was not collected: {why}");
        }
        Ok((end, collected.ok()))
    };
    // A driver that panics fails the run, and the VM is still stopped and destroyed below.
    let ran = catch_unwind(AssertUnwindSafe(steps))
        .unwrap_or_else(|_| Err(VmError::step(&name, Step::Exec, "the VM driver panicked")));
    let _ = reports.send(Report::Ended(ran));
    // Both always run, whatever came before: a VM is never reused and never left. (A clone
    // of this name that an earlier process left is destroyed with it.)
    let stopped = driver.stop(&name);
    let destroyed = driver.destroy(&name);
    drop(slot);
    let _ = reports.send(Report::Destroyed(stopped.and(destroyed)));
}

/// The runtime of VM runs; see the [module](self).
pub struct VmRuntime {
    driver: Arc<dyn VmDriver>,
    slots: VmSlots,
    runs: BTreeMap<RunHandle, Run>,
}

impl VmRuntime {
    /// A runtime whose VMs `driver` makes, with [`super::VM_SLOTS`] slots.
    pub fn new(driver: Arc<dyn VmDriver>) -> Self {
        Self {
            driver,
            slots: VmSlots::default(),
            runs: BTreeMap::new(),
        }
    }

    /// Whether `id` names a run of this runtime that is not yet removed.
    pub fn holds(&self, id: &RunHandle) -> bool {
        self.runs.contains_key(id)
    }

    /// The slots of this runtime's VMs.
    pub fn slots(&self) -> &VmSlots {
        &self.slots
    }
}

impl Runtime for VmRuntime {
    /// Takes a VM slot and starts the run's VM on a thread of its own; the slot is given
    /// back when the VM is destroyed, however the run ends. Both slots held is
    /// [`super::NoVmSlot`]: the start is refused, never queued.
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let Body::Container { args, .. } = &spec.body else {
            return Err(RuntimeError::new("a VM runs a command, not a native body"));
        };
        let name = VmName::new(&spec.run_id).map_err(|why| RuntimeError::new(why.to_string()))?;
        let id = RunHandle(spec.run_id.clone());
        if self.runs.contains_key(&id) {
            return Err(RuntimeError::new(format!("run {name} already has a VM")));
        }
        let slot = self
            .slots
            .acquire()
            .map_err(|full| RuntimeError::new(full.to_string()))?;
        // The lifecycle ends the run at its wall limit (a stop, then the kill); the guest's
        // own bound is the wire's longest, a backstop only.
        let request = ExecRequest {
            argv: args.clone(),
            env: spec.env.clone(),
            timeout_ms: MAX_WALL_MS,
        };
        let (sender, reports) = channel();
        let (driver, vm) = (
            Arc::clone(&self.driver),
            (name.clone(), size_of(spec), request),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let held = (slot, Arc::clone(&cancel));
        std::thread::spawn(move || life(&*driver, vm, held, &sender));
        let run = Run {
            name,
            cancel,
            reports,
            ended: None,
            tail: None,
            destroyed: None,
        };
        self.runs.insert(id.clone(), run);
        Ok(id)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        let Some(run) = self.runs.get_mut(id) else {
            return Ok(Status::Gone);
        };
        run.take_reports();
        Ok(run.ended.unwrap_or(Status::Running))
    }

    /// Ends the run's command (or, before it runs, keeps it from running): the kill is sent
    /// at once, and a command that has not ended within `grace` (a guest that does not answer)
    /// has its VM forced off, which ends it. The VM is then stopped and destroyed by its
    /// thread.
    ///
    /// # Errors
    ///
    /// The VM could not be forced off.
    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        let Some(run) = self.runs.get_mut(id) else {
            return Ok(());
        };
        run.cancel.store(true, Ordering::SeqCst);
        self.driver.kill(&run.name);
        // The end is waited for whether or not it has already come: the thread reports the
        // end before anything else, so the first report waiting is it, and one already sent
        // is received at once.
        if run.ended.is_none() {
            match run.reports.recv_timeout(grace) {
                Ok(report) => run.note(report),
                Err(_) => {
                    let forced = self.driver.stop(&run.name);
                    forced.map_err(|why| RuntimeError::new(why.to_string()))?;
                }
            }
        }
        Ok(())
    }

    /// Waits until the run's VM is destroyed, and forgets the run. A failed destroy is an
    /// error: something of the VM may be left on the host.
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let Some(mut run) = self.runs.remove(id) else {
            return Ok(());
        };
        while run.destroyed.is_none() {
            match run.reports.recv() {
                Ok(report) => run.note(report),
                Err(_) => run.lost(),
            }
        }
        match run.destroyed {
            Some(Err(why)) => Err(RuntimeError::new(why.to_string())),
            _ => Ok(()),
        }
    }

    /// None: a VM is never adopted. One an earlier process left is destroyed by
    /// [`Runtime::reclaim`].
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        Ok(Vec::new())
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        Ok(self.runs.get(id).and_then(|run| run.tail.clone()))
    }

    /// Stops and destroys every VM whose clone an earlier process of the agent left.
    fn reclaim(&mut self) -> Vec<String> {
        let names = match self.driver.leftovers() {
            Ok(names) => names,
            Err(why) => return vec![format!("vm: {why}")],
        };
        let end = |name: &VmName| self.driver.stop(name).and(self.driver.destroy(name));
        let line = |name: VmName| match end(&name) {
            Ok(()) => format!("vm: destroyed the VM {name} an earlier process left"),
            Err(why) => format!("vm: {why}"),
        };
        names.into_iter().map(line).collect()
    }

    fn check_vm(&self) -> Result<(), NativeRefusal> {
        match self.slots.free() {
            0 => Err(NativeRefusal {
                code: refusal::NO_CAPACITY,
                why: super::NoVmSlot.to_string(),
            }),
            _ => Ok(()),
        }
    }

    fn supports_gpus(&self) -> bool {
        false
    }
}
