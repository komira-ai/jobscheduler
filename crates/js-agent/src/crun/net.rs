//! The crun runtime's side of egress: the port it wires each container's network through
//! ([`EgressPort`]: [`Egress`] on a node, a fake in a test), the start of that port from the
//! agent's `--egress*` flags ([`start_egress`]), and the steps the runtime takes around a
//! start.
//!
//! The order is the one [`crate::egress`] fixes: [`EgressPort::check`] before every create
//! (`Runtime::check_egress`, which the lifecycle calls and answers `egress-unchecked`), the
//! create, `Oci::start`, `Oci::container_pid`, [`EgressPort::attach`], and
//! [`Wired::release`] once the run has ended or is removed. An agent in mode none has no
//! port, and none of this runs: its containers have loopback only, as before.

use std::sync::Arc;
use std::time::Duration;

use js_contain::container::ContainerSpec;
use js_contain::oci::Oci;
use js_contain::running::Running;

use crate::egress::{Attachment, Config, Egress, EgressError, Mode, ROUTE_WAIT};
use crate::lifecycle::RuntimeError;

/// How long the runtime pauses between two asks for a started container's pid.
const PID_PAUSE: Duration = Duration::from_millis(10);

/// What the crun runtime asks of the host's egress. [`Egress`] is the one an agent runs.
pub trait EgressPort: std::fmt::Debug + Send + Sync {
    /// Whether the host's firewall is as it was set up. Asked before every create.
    ///
    /// # Errors
    ///
    /// What is wrong, as [`Egress::check`] says it.
    fn check(&self) -> Result<(), EgressError>;

    /// Wires the network namespace of the container `run` (the run's container name, used
    /// in errors) whose first process is `pid`.
    ///
    /// # Errors
    ///
    /// As [`Egress::attach`]; nothing it made is left.
    fn attach(&self, run: &str, pid: u32) -> Result<Box<dyn Wired>, EgressError>;
}

/// One run's wiring, which [`Wired::release`] takes down.
pub trait Wired: Send {
    /// Takes the run's wiring down.
    ///
    /// # Errors
    ///
    /// As [`Attachment::release`].
    fn release(self: Box<Self>) -> Result<(), EgressError>;
}

impl EgressPort for Egress {
    fn check(&self) -> Result<(), EgressError> {
        Egress::check(self)
    }

    fn attach(&self, run: &str, pid: u32) -> Result<Box<dyn Wired>, EgressError> {
        let attachment = Egress::attach(self, run, pid)?;
        Ok(Box::new(attachment))
    }
}

impl Wired for Attachment {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        Attachment::release(*self)
    }
}

/// The port of the host's `egress`, for the runtime opened from a plan that carries it.
pub(crate) fn port_of(egress: &Egress) -> Arc<dyn EgressPort> {
    Arc::new(egress.clone())
}

/// The egress of an agent whose `--egress*` flags are `config`, started by `start`
/// ([`Egress::start`] on a node). Mode none starts nothing and gives none: the runtime then
/// calls no port, and a job's container has loopback only.
///
/// # Errors
///
/// What `start` returned.
pub fn start_egress<P>(
    config: &Config,
    start: impl FnOnce(Config) -> Result<P, EgressError>,
) -> Result<Option<P>, EgressError> {
    match config.mode() {
        Mode::None => Ok(None),
        Mode::AllowList => start(config.clone()).map(Some),
    }
}

/// [`EgressPort::check`] of `egress`, for `Runtime::check_egress`; nothing to check without
/// one.
pub(super) fn check(egress: Option<&dyn EgressPort>) -> Result<(), RuntimeError> {
    match egress {
        Some(egress) => egress
            .check()
            .map_err(|why| RuntimeError::new(why.to_string())),
        None => Ok(()),
    }
}

/// The wiring of the started container of `spec`, which `running` runs, by `egress`; none
/// without one. The container's pid is asked for until `crun` names one (right after
/// `Oci::start` the container may not be made yet), and the asks end without one when
/// `crun state` answers with no usable pid, when `crun run` has ended, or when [`ROUTE_WAIT`]
/// (an allow-list's `Config::route_wait`, the only egress with a port) has passed on
/// `now_ms`: the container's init gives up its wait for the route then.
///
/// # Errors
///
/// No pid, or the wiring failed; nothing of the wiring is left. The container's init holds
/// the job until the route, which never came: the caller removes the container.
pub(super) fn attach(
    egress: Option<&dyn EgressPort>,
    oci: &Oci,
    (spec, running): (&ContainerSpec, &Running),
    now_ms: fn() -> u64,
) -> Result<Option<Box<dyn Wired>>, RuntimeError> {
    let Some(egress) = egress else {
        return Ok(None);
    };
    let failed = |why: String| RuntimeError::new(format!("wire the container's network: {why}"));
    let pid = pid_of(oci, spec, running, now_ms).map_err(failed)?;
    let wired = egress.attach(&spec.name, pid);
    wired.map(Some).map_err(|why| failed(why.to_string()))
}

/// The host pid of the first process of the container of `spec`; see [`attach`].
fn pid_of(
    oci: &Oci,
    spec: &ContainerSpec,
    running: &Running,
    now_ms: fn() -> u64,
) -> Result<u32, String> {
    let deadline = now_ms().saturating_add(ROUTE_WAIT.as_secs().saturating_mul(1000));
    loop {
        let why = match oci.container_pid(spec) {
            Ok(pid) => return Ok(pid),
            Err(why) if answered(&why) => return Err(why),
            Err(why) => why,
        };
        if running.try_end().is_some() {
            return Err(format!(
                "crun run ended before the container had a pid: {why}"
            ));
        }
        if now_ms() >= deadline {
            return Err(format!(
                "the container had no pid within {ROUTE_WAIT:?}: {why}"
            ));
        }
        std::thread::sleep(PID_PAUSE);
    }
}

/// Whether `why`, an error of `Oci::container_pid`, is `crun`'s answer that the container is
/// there and names no usable pid (its state was read): asking again does not change that. Any
/// other error (`crun state` failed: the container is not made yet) is asked again.
fn answered(why: &str) -> bool {
    why.starts_with("crun state: no pid in") || why.ends_with("is not a pid")
}

/// Takes the wiring of the container `name` down, once: it is taken out of `wired`, so a
/// second call does nothing. A failure is logged and does not stop the caller (a removal
/// goes on, an end is still read): the job end of the pair goes with the container's
/// namespace, and what is left on the host is removed by the next start of the egress.
pub(super) fn release(name: &str, wired: &mut Option<Box<dyn Wired>>) {
    if let Some(wired) = wired.take()
        && let Err(why) = wired.release()
    {
        tracing::warn!(lease = name, "release the network of the run: {why}");
    }
}
