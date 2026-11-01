//! The fakes of the hook's ports. They keep their state in two directories, so that a second
//! process (the hook binary the conformance harness starts) sees what a test left and the test
//! sees what the process did:
//!
//! - `world/`, the observable state of the service the hook acts on, which the harness
//!   compares between runs: `units/<unit>` (the unit manager), `link/<name>` (release links),
//!   `lease.json` (the lease server's view of the host) and `state/` (the agent's state root).
//! - `control/`, what the harness must not compare: the clock (`clock`, milliseconds) and the
//!   count of pauses (`ticks`), and the skew of the lease server's clock (`server-skew-ms`).
//!
//! Every change to `world/` passes the [`Gate`] first; a read, and a call that changes nothing,
//! does not. Nothing here sleeps: [`Clock::pause`] moves the fake clock on and counts.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::lifecycle::StateRoot;
use js_agent::update_hook::{
    Clock, CordonView, Cordoned, Gate, HostReading, Lease, LeaseError, Link, MAX_CORDON_SECS,
    Params, PortError, Uncordoned, UnitState, Units, read_link, swap_link,
};
use js_agent_wire::Taint;
use serde::{Deserialize, Serialize};

/// Where the fake clock starts, in milliseconds.
pub const START_MS: u64 = 1_000_000;

/// The runtime a fake hello reports.
pub const RUNTIME: &str = "crun";

/// What the server saved of a host with its binding, and a restart keeps.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    cordon: Option<CordonView>,
    taint: Option<Taint>,
}

/// The fakes' world.
pub struct World {
    world: PathBuf,
    control: PathBuf,
    gate: Box<dyn Gate>,
}

fn io_err(why: &io::Error) -> PortError {
    PortError::Io(why.to_string())
}

fn unreachable(why: &impl std::fmt::Display) -> LeaseError {
    LeaseError::Unreachable(why.to_string())
}

impl World {
    /// The fakes over `world` and `control`, which are made if they are not there.
    ///
    /// # Panics
    /// A directory cannot be made.
    pub fn new(world: &Path, control: &Path, gate: Box<dyn Gate>) -> Self {
        for dir in [world, control] {
            fs::create_dir_all(dir).unwrap();
        }
        for sub in ["units", "link"] {
            fs::create_dir_all(world.join(sub)).unwrap();
        }
        Self {
            world: world.to_owned(),
            control: control.to_owned(),
            gate,
        }
    }

    fn gate(&self) {
        self.gate.point();
    }

    fn unit_file(&self, unit: &str) -> PathBuf {
        self.world.join("units").join(unit)
    }

    fn lease_file(&self) -> PathBuf {
        self.world.join("lease.json")
    }

    fn put_unit(&self, unit: &str, active: &str, sub: &str) -> Result<(), PortError> {
        self.gate();
        fs::write(self.unit_file(unit), format!("{active} {sub}")).map_err(|why| io_err(&why))
    }

    /// Seeds `unit` with the states a test needs.
    ///
    /// # Panics
    /// The file cannot be written.
    pub fn seed_unit(&self, unit: &str, active: &str, sub: &str) {
        fs::write(self.unit_file(unit), format!("{active} {sub}")).unwrap();
    }

