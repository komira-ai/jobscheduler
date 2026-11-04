//! The crun runtime's side of egress: the port it wires each container's network through
//! ([`EgressPort`]: [`Egress`] on a node, a fake in a test), the start of that port from the
//! agent's `--egress*` flags ([`start_egress`]), and the steps the runtime takes around a
//! start.
//!
//! The order is the one [`crate::egress`] fixes: [`EgressPort::check`] before every create
//! (`Runtime::check_egress`, which the lifecycle calls and answers `egress-unchecked`), the
//! create, `Oci::start_supervised`, `Oci::container_pid`, [`EgressPort::attach`] with the job's
//! level and declared ports, and [`Wired::release`] once the run has ended or is removed. An
//! agent in mode none has no port, and none of this runs: its containers have loopback only, as
//! before. Under a ceiling
//! a `none` job is not attached either: its bundle has the loopback resolver and no route
//! wait ([`create`]), and its container keeps loopback only.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::NetworkCeiling;
use js_contain::container::ContainerSpec;
use js_contain::oci::words::{crun_words, init_words};
use js_contain::oci::{Oci, Supervised};
use js_types::{JobNetwork, PortMap};

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

    /// The most network a job may ask for: the agent's ceiling ([`Config::ceiling`]).
    fn ceiling(&self) -> NetworkCeiling;

    /// Wires the network namespace of the container `run` (the run's container name, used
    /// in errors) whose first process is `pid`, for a job that asked for `level` and declared
    /// `ports`, each forwarded from the host's port to the job.
    ///
    /// # Errors
    ///
    /// As [`Egress::attach`]; nothing it made is left.
    fn attach(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError>;
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

    fn ceiling(&self) -> NetworkCeiling {
        self.config().ceiling()
    }

    fn attach(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        let attachment = Egress::attach(self, run, pid, level, ports)?;
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
/// ([`Egress::start`] on a node) for the ceiling `allow-list` or `open`. Mode none starts
/// nothing and gives none: the runtime then calls no port, and a job's container has loopback
/// only.
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
        Mode::AllowList | Mode::Open => start(config.clone()).map(Some),
    }
}

/// The ceiling of the runtime whose egress is `egress`: the port's, or `none` without one.
pub(super) fn ceiling(egress: Option<&dyn EgressPort>) -> NetworkCeiling {
    egress.map_or_else(NetworkCeiling::default, |port| port.ceiling())
}

/// Writes the bundle of `container`, whose job was admitted at `level`: a wired job
/// (`allow-list` or `open`) gets the backend's configured resolvers and route wait, a `none`
/// job the loopback resolver and no wait, since nothing will wire it.
///
/// # Errors
///
/// As `Oci::create`.
pub(super) async fn create(
    oci: &Oci,
    container: &ContainerSpec,
    level: JobNetwork,
) -> Result<(), String> {
    match level {
        JobNetwork::None => oci.create_loopback(container).await,
        JobNetwork::AllowList | JobNetwork::Open => oci.create(container).await,
    }
}

/// A run's network: the level its job was admitted at, the ports it declared, and its wiring
/// from its start until it is released.
pub(super) struct Net {
    level: JobNetwork,
    ports: Vec<PortMap>,
    wired: Option<Box<dyn Wired>>,
}

impl Net {
    /// The network of a run admitted at `level` with the declared `ports`, not wired yet.
    pub(super) fn asked(level: JobNetwork, ports: Vec<PortMap>) -> Self {
        Self {
            level,
            ports,
            wired: None,
        }
    }

    /// Wires the started container of `spec`, which `running` runs, by `egress` ([`attach`]),
    /// and keeps the wiring.
    ///
    /// # Errors
    ///
    /// As [`attach`].
    pub(super) fn attach(
        &mut self,
        egress: Option<&dyn EgressPort>,
        oci: &Oci,
        started: (&ContainerSpec, &Supervised),
        now_ms: fn() -> u64,
    ) -> Result<(), RuntimeError> {
        self.wired = attach(egress, oci, started, (self.level, &self.ports), now_ms)?;
        Ok(())
    }

