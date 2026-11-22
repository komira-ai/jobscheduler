//! A runtime whose create of the runs a test names is under way (`Pending`) until the test
//! releases it, as a crun create on its pool is, and whose removal of the containers a test
//! names is under way until the test releases it, as a crun removal on its pool is; every
//! other call, and every other run, is the wrapped [`FakeRuntime`]'s. Nothing here waits: the
//! create is the wrapped runtime's `create_and_start`, and the removal its `remove`, made at
//! the step after the release.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    CheckHook, Created, Found, FsPrepared, JobFs, Lifecycle, NativeRefusal, Prepared, Removal,
    RunHandle, RunSpec, Runtime, RuntimeError, Status, Ticket, Wake, Whereabouts,
};
use js_agent_wire::{
    HostPort, JobUsage, LogReport, LogTail, NativeSpec, NetworkCeiling, PortRange,
};

use super::fake::{FakeRuntime, ManualClock};
use super::{Harness, hook};

/// What a removal that cannot delete the run's account says: the helper's refusal.
pub const ACCOUNT_HELD: &str =
    "user-delete: kbf-mac-session refused: not shown gone: a record has uid 612";

/// What the test and the runtime share.
#[derive(Default)]
struct Gate {
    /// The runs whose create is to stay pending.
    held: BTreeSet<String>,
    /// The runs whose create was released.
    released: BTreeSet<String>,
    /// The creates begun and not yet answered, by run.
    under_way: BTreeMap<String, RunSpec>,
    /// The runs `begin_create` was called for, in order.
    begun: Vec<String>,
    /// How many times `poll_create` was called, by run.
    polls: BTreeMap<String, u32>,
    /// The containers whose removal is to stay pending.
    removals_held: BTreeSet<String>,
    /// The containers whose removal was released.
    removals_released: BTreeSet<String>,
    /// How many times `removing` was called, by container.
    removal_asks: BTreeMap<String, u32>,
    /// The clock a removal moves and by how much, as a `crun delete` and the removal of a
    /// lease's directories take on a busy disk.
    remove_takes: Option<(ManualClock, u64)>,
    /// The accounts held that the next removals answer, one for each: each removal ends the
    /// run and fails as a native run's does when the helper cannot delete its account.
    accounts_held: VecDeque<usize>,
    /// How many accounts the runtime holds now (`Runtime::held_accounts`).
    held_now: usize,
    /// When set, every start fails as a native one whose discard left an account held, the
    /// count after it being this.
    start_holds: Option<usize>,
}

/// The test's hand on the gate of a [`Deferring`] runtime.
#[derive(Clone, Default)]
pub struct Control(Rc<RefCell<Gate>>);

impl Control {
    /// From now on the create of `run_id` stays pending until [`Control::release`].
    pub fn hold(&self, run_id: &str) {
        self.0.borrow_mut().held.insert(run_id.to_owned());
    }

    /// The create of `run_id` is done at the next ask: the wrapped runtime makes it then.
    pub fn release(&self, run_id: &str) {
        self.0.borrow_mut().released.insert(run_id.to_owned());
    }

    /// The runs whose create was begun, in order.
    pub fn begun(&self) -> Vec<String> {
        self.0.borrow().begun.clone()
    }

    /// How often the lifecycle asked about the create of `run_id`.
    pub fn polls(&self, run_id: &str) -> u32 {
        self.0.borrow().polls.get(run_id).copied().unwrap_or(0)
    }

    /// From now on the removal of the container `id` (the fake's handle, `c1` for the first
    /// container) stays pending until [`Control::release_removal`].
    pub fn hold_removal(&self, id: &str) {
        self.0.borrow_mut().removals_held.insert(id.to_owned());
    }

    /// The removal of the container `id` is done at the next ask: the wrapped runtime
    /// removes it then.
    pub fn release_removal(&self, id: &str) {
        self.0.borrow_mut().removals_released.insert(id.to_owned());
    }

    /// Every removal the wrapped runtime makes moves `clock` by `ms`, on the thread that
    /// asks for it.
    pub fn remove_takes(&self, clock: &ManualClock, ms: u64) {
        self.0.borrow_mut().remove_takes = Some((clock.clone(), ms));
    }

