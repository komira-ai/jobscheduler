//! [`HelperDriver`]: the agent's real [`VmDriver`]. The VMs are made by the signed `js-vmm`
//! binary (the one entitled to run VMs), which the agent starts as `js-vmm lease --store
//! <dir>` through the one spawner, `js_mac_session::disclaim`, one process for each VM, and
//! drives over the line protocol of [`js_vm_lease::protocol`]. The agent links nothing of
//! `js-vmm`: it holds a [`Session`] to each VM's process, and the process ends when the VM is
//! destroyed (its input is closed).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_mac_session::disclaim::Program;
use js_vm_lease::protocol::{Reply, Request, Session};
use js_vm_lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmName, VmSize};

/// A session to a VM helper, and the helper's process when there is one to reap.
pub struct Launched {
    /// The connection.
    pub session: Session,
    /// The process, waited for when the session ends; none for a helper that is no process.
    pub child: Option<Child>,
}

/// How a VM helper is started.
pub trait Launch: Send + Sync {
    /// Starts one.
    ///
    /// # Errors
    ///
    /// It could not be started.
    fn launch(&self) -> io::Result<Launched>;
}

/// `<driver> lease --store <store>`, started through the spawner with its input and output
/// piped and its standard error the agent's.
pub struct HelperLaunch {
    driver: PathBuf,
    store: PathBuf,
}

impl HelperLaunch {
    /// The helper of the `js-vmm` binary `driver` for the VMs of `store`.
    pub fn new(driver: PathBuf, store: PathBuf) -> Self {
        Self { driver, store }
    }
}

impl Launch for HelperLaunch {
    fn launch(&self) -> io::Result<Launched> {
        let args = [
            OsStr::new("lease"),
            OsStr::new("--store"),
            self.store.as_os_str(),
        ];
        let mut program = Program::new(&self.driver, args);
        program.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = program.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or(io::Error::other("no input pipe"))?;
        let output = child
            .stdout
            .take()
            .ok_or(io::Error::other("no output pipe"))?;
        let session = Session::new(Box::new(input), Box::new(BufReader::new(output)));
        Ok(Launched {
            session,
            child: Some(child),
        })
    }
}

/// One VM's helper.
struct Helper {
    session: Arc<Session>,
    child: Option<Child>,
}

/// The driver; see the [module](self).
pub struct HelperDriver {
    launch: Box<dyn Launch>,
    vms: Mutex<BTreeMap<VmName, Helper>>,
}

/// The error of a reply that is not the one `step` of `vm` expects: the driver's own error, or
/// one that names the reply.
fn unexpected(vm: &VmName, step: Step, reply: Reply) -> VmError {
    match reply {
        Reply::Failed { error } => error,
        other => VmError::step(vm, step, format!("the VM helper answered {other:?}")),
    }
}

/// `Ok` for [`Reply::Done`].
fn done(vm: &VmName, step: Step, reply: Reply) -> Result<(), VmError> {
    match reply {
        Reply::Done => Ok(()),
        other => Err(unexpected(vm, step, other)),
    }
}