    /// Takes the wiring of the container `name` down ([`release`]).
    pub(super) fn release(&mut self, name: &str) {
        release(name, &mut self.wired);
    }
}

/// The port a run admitted at `level` is wired through: none for a `none` run, `egress` for a
/// wired one. The runtime asks before it makes anything of the run.
///
/// # Errors
///
/// A wired level and no egress (the runtime's ceiling is `none`): the run is refused, never
/// run with less than it asked for.
pub(super) fn port_for(
    egress: Option<&dyn EgressPort>,
    level: JobNetwork,
) -> Result<Option<&dyn EgressPort>, String> {
    match (level, egress) {
        (JobNetwork::None, _) => Ok(None),
        (_, Some(egress)) => Ok(Some(egress)),
        (_, None) => Err(format!(
            "the job asked for {level} and this agent's ceiling is none"
        )),
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

/// The wiring of the started container of `spec`, which `running` runs, by `egress`, for a
/// job admitted at `level` that declared `ports`; none for a `none` job, which is not wired
/// (and whose pid is not asked for). A wired level with no egress never gets here: the
/// runtime refused it before it made the container ([`port_for`]). The container's pid is
/// asked for until `crun` names one (right after `Oci::start_supervised` the container may not
/// be made yet), and the asks end without one when `crun state` answers with no usable pid (or
/// names one too late), when `crun run` has ended, or when [`ROUTE_WAIT`] (a wired ceiling's
/// `Config::route_wait`) has passed on `now_ms`: the container's init gives up its wait for the
/// route then.
///
/// # Errors
///
/// No pid, or the wiring failed; nothing of the wiring is left.
/// The container's init holds the job until the route, which never came: the caller removes
/// the container.
pub(super) fn attach(
    egress: Option<&dyn EgressPort>,
    oci: &Oci,
    (spec, running): (&ContainerSpec, &Supervised),
    (level, ports): (JobNetwork, &[PortMap]),
    now_ms: fn() -> u64,
) -> Result<Option<Box<dyn Wired>>, RuntimeError> {
    let failed = |why: String| RuntimeError::new(format!("wire the container's network: {why}"));
    // `stage` refused a wired run with no egress before anything was made (`port_for`).
    let Some(egress) = egress.filter(|_| !level.is_none()) else {
        return Ok(None);
    };
    let pid = pid_of(oci, spec, running, now_ms).map_err(failed)?;
    let wired = egress.attach(&spec.name, pid, level, ports);
    wired.map(Some).map_err(|why| failed(why.to_string()))
}

/// The host pid of the first process of the container of `spec`; see [`attach`].
fn pid_of(
    oci: &Oci,
    spec: &ContainerSpec,
    running: &Supervised,
    now_ms: fn() -> u64,
) -> Result<u32, String> {
    let deadline = now_ms().saturating_add(ROUTE_WAIT.as_secs().saturating_mul(1000));
    loop {
        let why = match oci.container_pid(spec) {
            Ok(_) if now_ms() >= deadline => {
                return Err(format!(
                    "crun state named the pid after the route wait of {ROUTE_WAIT:?}: the init has given up"
                ));
            }
            Ok(pid) => return Ok(pid),
            Err(why) if answered(&why) => return Err(why),
            Err(why) => why,
        };
        if running.try_end().is_some() {
            return Err(format!(
                "crun run ended before the container had a pid: {why}; {}",
                said(oci, spec)
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

/// What `crun` and the container's init said of the container of `spec`, their error lines
/// joined by `; ` (empty when neither said anything).
fn said(oci: &Oci, spec: &ContainerSpec) -> String {
    let reports = oci.reports(spec);
    let read = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
    let words = [
        crun_words(&read(&reports.log)),
        init_words(&read(&reports.init)),
    ];
    let said: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| !w.is_empty())
        .collect();
    said.join("; ")
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