    /// From now on the next removals each end the run's container and fail as a native run's
    /// removal does when the helper cannot delete the run's account: the first with `held`
    /// accounts held (itself among them), the second with the second count, and so on.
    pub fn hold_accounts(&self, held: &[usize]) {
        self.0
            .borrow_mut()
            .accounts_held
            .extend(held.iter().copied());
    }

    /// The runtime holds `held` accounts from now on, as one whose reclaim or boot reset
    /// found some does, whatever the lifecycle did.
    pub fn set_held(&self, held: usize) {
        self.0.borrow_mut().held_now = held;
    }

    /// From now on every start fails as a native one whose discard left an account held, the
    /// runtime then holding `held` accounts.
    pub fn fail_starts_holding(&self, held: usize) {
        self.0.borrow_mut().start_holds = Some(held);
    }

    /// How often the lifecycle asked about the removal of the container `id`.
    pub fn removal_asks(&self, id: &str) -> u32 {
        self.0.borrow().removal_asks.get(id).copied().unwrap_or(0)
    }
}

/// The name the direct runner gives the lease of run `run_id` (`js-<agent tag>-<run id>`),
/// which is also its control group's: what [`Deferring`] answers for a create it holds.
pub fn lease(run_id: &str) -> String {
    format!("js-0123456789abcdef-{run_id}")
}

/// The wrapped runtime, with the creates the gate holds.
pub struct Deferring {
    inner: FakeRuntime,
    gate: Control,
}

impl Deferring {
    /// A runtime around `inner` (a handle on the same state), and the control of its gate.
    pub fn around(inner: FakeRuntime) -> (Self, Control) {
        let gate = Control::default();
        let runtime = Self {
            inner,
            gate: gate.clone(),
        };
        (runtime, gate)
    }

    /// A harness whose lifecycle runs over a [`Deferring`] runtime around the harness's
    /// fake, and the control of its gate.
    pub fn harness(name: &str) -> (Harness, Control) {
        Self::harness_checked(name, None)
    }

    /// As [`Deferring::harness`], with `checks` as the post-job checks instead of the
    /// scripted ones when given.
    pub fn harness_checked(name: &str, checks: Option<Box<dyn CheckHook>>) -> (Harness, Control) {
        let mut h = Harness::new(name);
        let (runtime, gate) = Self::around(h.fake.clone());
        let scratch = h.dir.path("scratch");
        let checks = checks.unwrap_or_else(|| hook(&h.log, &h.fake, &scratch, &[]));
        let (life, _) = Lifecycle::adopt(
            h.config.clone(),
            Box::new(runtime),
            checks,
            Box::new(h.clock.clone()),
        )
        .unwrap();
        h.life = life;
        (h, gate)
    }
}

impl Runtime for Deferring {
    fn begin_create(&mut self, spec: &RunSpec) -> Created {
        let mut gate = self.gate.0.borrow_mut();
        gate.begun.push(spec.run_id.clone());
        if gate.held.contains(&spec.run_id) {
            gate.under_way.insert(spec.run_id.clone(), spec.clone());
            return Created::Pending;
        }
        drop(gate);
        Created::Done(self.create_and_start(spec))
    }

    fn poll_create(&mut self, run_id: &str) -> Created {
        let mut gate = self.gate.0.borrow_mut();
        *gate.polls.entry(run_id.to_owned()).or_default() += 1;
        let ready = gate.released.contains(run_id);
        match (ready, gate.under_way.remove(run_id)) {
            (true, Some(spec)) => {
                drop(gate);
                Created::Done(self.inner.create_and_start(&spec))
            }
            (_, Some(spec)) => {
                gate.under_way.insert(run_id.to_owned(), spec);
                Created::Pending
            }
            (_, None) => Created::Done(Err(RuntimeError::new(format!(
                "no create of {run_id} is under way"
            )))),
        }
    }

    /// The direct runner's name of the run's lease ([`lease`]) while its create is held.
    fn creating(&self, run_id: &str) -> Option<RunHandle> {
        let gate = self.gate.0.borrow();
        gate.under_way
            .contains_key(run_id)
            .then(|| RunHandle(lease(run_id)))
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let mut gate = self.gate.0.borrow_mut();
        if let Some(held) = gate.start_holds {
            gate.held_now = held;
            return Err(RuntimeError::new("user-create: the start failed"));
        }
        drop(gate);
        self.inner.create_and_start(spec)
    }