impl HelperDriver {
    /// A driver whose VM helpers `launch` starts.
    pub fn new(launch: Box<dyn Launch>) -> Self {
        Self {
            launch,
            vms: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<VmName, Helper>> {
        self.vms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session of `vm`'s helper, started when it has none.
    fn session(&self, vm: &VmName, step: Step) -> Result<Arc<Session>, VmError> {
        let mut vms = self.lock();
        if let Some(helper) = vms.get(vm) {
            return Ok(Arc::clone(&helper.session));
        }
        let launched = self.launch.launch().map_err(|why| {
            VmError::step(vm, step, format!("the VM helper did not start: {why}"))
        })?;
        let session = Arc::new(launched.session);
        let helper = Helper {
            session: Arc::clone(&session),
            child: launched.child,
        };
        vms.insert(vm.clone(), helper);
        Ok(session)
    }

    /// Makes the call `request` of `step` on `vm`'s helper.
    fn call(&self, vm: &VmName, step: Step, request: &Request) -> Result<Reply, VmError> {
        let session = self.session(vm, step)?;
        let reply = session.call(request);
        reply.map_err(|why| VmError::step(vm, step, format!("the VM helper: {why}")))
    }
}

/// Ends a helper's session (its input closes, so it ends) and reaps its process.
fn end(helper: Helper) {
    drop(helper.session);
    if let Some(mut child) = helper.child {
        let _ = child.wait();
    }
}

impl VmDriver for HelperDriver {
    fn clone_base(&self, vm: &VmName) -> Result<(), VmError> {
        let reply = self.call(vm, Step::Clone, &Request::Clone { vm: vm.clone() })?;
        done(vm, Step::Clone, reply)
    }

    fn boot(&self, vm: &VmName, size: VmSize) -> Result<(), VmError> {
        let request = Request::Boot {
            vm: vm.clone(),
            size,
        };
        done(vm, Step::Boot, self.call(vm, Step::Boot, &request)?)
    }

    fn wait_ready(&self, vm: &VmName) -> Result<(), VmError> {
        let request = Request::WaitReady { vm: vm.clone() };
        done(vm, Step::Ready, self.call(vm, Step::Ready, &request)?)
    }

    fn run_command(&self, vm: &VmName, request: &ExecRequest) -> Result<ExecEnd, VmError> {
        let request = Request::RunCommand {
            vm: vm.clone(),
            request: request.clone(),
        };
        match self.call(vm, Step::Exec, &request)? {
            Reply::Ended { end } => Ok(end),
            other => Err(unexpected(vm, Step::Exec, other)),
        }
    }

    /// Sends the kill to `vm`'s helper, when it has one, without waiting: the helper acts on
    /// it while the command runs. A VM with no helper yet runs nothing to kill.
    fn kill(&self, vm: &VmName) {
        if let Some(helper) = self.lock().get(vm) {
            let _ = helper.session.send(&Request::Kill { vm: vm.clone() });
        }
    }

    fn collect(&self, vm: &VmName, tail_bytes: usize) -> Result<Collected, VmError> {
        let request = Request::Collect {
            vm: vm.clone(),
            tail_bytes,
        };
        match self.call(vm, Step::Collect, &request)? {
            Reply::Collected { output } => Ok(output),
            other => Err(unexpected(vm, Step::Collect, other)),
        }
    }

    fn stop(&self, vm: &VmName) -> Result<(), VmError> {
        let request = Request::Stop { vm: vm.clone() };
        done(vm, Step::Stop, self.call(vm, Step::Stop, &request)?)
    }

    /// Destroys `vm` and ends its helper, whatever the destroy answered.
    fn destroy(&self, vm: &VmName) -> Result<(), VmError> {
        let request = Request::Destroy { vm: vm.clone() };
        let destroyed = self
            .call(vm, Step::Destroy, &request)
            .and_then(|reply| done(vm, Step::Destroy, reply));
        let helper = self.lock().remove(vm);
        helper.into_iter().for_each(end);
        destroyed
    }

    /// Asks a helper of its own, ended afterwards.
    fn leftovers(&self) -> Result<Vec<VmName>, VmError> {
        let launched = self.launch.launch();
        let launched =
            launched.map_err(|why| VmError::List(format!("the VM helper did not start: {why}")))?;
        let reply = launched.session.call(&Request::Leftovers);
        end(Helper {
            session: Arc::new(launched.session),
            child: launched.child,
        });
        match reply.map_err(|why| VmError::List(format!("the VM helper: {why}")))? {
            Reply::Names { names } => Ok(names),
            Reply::Failed { error } => Err(error),
            other => Err(VmError::List(format!("the VM helper answered {other:?}"))),
        }
    }
}
