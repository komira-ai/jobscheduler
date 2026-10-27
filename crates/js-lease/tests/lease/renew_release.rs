//! Renewing and releasing a lease, and how an Unknown stop is treated.

use std::time::Duration;

use js_lease::{LeaseState, ReleaseOutcome, RenewError};
use js_provider::{Call, Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_provider_fake::FakeProvider;
use js_types::{LeaseId, ProviderId, ResourceClass};

use crate::support::{Scratch, T0, at, fake, fresh, host, open_with, request, secs, states};

/// Asserts that acquire, renew and release work on the fake: the lease takes the first
/// host, expires after the default ttl, renews from the renewal's time, never shortens
/// on a short ttl, and release frees the provider's host.
#[test]
fn acquire_renew_release_on_the_fake() {
    let (mut table, clock, _scratch) = fresh("happy_fake");
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(lease.id, LeaseId::new(1, 1));
    assert_eq!(lease.host, host("h1"));
    assert_eq!(lease.expires_at, at(T0 + 60_000));
    assert_eq!(states(&table), vec![(lease.id, LeaseState::Held)]);

    clock.advance(secs(50));
    assert_eq!(table.renew(&lease.id, None).unwrap(), at(T0 + 110_000));
    let short = table
        .renew(&lease.id, Some(Duration::from_millis(1)))
        .unwrap();
    assert_eq!(short, at(T0 + 110_000));

    assert_eq!(table.release(&lease.id).unwrap(), ReleaseOutcome::Freed);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that an Unknown stop keeps the lease releasing with its host still booked,
/// that a renew of it is refused, that a tick whose stop and observe both answer
/// Unknown keeps it and changes nothing, and that a later tick frees it.
#[test]
fn unknown_stop_keeps_releasing_and_tick_retries() {
    let (mut table, _clock, _scratch) = fresh("unknown_stop");
    let lease = table.acquire(request(3600), None).unwrap();
    table.provider_mut().script_unknown(Call::Stop, false);
    assert_eq!(table.release(&lease.id).unwrap(), ReleaseOutcome::Releasing);
    assert_eq!(states(&table), vec![(lease.id, LeaseState::Releasing)]);
    assert_eq!(table.provider().booked().len(), 1);
    let err = table.renew(&lease.id, None).unwrap_err();
    assert!(matches!(
        err,
        RenewError::NotHeld {
            state: LeaseState::Releasing,
            ..
        }
    ));

    table.provider_mut().script_unknown(Call::Stop, false);
    table.provider_mut().script_unknown(Call::Observe, false);
    let stuck = table.tick();
    assert_eq!((stuck.releasing, stuck.changed()), (1, false));
    assert_eq!(table.provider().booked().len(), 1);

    let report = table.tick();
    assert_eq!(report.freed, vec![lease.id]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that a releasing lease whose retried stop is Unknown but landed is freed by
/// the observation that follows it, and that an expired lease whose stop is Unknown
/// stays releasing until a retry frees it.
#[test]
fn observe_absent_frees_a_releasing_lease() {
    let (mut table, clock, _scratch) = fresh("observe_frees");
    let a = table.acquire(request(3600), None).unwrap();
    table.provider_mut().script_unknown(Call::Stop, false);
    table.release(&a.id).unwrap();
    table.provider_mut().script_unknown(Call::Stop, true);
    assert_eq!(table.tick().freed, vec![a.id]);

    let b = table.acquire(request(3600), None).unwrap();
    clock.advance(secs(60));
    table.provider_mut().script_unknown(Call::Stop, false);
    let report = table.tick();
    assert_eq!(
        (report.expired, report.freed.len(), report.releasing),
        (vec![b.id], 0, 1)
    );
    assert_eq!(table.tick().freed, vec![b.id]);
}

/// Asserts that a stop that read back Present, or nothing, does not free the lease: a
/// stop is believed only as far as its read-back shows.
#[test]
fn stop_that_reads_back_present_does_not_free() {
    let scratch = Scratch::new("present_readback");
    let mut table = open_with(Odd(fake()), &scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(table.release(&lease.id).unwrap(), ReleaseOutcome::Releasing);
    assert_eq!(table.list()[0].state, LeaseState::Releasing);
    let report = table.tick();
    assert_eq!((report.releasing, report.changed()), (1, false));
}

/// A provider whose stop claims to have stopped while reading the lease back as still
/// present.
struct Odd(FakeProvider);

impl Provider for Odd {
    fn id(&self) -> ProviderId {
        self.0.id()
    }

    fn flavour(&self) -> Flavour {
        self.0.flavour()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.0.classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        self.0.start(spec)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        match self.0.observe(lease) {
            read_back @ Observed::Present { .. } => StopAck::Stopped { read_back },
            _ => StopAck::Unknown,
        }
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.0.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.0.capacity()
    }
}

/// Asserts that a lease still pending can be released: it is stopped on the provider
/// (a start that landed late is not left booked) and removed from the table.
#[test]
fn a_pending_lease_can_be_released() {
    let (mut table, _clock, _scratch) = fresh("release_pending");
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(request(3600), None).unwrap_err();
    let id = LeaseId::new(1, 1);
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);
    assert_eq!(table.release(&id).unwrap(), ReleaseOutcome::Freed);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}
