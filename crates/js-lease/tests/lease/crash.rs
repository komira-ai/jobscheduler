//! Crash windows: what a restart finds when the process stopped between the provider and the file.

use std::fs;
use std::path::PathBuf;

use js_lease::{AcquireError, Boot, LeaseState, LeaseTable, ManualClock};
use js_provider::{
    Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_provider_fake::FakeProvider;
use js_types::{ClassId, LeaseId, ProviderId, ResourceClass, Resources};

use crate::support::{
    Scratch, T0, at, clock, fake, host, open, open_with, provider_classes, request, settings,
    states,
};

/// A provider that removes the lease directory when it is asked to start, so the save
/// that follows the start fails.
struct Saboteur {
    inner: FakeProvider,
    dir: PathBuf,
}

impl Provider for Saboteur {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn flavour(&self) -> Flavour {
        self.inner.flavour()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.inner.classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        let ack = self.inner.start(spec);
        let _ = fs::remove_dir_all(&self.dir);
        ack
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.inner.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.inner.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.inner.capacity()
    }
}

/// Asserts that a save that fails after the provider started a lease does not lose it:
/// the client gets the lease with its id and host, the table lists it held, the next
/// tick reports the failed save with nothing else changed, and once the directory is
/// back the following tick writes the lease to the file.
#[test]
fn a_save_that_fails_after_the_start_keeps_the_lease() {
    let scratch = Scratch::new("save_after_start");
    let provider = Saboteur {
        inner: fake(),
        dir: scratch.dir.clone(),
    };
    let mut table = open_with(provider, &scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(lease.host, host("h1"));
    assert_eq!(states(&table), vec![(lease.id, LeaseState::Held)]);

    let report = table.tick();
    assert!(!report.changed());
    assert!(report.save_error.is_some());
    assert_eq!(table.list().len(), 1);

    fs::create_dir_all(&scratch.dir).unwrap();
    assert!(table.tick().save_error.is_none());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("state = \"held\"\nhost = \"h1\""));
    let renewed = table.renew(&lease.id, None).unwrap();
    assert_eq!(renewed, at(T0 + 60_000));
}

/// Asserts that a lease still pending when the process stopped (saved before the
/// provider was called) is resolved by a tick after a real open: confirmed and held
/// on the provider's host when the provider has it, dropped when it does not.
#[test]
fn a_lease_saved_pending_before_the_start_is_resolved_after_reopen() {
    let scratch = Scratch::new("crash_window");
    let text = "version = 1\nterm = 2\nnext_sequence = 4\nlease_count = 1\n\n[[lease]]\n\
        term = 2\nsequence = 3\nstate = \"pending\"\nacquired_at_ms = 1000000\n\
        expires_at_ms = 1060000\nends_at_ms = 4600000\n";
    let id = LeaseId::new(2, 3);

    fs::write(scratch.file(), text).unwrap();
    let mut provider = fake();
    provider.start(StartSpec::new(id, request(3600), &provider_classes()).unwrap());
    let mut table = open(provider, &clock(), &scratch);
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);
    let report = table.tick();
    assert_eq!(report.confirmed, vec![id]);
    let info = &table.list()[0];
    assert_eq!(
        (info.state, info.host.clone()),
        (LeaseState::Held, Some(host("h1")))
    );
    assert!(
        fs::read_to_string(scratch.file())
            .unwrap()
            .contains("state = \"held\"")
    );
    drop(table);

    scratch.clear();
    fs::write(scratch.file(), text).unwrap();
    let mut table = open(fake(), &clock(), &scratch);
    let report = table.tick();
    assert_eq!(report.dropped, vec![id]);
    assert!(table.list().is_empty());
}

/// A provider that records what the lease file said each time it was asked to start,
/// and can make the next save fail once it has answered.
struct Watched {
    inner: FakeProvider,
    file: PathBuf,
    /// The file's text at each start, `None` when there was no file.
    seen: Vec<Option<String>>,
    /// After answering a start, put a directory where the save writes its temporary
    /// file, so the save that follows fails and the file on disk stays as it was.
    block_next_save: bool,
}

impl Watched {
    fn new(scratch: &Scratch, block_next_save: bool) -> Self {
        Self {
            inner: fake(),
            file: scratch.file(),
            seen: Vec::new(),
            block_next_save,
        }
    }
}

impl Provider for Watched {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn flavour(&self) -> Flavour {
        self.inner.flavour()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.inner.classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        self.seen.push(fs::read_to_string(&self.file).ok());
        let ack = self.inner.start(spec);
        if self.block_next_save {
            let mut name = self.file.file_name().unwrap().to_os_string();
            name.push(".tmp");
            fs::create_dir(self.file.with_file_name(name)).unwrap();
        }
        ack
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.inner.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.inner.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.inner.capacity()
    }
}

fn read(scratch: &Scratch) -> String {
    fs::read_to_string(scratch.file()).unwrap()
}

