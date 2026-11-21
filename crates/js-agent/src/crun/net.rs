//! The crun runtime's side of egress: the port it wires each container's network through
//! ([`EgressPort`]: [`Egress`] on a node, a fake in a test), the start of that port from the
//! agent's `--egress*` flags ([`start_egress`]), the steps the runtime takes around a start,
//! and the offer of a run's host ports ([`Forwarder`]) at every level, its own or an adopted
//! run's, with the label that records them.
//!
//! The order is the one [`crate::egress`] fixes: [`EgressPort::check`] before every create of a
//! wired job (`Runtime::check_egress`, which the lifecycle calls, for no `none` job, and answers
//! `egress-unchecked`), the
//! create, `Oci::start_supervised`, `Oci::container_pid`, [`EgressPort::attach`] with the job's
//! level and declared ports, the offer of the run's host ports, and [`Wired::release`] and the
//! offer's close once the run has ended or is removed. An agent in mode none has no port, and
//! none of the egress runs: its containers have loopback only, as
//! before. Under a ceiling
//! a `none` job is not attached either: its bundle has the loopback resolver and no route
//! wait ([`create`]), and its container keeps loopback only.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{HostPort, NetworkCeiling, PortProtocol};
use js_contain::container::ContainerSpec;
use js_contain::oci::words::{crun_words, init_words};
use js_contain::oci::{Oci, Supervised};
use js_types::{JobNetwork, PortMap};

use crate::egress::{Attachment, Config, Egress, EgressError, Mode, ROUTE_WAIT};
use crate::host_ports::{Forwarder, OfferError, Offered};
use crate::lifecycle::{RuntimeError, label};

// The container's init waits for its listeners as long as for its route, and the runtime asks
// for the pid that long.
const _: () = assert!(ROUTE_WAIT.as_secs() == js_contain::oci::LISTEN_WAIT_SECS);

/// How long the runtime pauses between two asks for a started container's pid.
const PID_PAUSE: Duration = Duration::from_millis(10);

/// What the crun runtime asks of the host's egress. [`Egress`] is the one an agent runs.
pub trait EgressPort: std::fmt::Debug + Send + Sync {
    /// Whether the host's firewall is as it was set up. Asked before every create of a wired
    /// job (`allow-list` or `open`).
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

    /// Takes over the wiring an earlier process of the agent made for the container `run`
    /// whose first process is `pid`, for a job that asked for `level` and declared `ports`;
    /// the wiring is left as it is.
    ///
    /// # Errors
    ///
    /// As [`Egress::adopt`]; nothing is taken.
    fn adopt(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError>;

    /// Removes what an earlier process left and no run took over; called once, after the
    /// runs are adopted.
    ///
    /// # Errors
    ///
    /// As [`Egress::settle`].
    fn settle(&self) -> Result<(), EgressError>;
}

/// One run's wiring, which [`Wired::release`] takes down and [`Wired::leave`] leaves to the
/// next process of the agent.
pub trait Wired: Send {
    /// Takes the run's wiring down.
    ///
    /// # Errors
    ///
    /// As [`Attachment::release`].
    fn release(self: Box<Self>) -> Result<(), EgressError>;

    /// Leaves the run's wiring as it is, for the next process of the agent to take over: the
    /// run goes on after this process ends.
    fn leave(self: Box<Self>);
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

    fn adopt(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        let attachment = Egress::adopt(self, run, pid, level, ports)?;
        Ok(Box::new(attachment))
    }

    fn settle(&self) -> Result<(), EgressError> {
        Egress::settle(self)
    }
}

impl Wired for Attachment {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        Attachment::release(*self)
    }