    /// The agent's state root in the world (`state/`, mode 0700), made if absent.
    ///
    /// # Panics
    /// The directory cannot be made or opened.
    pub fn state_root(&self) -> StateRoot {
        let dir = self.world.join("state");
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Err(why) if why.kind() != io::ErrorKind::AlreadyExists => panic!("{why}"),
            _ => {}
        }
        StateRoot::open(&dir).unwrap()
    }

    /// Makes the lease server know the host `id`: live, epoch 1, no lease, no cordon.
    ///
    /// # Panics
    /// The file cannot be written.
    pub fn seed_host(&self, id: &str, agent_version: &str) {
        let host = HostReading {
            id: id.to_owned(),
            state: "live".to_owned(),
            epoch: 1,
            session_started_ms: Some(self.server_now_ms()),
            leases: 0,
            lease: None,
            releasing: false,
            mark: None,
            cordon: None,
            taint: None,
            agent_version: Some(agent_version.to_owned()),
            runtime: Some(RUNTIME.to_owned()),
        };
        fs::write(self.lease_file(), serde_json::to_vec(&host).unwrap()).unwrap();
        fs::write(self.control.join("host-id"), id).unwrap();
    }

    /// The world before an update that `params` describes. The release that runs is linked at
    /// `link` exactly as the params spell it, and the host is known to the server with the
    /// version that runs. A first install (neither previous key) has no link and the server
    /// knows no host.
    ///
    /// # Panics
    /// A file cannot be written.
    pub fn seed_update(&self, link: &Path, host: &str, params: &Params) {
        self.register_release(&params.staged_release, &params.target_version);
        if let (Some(release), Some(version)) = (&params.previous_release, &params.previous_version)
        {
            self.register_release(release, version);
        }
        if let Some(release) = &params.previous_release {
            swap_link(&self.link_file(link).unwrap(), release).unwrap();
        }
        if let Some(version) = &params.previous_version {
            self.seed_host(host, version);
        }
    }

    /// Tells the fakes which version the release directory `path` holds, as the params of an
    /// update do: an agent started from a link to `path` says hello with this version.
    ///
    /// # Panics
    /// The file cannot be written.
    pub fn register_release(&self, path: &Path, version: &str) {
        let mut text = fs::read_to_string(self.control.join("releases")).unwrap_or_default();
        text.push_str(&format!("{}\t{version}\n", path.display()));
        fs::write(self.control.join("releases"), text).unwrap();
    }

    /// The server restarts: it forgets the host's session, and the next hello makes it live
    /// again. Until then the host is `awaiting` (epoch 0, no session) and a cordon, an uncordon
    /// and a reading work on it. The cordon and the taint were saved with the host's binding and
    /// are what the awaiting host holds, a cordon only while it has not lapsed.
    ///
    /// # Panics
    /// A file cannot be read, written or removed.
    pub fn forget_host(&self) {
        let host = self.stored().unwrap();
        let saved = Saved {
            cordon: host.cordon,
            taint: host.taint,
        };
        fs::write(self.saved_file(), serde_json::to_vec(&saved).unwrap()).unwrap();
        // An awaiting host has no file of its own.
        let _ = fs::remove_file(self.lease_file());
    }

    fn saved_file(&self) -> PathBuf {
        self.world.join("lease-saved.json")
    }

    /// The server loses its state file as well: nothing saved is restored by the next hello.
    ///
    /// # Panics
    /// A file cannot be removed.
    pub fn lose_state_file(&self) {
        self.forget_host();
        let _ = fs::remove_file(self.saved_file());
    }

    /// Changes the server's stored view of the host, as the server would on its own.
    ///
    /// # Panics
    /// No host is seeded, or the file cannot be written.
    pub fn alter_host(&self, change: impl FnOnce(&mut HostReading)) {
        let mut host = self.stored().unwrap();
        change(&mut host);
        fs::write(self.lease_file(), serde_json::to_vec(&host).unwrap()).unwrap();
    }

    /// The host as stored, expiry not applied. After a restart that kept the host's binding
    /// (see [`World::forget_host`]) the host is `awaiting`, as js-registry's `Record::awaiting`
    /// has it: epoch 0, no session, the saved cordon and taint, until its next hello. The
    /// server knows no host before a hello, or after the state file was lost.
    fn stored(&self) -> Result<HostReading, LeaseError> {
        match fs::read(self.lease_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|why| unreachable(&why)),
            Err(why) if why.kind() == io::ErrorKind::NotFound => self.awaiting(),
            Err(why) => Err(unreachable(&why)),
        }
    }

    fn awaiting(&self) -> Result<HostReading, LeaseError> {
        let bytes = fs::read(self.saved_file()).map_err(|_| LeaseError::UnknownHost)?;
        let saved: Saved = serde_json::from_slice(&bytes).map_err(|why| unreachable(&why))?;
        Ok(HostReading {
            id: self.host_id(),
            state: "awaiting".to_owned(),
            epoch: 0,
            session_started_ms: None,
            leases: 0,
            lease: None,
            releasing: false,
            mark: None,
            cordon: saved.cordon,
            taint: saved.taint,
            agent_version: None,
            runtime: None,
        })
    }

    fn host_id(&self) -> String {
        fs::read_to_string(self.control.join("host-id")).unwrap_or_else(|_| "host-1".to_owned())
    }

    /// The host as the server reads it now: a cordon past its expiry is gone.
    fn live(&self) -> Result<HostReading, LeaseError> {
        let mut host = self.stored()?;
        let now = self.now_ms();
        host.cordon = host.cordon.filter(|cordon| cordon.expires_at_ms > now);
        Ok(host)
    }

    fn save(&self, host: &HostReading) -> Result<(), LeaseError> {
        self.gate();
        serde_json::to_vec(host)
            .map_err(|why| unreachable(&why))
            .and_then(|bytes| fs::write(self.lease_file(), bytes).map_err(|why| unreachable(&why)))
    }

    /// The agent says hello: the server makes the host if it knows none (epoch 1), else one more
    /// epoch; a new session, the version and the runtime either way.
    ///
    /// # Errors
    /// The server's file cannot be read or written.
    pub fn hello(&self, agent_version: &str) -> Result<(), LeaseError> {
        let mut host = match self.live() {
            Err(LeaseError::UnknownHost) => HostReading {
                id: self.host_id(),
                state: "live".to_owned(),
                epoch: 0,
                session_started_ms: None,
                leases: 0,
                lease: None,
                releasing: false,
                mark: None,
                cordon: None,
                taint: None,
                agent_version: None,
                runtime: None,
            },
            other => other?,
        };
        host.epoch += 1;
        host.state = "live".to_owned();
        host.session_started_ms = Some(self.server_now_ms());
        host.agent_version = Some(agent_version.to_owned());
        host.runtime = Some(RUNTIME.to_owned());
        self.save(&host)
    }

    /// The lease server's clock runs `skew_ms` ahead of the host's (negative: behind). Only the
    /// session stamps the server makes are on its clock; cordon expiries stay on the host's, so
    /// a test of the clock difference changes nothing else.
    ///
    /// # Panics
    /// The file cannot be written.
    pub fn set_server_skew_ms(&self, skew_ms: i64) {
        fs::write(self.control.join("server-skew-ms"), skew_ms.to_string()).unwrap();
    }

    /// The server's clock: the host's plus the skew (see [`World::set_server_skew_ms`]).
    fn server_now_ms(&self) -> u64 {
        let skew: i64 = fs::read_to_string(self.control.join("server-skew-ms"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        self.now_ms().saturating_add_signed(skew)
    }

    /// The number of pauses taken.
    #[must_use]
    pub fn ticks(&self) -> usize {
        fs::read_to_string(self.control.join("ticks")).map_or(0, |text| text.len())
    }

    /// The version a starting agent reports: the one the params give for the release the first
    /// release link points at (see [`World::register_release`]); none when the link or the
    /// release is not known.
    fn linked_version(&self) -> Result<Option<String>, PortError> {
        let mut names: Vec<PathBuf> = fs::read_dir(self.world.join("link"))
            .map_err(|why| io_err(&why))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        names.sort();
        let Some(link) = names.first() else {
            return Ok(None);
        };
        let Some(target) = read_link(link).map_err(|why| io_err(&why))? else {
            return Ok(None);
        };
        let table = fs::read_to_string(self.control.join("releases")).unwrap_or_default();
        table
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .rev()
            .find(|(path, _)| Path::new(path) == target)
            .map(|(_, version)| Some(version.to_owned()))
            .ok_or_else(|| {
                PortError::Io(format!(
                    "the release {} is in no params: the fake knows no version for it",
                    target.display()
                ))
            })
    }

    fn link_file(&self, link: &Path) -> Result<PathBuf, PortError> {
        link.file_name()
            .map(|name| self.world.join("link").join(name))
            .ok_or_else(|| PortError::Io(format!("{}: no file name", link.display())))
    }
}

impl Units for World {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        match fs::read_to_string(self.unit_file(unit)) {
            Ok(text) => {
                let (active, sub) = text.split_once(' ').unwrap_or((&text, ""));
                Ok(UnitState {
                    active: active.to_owned(),
                    sub: sub.to_owned(),
                })
            }
            // The manager knows a unit it has never run as inactive.
            Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(UnitState {
                active: "inactive".to_owned(),
                sub: "dead".to_owned(),
            }),
            Err(why) => Err(io_err(&why)),
        }
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        if self.show(unit)?.active == "inactive" {
            return Ok(());
        }
        self.put_unit(unit, "inactive", "dead")
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        if self.show(unit)?.active == "active" {
            return Ok(());
        }
        // The agent that starts says hello with the version of the release the link names; a
        // release the params never named is an error, before anything changes.
        let version = self.linked_version()?;
        self.put_unit(unit, "active", "running")?;
        match version {
            Some(version) => self
                .hello(&version)
                .map_err(|why| PortError::Io(why.to_string())),
            None => Ok(()),
        }
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.show(unit).map(|state| state.active == "active")
    }
}