/// Asserts that the lease is in the file, as pending with no host, before the provider
/// is asked to start it: the provider reads the file from inside its start and finds the
/// new lease (its id, the next sequence already counted), and after the start answered
/// the file says held.
#[test]
fn the_lease_is_on_disk_as_pending_before_the_provider_is_asked() {
    let scratch = Scratch::new("order_pending");
    let mut table = open_with(Watched::new(&scratch, false), &scratch);
    let lease = table.acquire(request(3600), None).unwrap();

    let seen = &table.provider().seen;
    assert_eq!(seen.len(), 1);
    let before = seen[0].as_deref().unwrap();
    assert!(
        before.contains("next_sequence = 2\nlease_count = 1\n"),
        "{before}"
    );
    assert!(
        before.contains("sequence = 1\nstate = \"pending\"\n"),
        "{before}"
    );
    assert!(!before.contains("host ="), "{before}");
    assert!(!before.contains("held"), "{before}");

    let after = read(&scratch);
    assert!(after.contains("sequence = 1\nstate = \"held\"\nhost = \"h1\"\n"));
    assert_eq!(lease.id, LeaseId::new(1, 1));
}

/// Asserts that a start the provider refuses leaves no record: the provider saw the
/// lease pending in the file when it refused, afterwards the file counts no leases (and
/// keeps the sequence it used), and a reopen finds an empty table. A start the contract
/// refuses never reaches the provider and never reaches the file.
#[test]
fn a_refused_start_leaves_no_record_in_the_file() {
    let scratch = Scratch::new("order_refused");
    let mut table = open_with(Watched::new(&scratch, false), &scratch);

    let mut unknown = request(3600);
    unknown.class = ClassId::new("other").unwrap();
    let err = table.acquire(unknown, None).unwrap_err();
    assert!(matches!(
        err,
        AcquireError::Refused(RefusalCode::UnknownClass)
    ));
    assert!(table.provider().seen.is_empty());
    assert!(!read(&scratch).contains("[[lease]]"));

    let mut wide = request(3600);
    wide.resources = Resources::new(99_000, 1, 1, Some("g".into())).unwrap();
    let err = table.acquire(wide, None).unwrap_err();
    assert!(matches!(err, AcquireError::Refused(RefusalCode::NoFit)));
    let seen = &table.provider().seen;
    assert_eq!(seen.len(), 1);
    let before = seen[0].as_deref().unwrap();
    assert!(
        before.contains("sequence = 2\nstate = \"pending\"\n"),
        "{before}"
    );

    let after = read(&scratch);
    assert!(
        after.contains("next_sequence = 3\nlease_count = 0\n"),
        "{after}"
    );
    assert!(!after.contains("[[lease]]"), "{after}");
    assert!(table.list().is_empty());

    let provider = table.into_provider();
    let boot = Boot::Resume;
    let reopened = LeaseTable::open(provider, clock(), settings(), scratch.file(), boot).unwrap();
    assert!(reopened.list().is_empty());
}

/// Runs the real acquire path on a provider that makes the save after its start fail,
/// then stops the process: the client got a held lease, the file still says pending,
/// and the table is gone. Returns the provider as it was at that moment.
fn crash_after_the_start(scratch: &Scratch) -> (Watched, LeaseId) {
    let mut table = open_with(Watched::new(scratch, true), scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(states(&table), vec![(lease.id, LeaseState::Held)]);
    let on_disk = read(scratch);
    assert!(
        on_disk.contains("sequence = 1\nstate = \"pending\"\n"),
        "{on_disk}"
    );
    assert!(!on_disk.contains("held"), "{on_disk}");

    let mut provider = table.into_provider();
    provider.block_next_save = false;
    fs::remove_dir(scratch.dir.join("leases.toml.tmp")).unwrap();
    (provider, lease.id)
}

fn resume(provider: Watched, scratch: &Scratch) -> LeaseTable<Watched, ManualClock> {
    let boot = Boot::Resume;
    LeaseTable::open(provider, clock(), settings(), scratch.file(), boot).unwrap()
}

/// Asserts the crash between the provider's start and the save of the held state, driven
/// through the real acquire: the reopened table has the lease pending (the file never
/// said held), and the first tick observes the provider, finds it present, makes it held
/// on the provider's host and writes that; ids of the new boot continue past it.
#[test]
fn a_crash_after_the_start_resolves_to_held_when_the_provider_has_it() {
    let scratch = Scratch::new("crash_present");
    let (provider, id) = crash_after_the_start(&scratch);
    let mut table = resume(provider, &scratch);
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);

    let report = table.tick();
    assert_eq!(report.confirmed, vec![id]);
    assert!(report.save_error.is_none());
    assert_eq!(table.list()[0].host, Some(host("h1")));
    assert!(read(&scratch).contains("sequence = 1\nstate = \"held\"\nhost = \"h1\"\n"));
    assert_eq!(table.renew(&id, None).unwrap(), at(T0 + 60_000));
    let next = table.acquire(request(3600), None).unwrap();
    assert_eq!(next.id, LeaseId::new(2, 2));
}

/// Asserts the same crash when the provider no longer has the lease (it was stopped or
/// never survived): the reopened table has it pending, the first tick observes it
/// absent and drops it, nothing stays booked and the file counts no leases.
#[test]
fn a_crash_after_the_start_is_dropped_when_the_provider_lost_it() {
    let scratch = Scratch::new("crash_absent");
    let (mut provider, id) = crash_after_the_start(&scratch);
    provider.stop(&id);
    let mut table = resume(provider, &scratch);
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);

    let report = table.tick();
    assert_eq!(report.dropped, vec![id]);
    assert!(table.list().is_empty());
    assert!(table.provider().inner.booked().is_empty());
    assert!(read(&scratch).contains("lease_count = 0\n"));
}
