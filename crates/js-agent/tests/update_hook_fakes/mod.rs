//! The fakes of the hook's ports. They keep their state in two directories, so that a second
//! process (the hook binary the conformance harness starts) sees what a test left and the test
//! sees what the process did:
//!
//! - `world/`, the observable state of the service the hook acts on, which the harness
//!   compares between runs: `units/<unit>` (the unit manager), `link/<name>` (release links),
//!   `lease.json` (the lease server's view of the host) and `state/` (the agent's state root).
//! - `control/`, what the harness must not compare: the clock (`clock`, milliseconds) and the
//!   count of pauses (`ticks`).
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
    PortError, Uncordoned, UnitState, Units, read_link, swap_link,
};

/// Where the fake clock starts, in milliseconds.
pub const START_MS: u64 = 1_000_000;

/// The runtime a fake hello reports.
pub const RUNTIME: &str = "crun";

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
            session_started_ms: Some(self.now_ms()),
            leases: 0,
            lease: None,
            releasing: false,
            cordon: None,
            taint: None,
            agent_version: Some(agent_version.to_owned()),
            runtime: Some(RUNTIME.to_owned()),
        };
        fs::write(self.lease_file(), serde_json::to_vec(&host).unwrap()).unwrap();
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

    /// The host as stored, expiry not applied; `None` when the server knows no host.
    fn stored(&self) -> Result<HostReading, LeaseError> {
        match fs::read(self.lease_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|why| unreachable(&why)),
            Err(why) if why.kind() == io::ErrorKind::NotFound => Err(LeaseError::UnknownHost),
            Err(why) => Err(unreachable(&why)),
        }
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

    /// The agent says hello: one more epoch, a new session, the version and the runtime.
    ///
    /// # Errors
    /// No host is seeded.
    pub fn hello(&self, agent_version: &str) -> Result<(), LeaseError> {
        let mut host = self.live()?;
        host.epoch += 1;
        host.state = "live".to_owned();
        host.session_started_ms = Some(self.now_ms());
        host.agent_version = Some(agent_version.to_owned());
        host.runtime = Some(RUNTIME.to_owned());
        self.save(&host)
    }

    /// The number of pauses taken.
    #[must_use]
    pub fn ticks(&self) -> usize {
        fs::read_to_string(self.control.join("ticks")).map_or(0, |text| text.len())
    }

    /// The version a starting agent reports: the name of the directory the first release link
    /// points into, or none.
    fn linked_version(&self) -> Option<String> {
        let mut names: Vec<PathBuf> = fs::read_dir(self.world.join("link"))
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        names.sort();
        let target = read_link(names.first()?).ok()??;
        Some(target.file_name()?.to_string_lossy().into_owned())
    }

    fn link_file(&self, link: &Path) -> Result<PathBuf, PortError> {
        link.file_name()
            .map(|name| self.world.join("link").join(name))
            .ok_or_else(|| PortError::Io(format!("{}: no file name", link.display())))
    }
}

// The trait impls are stubs: the unit never changes, no link is made, the server answers
// nothing, the clock stands still.
impl Units for World {
    fn show(&self, _unit: &str) -> Result<UnitState, PortError> {
        Ok(UnitState {
            active: "inactive".to_owned(),
            sub: "dead".to_owned(),
        })
    }

    fn stop_no_block(&self, _unit: &str) -> Result<(), PortError> {
        Ok(())
    }

    fn start(&self, _unit: &str) -> Result<(), PortError> {
        let _ = (World::put_unit, World::linked_version, World::hello);
        Ok(())
    }

    fn is_active(&self, _unit: &str) -> Result<bool, PortError> {
        Ok(false)
    }
}

impl Link for World {
    fn target(&self, _link: &Path) -> Result<Option<PathBuf>, PortError> {
        let _ = (World::link_file, read_link, swap_link);
        Ok(None)
    }

    fn relink(&self, _link: &Path, _target: &Path) -> Result<(), PortError> {
        Ok(())
    }
}

impl Lease for World {
    fn cordon(&self, _lease_secs: u64) -> Result<Cordoned, LeaseError> {
        let _ = (
            World::save,
            MAX_CORDON_SECS,
            CordonView { expires_at_ms: 0 },
        );
        Err(LeaseError::UnknownHost)
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        Err(LeaseError::UnknownHost)
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        let _ = World::live;
        Err(LeaseError::UnknownHost)
    }
}

impl Clock for World {
    fn now_ms(&self) -> u64 {
        START_MS
    }

    fn pause(&self, _span: Duration) {}
}