impl Link for World {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        read_link(&self.link_file(link)?).map_err(|why| io_err(&why))
    }

    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        let file = self.link_file(link)?;
        if read_link(&file).map_err(|why| io_err(&why))?.as_deref() == Some(target) {
            return Ok(());
        }
        self.gate();
        swap_link(&file, target).map_err(|why| io_err(&why))
    }
}

impl Lease for World {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        if lease_secs == 0 {
            return Err(LeaseError::Refused("invalid-cordon".to_owned()));
        }
        let mut host = self.live()?;
        let asked = self.now_ms() + lease_secs.min(MAX_CORDON_SECS) * 1000;
        let already_cordoned = host.cordon.is_some();
        let expires_at_ms = host
            .cordon
            .map_or(asked, |cordon| cordon.expires_at_ms.max(asked));
        host.cordon = Some(CordonView {
            expires_at_ms,
            expires_in_ms: None,
        });
        self.save(&host)?;
        Ok(Cordoned {
            already_cordoned,
            host,
        })
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        let mut host = self.live()?;
        let lifted = host.cordon.take().is_some();
        if lifted {
            self.save(&host)?;
        }
        Ok(Uncordoned { lifted, host })
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        self.live()
    }
}

impl Clock for World {
    fn now_ms(&self) -> u64 {
        fs::read_to_string(self.control.join("clock"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(START_MS)
    }

    fn pause(&self, span: Duration) {
        let moved = self.now_ms() + u64::try_from(span.as_millis()).unwrap();
        fs::write(self.control.join("clock"), moved.to_string()).unwrap();
        let mut ticks = fs::read_to_string(self.control.join("ticks")).unwrap_or_default();
        ticks.push('.');
        fs::write(self.control.join("ticks"), ticks).unwrap();
    }
}