    fn leave(self: Box<Self>) {
        Attachment::leave(*self);
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

/// A run's network: the level its job was admitted at, the ports it declared and its host
/// ports, its wiring from its start until it is released, and the offer of its host ports
/// (or why they could not be offered again after a restart).
pub(super) struct Net {
    level: JobNetwork,
    ports: Vec<PortMap>,
    host_ports: Vec<HostPort>,
    wired: Option<Box<dyn Wired>>,
    offered: Option<Offered>,
    lost: Option<String>,
}

impl Net {
    /// The network of a run admitted at `level` with the declared `ports` and the
    /// `host_ports` it reaches, not wired or offered yet.
    pub(super) fn asked(level: JobNetwork, ports: Vec<PortMap>, host_ports: Vec<HostPort>) -> Self {
        Self {
            level,
            ports,
            host_ports,
            wired: None,
            offered: None,
            lost: None,
        }
    }

    /// Wires the started container of `spec`, which `running` runs, by `egress` ([`attach`]),
    /// and offers it its host ports by `forwarder`, at every level; keeps both. The
    /// container's pid is asked for once, and only when there is something to do.
    ///
    /// # Errors
    ///
    /// As [`attach`], or the host ports could not be offered.
    pub(super) fn attach(
        &mut self,
        (egress, forwarder): (Option<&dyn EgressPort>, &Forwarder),
        oci: &Oci,
        started: (&ContainerSpec, &Supervised),
        now_ms: fn() -> u64,
    ) -> Result<(), RuntimeError> {
        // `stage` refused a wired run with no egress before anything was made (`port_for`).
        let egress = egress.filter(|_| !self.level.is_none());
        if egress.is_none() && self.host_ports.is_empty() {
            return Ok(());
        }
        let failed =
            |why: String| RuntimeError::new(format!("wire the container's network: {why}"));
        let pid = pid_of(oci, started.0, started.1, now_ms).map_err(failed)?;
        self.wired = attach(egress, (&started.0.name, pid), (self.level, &self.ports))?;
        self.offered = offer(forwarder, pid, &self.host_ports)
            .map_err(|why| RuntimeError::new(format!("offer the host ports: {why}")))?;
        Ok(())
    }

    /// Takes over, by `egress`, the wiring an earlier process made for the adopted run's
    /// container `name`, whose first process is `pid` as `crun state` named it, for a run whose job
    /// was wired (the caller asks for no `none` run). A container whose state `crun` read
    /// and that names no usable pid is ending: its network ends with its namespace, and
    /// nothing is taken over (what is left on the host goes with the egress's settle). When
    /// `crun state` failed, the container may have ended and been deleted (its result then
    /// comes from its supervisor's record) or may still run: nothing is taken over and the run
    /// is marked lost ([`Net::lost`]), so the lifecycle stops it and reports its end, rather
    /// than leave a job running whose wiring the settle removes, or drop the result of one
    /// that ended.
    ///
    /// # Errors
    ///
    /// The wiring is not the run's ([`Egress::adopt`]).
    pub(super) fn adopt(
        &mut self,
        egress: &dyn EgressPort,
        name: &str,
        pid: &Result<u32, String>,
    ) -> Result<(), String> {
        let pid = match pid {
            Err(why) if answered(why) => return Ok(()),
            Err(why) => {
                self.lost = Some(format!(
                    "its network cannot be taken over: find the container's pid: {why}"
                ));
                return Ok(());
            }
            Ok(pid) => *pid,
        };
        let wired = egress.adopt(name, pid, self.level, &self.ports);
        self.wired = Some(wired.map_err(|why| why.to_string())?);
        Ok(())
    }

    /// Leaves the run's wiring as it is ([`Wired::leave`]): for a run that goes on after
    /// this process ends.
    pub(super) fn leave(&mut self) {
        if let Some(wired) = self.wired.take() {
            wired.leave();
        }
    }

    /// Offers the run, adopted from an earlier process, its host ports again, in the
    /// network namespace of `pid` (its container's first process, as `crun state` named it): a
    /// run that cannot be offered them is marked lost ([`Net::lost`]), `crun state` failing
    /// among the reasons. A container whose state `crun` read and that names no usable pid is
    /// ending (its sidecar says it ran, so it was made), and nothing is offered.
    pub(super) fn offer_again(&mut self, forwarder: &Forwarder, pid: Result<u32, String>) {
        (self.offered, self.lost) = match pid {
            _ if self.host_ports.is_empty() => return,
            Err(why) if answered(&why) => return,
            Err(why) => (None, Some(format!("find the container's pid: {why}"))),
            Ok(pid) => match offer(forwarder, pid, &self.host_ports) {
                Ok(offered) => (offered, None),
                Err(why) => (None, Some(why.to_string())),
            },
        };
    }

    /// Why the run's network could not be taken over, or its host ports offered again, after a
    /// restart; `None` when they were, or it has none.
    pub(super) fn lost(&self) -> Option<String> {
        self.lost.clone()
    }

    /// Takes the wiring of the container `name` down ([`release`]) and closes its host
    /// ports.
    pub(super) fn release(&mut self, name: &str) {
        release(name, &mut self.wired);
        self.offered = None;
    }
}

/// The offer of `host_ports` to the container whose first process is `pid`; none when there
/// are none.
fn offer(
    forwarder: &Forwarder,
    pid: u32,
    host_ports: &[HostPort],
) -> Result<Option<Offered>, OfferError> {
    match host_ports.is_empty() {
        true => Ok(None),
        false => forwarder.offer(pid, host_ports).map(Some),
    }
}

/// The value of the label [`label::HOST_PORTS`] for `host_ports`; none when there are none.
pub(super) fn host_ports_label(host_ports: &[HostPort]) -> Option<String> {
    let each = host_ports.iter().map(|port| match port.job_port() {
        job if job == port.host_port => port.host_port.to_string(),
        job => format!("{}:{job}", port.host_port),
    });
    let text: Vec<String> = each.collect();
    (!text.is_empty()).then(|| text.join(","))
}

/// The host ports the label [`label::HOST_PORTS`] of `labels` records: none without it.
///
/// # Errors
///
/// The label is there and is not what [`host_ports_label`] writes.
pub(super) fn recorded_host_ports(
    labels: &BTreeMap<String, String>,
) -> Result<Vec<HostPort>, String> {
    let Some(text) = labels.get(label::HOST_PORTS) else {
        return Ok(Vec::new());
    };
    let number = |text: &str| text.parse::<u16>().ok().filter(|port| *port != 0);
    let port = |entry: &str| {
        let (host, job) = entry.split_once(':').unwrap_or((entry, entry));
        Some(HostPort {
            host_port: number(host)?,
            container_port: Some(number(job)?),
            protocol: PortProtocol::Tcp,
        })
    };
    let ports: Option<Vec<HostPort>> = text.split(',').map(port).collect();
    ports.ok_or_else(|| {
        format!(
            "the label {} is not a list of host ports",
            label::HOST_PORTS
        )
    })
}

/// The labels that record the network of a run admitted at `level` with the declared
/// `ports`, so a restarted agent takes its wiring over: [`label::NETWORK`] always, and
/// [`label::NETWORK_PORTS`] when it declared ports.
pub(super) fn network_labels(level: JobNetwork, ports: &[PortMap]) -> Vec<(String, String)> {
    let mut labels = vec![(label::NETWORK.to_owned(), level.name().to_owned())];
    if !ports.is_empty() {
        let each: Vec<String> = ports
            .iter()
            .map(|port| format!("{}:{}", port.host, port.container))
            .collect();
        labels.push((label::NETWORK_PORTS.to_owned(), each.join(",")));
    }
    labels
}

/// The network the labels [`label::NETWORK`] and [`label::NETWORK_PORTS`] of `labels` record:
/// none without the first (a run an agent that did not record it started).
///
/// # Errors
///
/// A label is there and is not what [`network_labels`] writes.
pub(super) fn recorded_network(
    labels: &BTreeMap<String, String>,
) -> Result<Option<(JobNetwork, Vec<PortMap>)>, String> {
    let Some(level) = labels.get(label::NETWORK) else {
        return Ok(None);
    };
    let unreadable = |key: &str| format!("the label {key} is not what the agent writes");
    let level: JobNetwork = level.parse().map_err(|_| unreadable(label::NETWORK))?;
    let Some(text) = labels.get(label::NETWORK_PORTS) else {
        return Ok(Some((level, Vec::new())));
    };
    let number = |text: &str| text.parse::<u16>().ok().filter(|port| *port != 0);
    let port = |entry: &str| {
        let (host, container) = entry.split_once(':')?;
        Some(PortMap {
            host: number(host)?,
            container: number(container)?,
        })
    };
    let ports: Option<Vec<PortMap>> = text.split(',').map(port).collect();
    let ports = ports.ok_or_else(|| unreadable(label::NETWORK_PORTS))?;
    Ok(Some((level, ports)))
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

/// The wiring of the container `name`, whose first process is `pid`, by `egress`, for a job
/// admitted at `level` that declared `ports`; none without an egress, which is how a `none`
/// job comes (a wired level with no egress never gets here: the runtime refused it before it
/// made the container, [`port_for`]).
///
/// The pid is asked for ([`pid_of`]) until `crun` names one (right after
/// `Oci::start_supervised` the container may not be made yet), and the asks end without one
/// when `crun state` answers with no usable pid (or names one too late), when `crun run` has
/// ended, or when [`ROUTE_WAIT`] (a wired ceiling's `Config::route_wait`, and the init's wait
/// for its listeners) has passed on `now_ms`: the container's init gives up its wait then.
///
/// # Errors
///
/// The wiring failed; nothing of it is left. The container's init holds the job until the
/// route, which never came: the caller removes the container.
pub(super) fn attach(
    egress: Option<&dyn EgressPort>,
    (name, pid): (&str, u32),
    (level, ports): (JobNetwork, &[PortMap]),
) -> Result<Option<Box<dyn Wired>>, RuntimeError> {
    let Some(egress) = egress else {
        return Ok(None);
    };
    let wired = egress.attach(name, pid, level, ports);
    wired
        .map(Some)
        .map_err(|why| RuntimeError::new(format!("wire the container's network: {why}")))
}

/// The host pid of the first process of the container of `spec`; see [`attach`].
pub(super) fn pid_of(
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
pub(super) fn answered(why: &str) -> bool {
    why.starts_with("crun state: no pid in") || why.ends_with("is not a pid")
}

/// Takes the wiring of the container `name` down, once: it is taken out of `wired`, so a
/// second call does nothing. A failure is logged and does not stop the caller (a removal
/// goes on, an end is still read): the job end of the pair goes with the container's
/// namespace, and what is left on the host is removed by `Egress::settle` of the next process,
/// once its runs are adopted.
pub(super) fn release(name: &str, wired: &mut Option<Box<dyn Wired>>) {
    if let Some(wired) = wired.take()
        && let Err(why) = wired.release()
    {
        tracing::warn!(lease = name, "release the network of the run: {why}");
    }
}