    fn held_accounts(&self) -> usize {
        self.gate.0.borrow().held_now
    }

    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.inner.prepare(image, waited)
    }
    fn set_wake(&mut self, wake: Wake) {
        self.inner.set_wake(wake);
    }
    fn start_steps(&mut self, id: &RunHandle) -> Vec<(&'static str, u64)> {
        self.inner.start_steps(id)
    }
    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        self.inner.poll(id)
    }
    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.inner.stop(id, grace)
    }
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        if let Some((clock, ms)) = &self.gate.0.borrow().remove_takes {
            clock.advance(*ms);
        }
        let removed = self.inner.remove(id);
        let mut gate = self.gate.0.borrow_mut();
        match gate.accounts_held.pop_front() {
            Some(held) if removed.is_ok() => {
                gate.held_now = held;
                Err(RuntimeError::account_held(ACCOUNT_HELD, held))
            }
            _ => removed,
        }
    }
    /// Pending while the gate holds the removal of `id`, and the wrapped runtime's `remove`
    /// otherwise.
    fn removing(&mut self, id: &RunHandle) -> Removal {
        let mut gate = self.gate.0.borrow_mut();
        *gate.removal_asks.entry(id.0.clone()).or_default() += 1;
        if gate.removals_held.contains(&id.0) && !gate.removals_released.contains(&id.0) {
            return Removal::Pending;
        }
        drop(gate);
        Removal::Done(self.remove(id))
    }
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.inner.list_labelled()
    }
    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        self.inner.log_tail(id)
    }
    fn logs_report(&mut self, id: &RunHandle) -> Option<LogReport> {
        self.inner.logs_report(id)
    }
    fn usage(&mut self, id: &RunHandle) -> Option<JobUsage> {
        self.inner.usage(id)
    }
    fn check_native(&self, native: &NativeSpec) -> Result<(), NativeRefusal> {
        self.inner.check_native(native)
    }
    fn check_env(&self, env: &[(String, String)]) -> Result<(), NativeRefusal> {
        self.inner.check_env(env)
    }
    fn check_image(&self) -> Result<(), NativeRefusal> {
        self.inner.check_image()
    }
    fn runs_image_runs(&self) -> bool {
        self.inner.runs_image_runs()
    }
    fn supports_gpus(&self) -> bool {
        self.inner.supports_gpus()
    }
    fn check_egress(&self) -> Result<(), RuntimeError> {
        self.inner.check_egress()
    }
    fn network_ceiling(&self) -> NetworkCeiling {
        self.inner.network_ceiling()
    }
    fn host_ports(&self) -> Vec<PortRange> {
        self.inner.host_ports()
    }
    fn host_port_lost(&self, id: &RunHandle) -> Option<String> {
        self.inner.host_port_lost(id)
    }
    fn layered_rootfs(&self) -> bool {
        self.inner.layered_rootfs()
    }
    fn check_job_fs(&self, fs: &JobFs, host_ports: &[HostPort]) -> Result<(), NativeRefusal> {
        self.inner.check_job_fs(fs, host_ports)
    }
    fn check_layer_image(&self, image: &str) -> Result<(), String> {
        self.inner.check_layer_image(image)
    }
    fn binds_named_mounts(&self) -> bool {
        self.inner.binds_named_mounts()
    }
    fn resolve_fs(&mut self, run_id: &str, image: &str, fs: &JobFs, now_ms: u64) -> FsPrepared {
        self.inner.resolve_fs(run_id, image, fs, now_ms)
    }
    fn assemble_fs(&mut self, run_id: &str, now_ms: u64) -> FsPrepared {
        self.inner.assemble_fs(run_id, now_ms)
    }
    fn discard_fs(&mut self, run_id: &str) -> Result<(), RuntimeError> {
        self.inner.discard_fs(run_id)
    }
    fn mounts_of(&self, run_id: &str) -> Option<PathBuf> {
        self.inner.mounts_of(run_id)
    }
    fn input_failure(&self, id: &RunHandle) -> Option<String> {
        self.inner.input_failure(id)
    }
    fn ends_held(&self, id: &RunHandle) -> bool {
        self.inner.ends_held(id)
    }
    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        self.inner.cgroup(id)
    }
    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        self.inner.lease_dir(run_id)
    }
    fn places_in_cgroup(&self) -> bool {
        self.inner.places_in_cgroup()
    }
    fn whereabouts(&mut self, id: &RunHandle) -> Whereabouts {
        self.inner.whereabouts(id)
    }
}
