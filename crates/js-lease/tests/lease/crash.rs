//! Crash windows: what a restart finds when the process stopped between the provider and the file.

use std::fs;
use std::path::PathBuf;

use js_lease::LeaseState;
use js_provider::{Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_provider_fake::FakeProvider;
use js_types::{LeaseId, ProviderId, ResourceClass};

use crate::support::{
    Scratch, T0, at, clock, fake, host, open, open_with, provider_classes, request, states,
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
