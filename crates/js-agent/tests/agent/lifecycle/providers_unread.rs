//! A restart whose runtime left a lease alone because it cannot read the lease's ledger file
//! (`Runtime::unread_leases`): the job may still run, so its provider sources are kept (the
//! sweep keeps its run, no release is asked) and so is its run's directory, and each such
//! lease is reported.

use std::path::PathBuf;
use std::time::Duration;

use js_agent::lifecycle::{
    Event, Found, Lifecycle, RunHandle, RunSpec, Runtime, RuntimeError, Status, UnreadLease,
};

use super::fake::FakeRuntime;
use super::fake_providers::{WAIT_MS, rig, run_one};
use super::{Harness, hook, names};

/// The wrapped [`FakeRuntime`], which says it left `leases` alone; the calls of a lifecycle
/// that adopts and steps with no job are the fake's.
pub struct Unread {
    inner: FakeRuntime,
    leases: Vec<UnreadLease>,
}

impl Unread {
    /// `inner` (a handle on the same state), leaving `leases` alone.
    pub fn around(inner: FakeRuntime, leases: Vec<UnreadLease>) -> Self {
        Self { inner, leases }
    }
}

impl Runtime for Unread {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        self.inner.create_and_start(spec)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        self.inner.poll(id)
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.inner.stop(id, grace)
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.inner.remove(id)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.inner.list_labelled()
    }

    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        self.inner.lease_dir(run_id)
    }

    fn unread_leases(&self) -> Vec<UnreadLease> {
        self.leases.clone()
    }
}

/// A lease of the run `run_id` left alone, its file of version `version`.
pub fn unread(run_id: &str, version: Option<u64>) -> UnreadLease {
    UnreadLease {
        run_id: run_id.to_owned(),
        lease: format!("js-0123456789abcdef-{run_id}"),
        version,
        why: "its form is version 3".to_owned(),
    }
}

/// Restarts the lifecycle of `h` over an [`Unread`] runtime around its fake, leaving `leases`
/// alone, and returns what the adoption gave.
fn restart(h: &mut Harness, leases: Vec<UnreadLease>) -> Vec<Event> {
    let scratch = h.dir.path("scratch");
    let checks = hook(&h.log, &h.fake, &scratch, &[]);
    let runtime = Unread::around(h.fake.clone(), leases);
    let (life, events) = Lifecycle::adopt(
        h.config.clone(),
        Box::new(runtime),
        checks,
        Box::new(h.clock.clone()),
    )
    .expect("the runtime lists its containers");
    h.life = life;
    events
}

/// Asserts the restart of an agent whose runtime cannot read the ledger files of two of its
/// leases (one of a version it does not read, one that says no version): each is reported in
/// one `lease-unreadable` finding naming the lease, its job and the file's version; the
/// sweep keeps their runs, each from the attempt its record names; no release is asked for
/// them, however long the agent steps; and their records and run directories stay. A run
/// whose container is gone and whose lease is not among them is released and its directory
/// removed, as before. Catches a job that runs on with no agent to bound it losing its tree
/// to the sweep or to a release, or its directory to the start's sweep, and such a lease
/// left unsaid.
#[test]
fn a_lease_left_alone_keeps_its_sources_and_its_directory() {
    let (mut h, providers) = rig("prov-unread", |_| {});
    for job in ["j1", "j2", "j3"] {
        run_one(&mut h, &providers, job);
    }
    let (kept, alone, gone) = (h.scratch_of("j1"), h.scratch_of("j2"), h.scratch_of("j3"));
    assert!(kept.is_dir() && alone.is_dir() && gone.is_dir());
    for job in ["j1", "j2", "j3"] {
        h.fake.vanish(job);
    }

    let adopted = restart(&mut h, vec![unread("j1", Some(3)), unread("j2", None)]);
    assert_eq!(
        names(&adopted),
        ["finding j1 lease-unreadable", "finding j2 lease-unreadable"]
    );
    let messages: Vec<&str> = adopted
        .iter()
        .filter_map(|event| match event {
            Event::Finding(found) => Some(found.message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages,
        [
            "the lease js-0123456789abcdef-j1 of job j1 is left alone: its ledger file, of \
             version 3, cannot be read (its form is version 3); its job, if it still runs, is \
             neither taken over nor stopped, and its provider sources and run directory are \
             kept",
            "the lease js-0123456789abcdef-j2 of job j2 is left alone: its ledger file, of no \
             version this agent can read, cannot be read (its form is version 3); its job, if \
             it still runs, is neither taken over nor stopped, and its provider sources and \
             run directory are kept",
        ]
    );
    h.life
        .attach_providers(Box::new(providers.clone()), WAIT_MS);
    assert_eq!(
        providers.attempts("sweep").last(),
        Some(&(
            "provider sweep tier keep=[\"j1\", \"j2\"]".to_owned(),
            vec![1, 2]
        ))
    );
    for _ in 0..8 {
        h.clock.advance(30_000);
        h.life.step();
    }
    assert_eq!(providers.sent("release"), ["provider release tier j3#0"]);
    let records = h.dir.path("state/providers");
    assert!(records.join("j1.json").is_file() && records.join("j2.json").is_file());
    assert!(!records.join("j3.json").exists());
    assert!(kept.is_dir() && alone.is_dir());
    assert!(!gone.exists());
}
