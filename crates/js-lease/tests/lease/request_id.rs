//! A lease acquired under a request id: a repeat of the acquire finds it, a provider that
//! queues the request is polled by the tick under the same lease and id, and the id and
//! the request are kept in the file across a restart.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

use js_lease::{AcquireError, Boot, LeaseState, LeaseTable, ManualClock, OpenError};
use js_provider::{
    Call, Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_provider_fake::FakeProvider;
use js_types::{ClassId, LeaseId, ProviderId, Request, RequestId, ResourceClass, Resources};

use crate::support::{
    Scratch, T0, at, break_dir, clock, fake, fresh, host, request, secs, settings, states,
};

/// What [`Queueing`] answers a start with.
#[derive(Debug, Clone, Copy)]
enum Answer {
    /// Hold the lease on the wait list: `Unknown`, and `waits` says so.
    Queue,
    /// Take it off the list and book it as the fake does.
    Place,
    /// Take it off the list and refuse it `NoCapableHost`.
    Refuse,
}

/// A provider that queues, over the fake: a start is answered as `answer` says, and every
/// start is kept with the lease and request id it carried. With `serves` off it lists no
/// class, as a provider that stopped serving the lease's class.
struct Queueing {
    fake: FakeProvider,
    answer: Answer,
    serves: bool,
    waiting: BTreeSet<LeaseId>,
    starts: Vec<(LeaseId, Option<RequestId>)>,
}

impl Queueing {
    fn new() -> Self {
        Self {
            fake: fake(),
            answer: Answer::Queue,
            serves: true,
            waiting: BTreeSet::new(),
            starts: Vec::new(),
        }
    }
}

impl Provider for Queueing {
    fn id(&self) -> ProviderId {
        self.fake.id()
    }

    fn flavour(&self) -> Flavour {
        self.fake.flavour()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        match self.serves {
            true => self.fake.classes(),
            false => Vec::new(),
        }
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        let lease = spec.lease();
        self.starts.push((lease, spec.request_id().cloned()));
        self.waiting.remove(&lease);
        match self.answer {
            Answer::Queue => {
                self.waiting.insert(lease);
                StartAck::Unknown
            }
            Answer::Place => self.fake.start(spec),
            Answer::Refuse => StartAck::Refused(RefusalCode::NoCapableHost),
        }
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.waiting.remove(lease);
        self.fake.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.fake.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.fake.capacity()
    }

    fn waits(&self, lease: &LeaseId) -> bool {
        self.waiting.contains(lease)
    }
}

type Queued = LeaseTable<Queueing, ManualClock>;

fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

/// A whole-node request with labels and a validation run, which the fake's hosts take.
fn asked() -> Request {
    Request {
        labels: BTreeMap::from([("team".to_owned(), "infra".to_owned())]),
        validation_run: Some("val-7".to_owned()),
        ..request(600)
    }
}

/// [`asked`] with something in every other field the file keeps for it: a probe of host
/// `h1` and a capability. Only a provider that queues takes it (the fake's hosts report
/// no capability).
fn probed() -> Request {
    Request {
        probe_host: Some(host("h1")),
        capabilities: BTreeMap::from([("isa".to_owned(), "x86-64-v3".to_owned())]),
        ..asked()
    }
}

fn queued(scratch: &Scratch, clock: &ManualClock, boot: Boot) -> Queued {
    LeaseTable::open(
        Queueing::new(),
        clock.clone(),
        settings(),
        scratch.file(),
        boot,
    )
    .unwrap()
}

/// The pending lease the first acquire of `request` under `r1` makes over `table`.
fn waiting_lease(table: &mut Queued, request: Request) -> LeaseId {
    match table.acquire_with_id(id("r1"), request, None) {
        Err(AcquireError::Unconfirmed(lease)) => lease,
        other => panic!("not pending: {other:?}"),
    }
}

/// Asserts the poll of a waiting lease. The first acquire carries the request id to the
/// provider, which queues it; a repeat of the acquire then asks the provider nothing and
/// answers the same pending lease. Past the prepare timeout (600 s) the lease is still
/// pending, because each tick presented it again, under the same lease and request id, and
/// its expiry is the prepare timeout from the last tick. When the provider places it, the
/// tick makes it held on the fake's host. It catches a waiting lease expired at the
/// prepare timeout, a tick that observes it instead (the fake would answer absent and drop
/// it), a presentation under a new lease or without the id, and a repeat that starts a
/// second attempt.
#[test]
fn a_waiting_lease_is_presented_again_each_tick_until_placed() {
    let scratch = Scratch::new("request_id_poll");
    let clock = clock();
    let mut table = queued(&scratch, &clock, Boot::Initialise);
    let lease = waiting_lease(&mut table, asked());
    let carried = (lease, Some(id("r1")));
    assert_eq!(table.provider().starts, std::slice::from_ref(&carried));
    let repeat = table.acquire_with_id(id("r1"), asked(), None);
    assert!(matches!(repeat, Err(AcquireError::Unconfirmed(same)) if same == lease));
    assert_eq!(table.provider().starts.len(), 1, "a repeat calls nothing");

    clock.advance(secs(700));
    let report = table.tick();
    assert_eq!(report.pending, 1);
    assert!(report.expired.is_empty() & report.dropped.is_empty());
    assert_eq!(table.provider().starts, [carried.clone(), carried.clone()]);
    let listed = table.list();
    assert_eq!(listed[0].state, LeaseState::Pending);
    assert_eq!(listed[0].expires_at, at(T0 + 700_000 + 600_000));

    table.provider_mut().answer = Answer::Place;
    clock.advance(secs(5));
    let report = table.tick();
    assert_eq!(report.confirmed, [lease]);
    assert_eq!(table.provider().starts.len(), 3);
    let listed = table.list();
    assert_eq!(listed[0].state, LeaseState::Held);
    assert_eq!(listed[0].host, Some(host("h1")));
    let booked = table.provider().capacity();
    assert_eq!(booked[0].leased_to, Some(lease));
    assert_eq!(table.provider().id(), ProviderId::new("fake").unwrap());
    assert_eq!(table.provider().flavour(), Flavour::Delegated);
}

/// Asserts that a presentation the provider refuses ends the waiting lease: the tick drops
/// it and the table lists nothing. It catches a refusal kept as a pending lease that
/// nothing would ever place.
#[test]
fn a_refused_presentation_drops_the_waiting_lease() {
    let scratch = Scratch::new("request_id_refused");
    let clock = clock();
    let mut table = queued(&scratch, &clock, Boot::Initialise);
    let lease = waiting_lease(&mut table, asked());
    table.provider_mut().answer = Answer::Refuse;
    let report = table.tick();
    assert_eq!(report.dropped, [lease]);
    assert!(table.list().is_empty());
}

/// Asserts what a repeat under a request id answers over the life of a lease the provider
/// started at once: the lease as it stands while it is held (its expiry after a renewal
/// included), with nothing booked a second time; a conflict naming the lease for the same
/// id with another request, which leaves the lease alone; `lease-ended` while the lease is
/// releasing; and, once the lease is gone, a new lease under the same id. It catches a
/// retry that takes a second host, a conflict answered with the lease, a releasing lease
/// handed back as held, and an id remembered after its lease is gone.
#[test]
fn a_repeat_answers_the_lease_as_it_stands() {
    let (mut table, clock, _scratch) = fresh("request_id_repeat");
    let first = table.acquire_with_id(id("r1"), asked(), None).unwrap();
    let again = table.acquire_with_id(id("r1"), asked(), None).unwrap();
    assert_eq!(again, first);
    clock.advance(secs(10));
    let renewed = table.renew(&first.id, None).unwrap();
    let again = table.acquire_with_id(id("r1"), asked(), None).unwrap();
    assert_eq!((again.id, again.expires_at), (first.id, renewed));
    assert_eq!(table.provider().booked().len(), 1);

    let other = Request {
        max_wall: secs(601),
        ..asked()
    };
    let conflict = table.acquire_with_id(id("r1"), other, None);
    assert!(matches!(conflict, Err(AcquireError::RequestIdConflict(named)) if named == first.id));
    assert_eq!(states(&table), [(first.id, LeaseState::Held)]);

    table.provider_mut().script_unknown(Call::Stop, false);
    table.release(&first.id).unwrap();
    let ended = table.acquire_with_id(id("r1"), asked(), None);
    assert!(matches!(
        ended,
        Err(AcquireError::Refused(RefusalCode::LeaseEnded))
    ));
    table.tick();
    assert!(table.list().is_empty());
    let new = table.acquire_with_id(id("r1"), asked(), None).unwrap();
    assert_ne!(new.id, first.id);
}

/// Asserts that the id and the request outlive a restart. The lease file keeps the
/// waiting lease's request under `[lease.request]`, its probe host included; after a
/// `resume` boot over a fresh provider a repeat of the acquire answers the same pending
/// lease without a start, a repeat without the probe host and the capability is a conflict, and
/// the first tick presents the lease again under its id. It catches a request id or a
/// request lost with the process (the repeat would make a second lease, or a changed
/// request would be taken for the same), and a restored lease that is never presented
/// again (the provider's restored record of the id would go unclaimed).
#[test]
fn the_request_id_and_the_request_survive_a_restart() {
    let scratch = Scratch::new("request_id_restart");
    let clock = clock();
    let mut table = queued(&scratch, &clock, Boot::Initialise);
    let lease = waiting_lease(&mut table, probed());
    drop(table);
    let text = fs::read_to_string(scratch.file()).unwrap();
    let row = "[lease.request]\nid = \"r1\"\nclass = \"whole-node\"\nmax_wall_ms = 600000\n\
               probe_host = \"h1\"\n";
    assert!(text.contains(row), "{text}");

    let mut table = queued(&scratch, &clock, Boot::Resume);
    let repeat = table.acquire_with_id(id("r1"), probed(), None);
    assert!(matches!(repeat, Err(AcquireError::Unconfirmed(same)) if same == lease));
    let conflict = table.acquire_with_id(id("r1"), asked(), None);
    assert!(matches!(conflict, Err(AcquireError::RequestIdConflict(named)) if named == lease));
    assert!(table.provider().starts.is_empty());
    table.tick();
    assert_eq!(table.provider().starts, [(lease, Some(id("r1")))]);
    assert_eq!(states(&table), [(lease, LeaseState::Pending)]);
}

/// Asserts that a lease without a request id is written as before: no `request` row. It
/// catches a row written for every lease, which would change the file of every server
/// that never sends an id.
#[test]
fn a_lease_without_a_request_id_has_no_request_row() {
    let (mut table, _clock, scratch) = fresh("request_id_none");
    table.acquire(asked(), None).unwrap();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("request"), "{text}");
}

/// Opens, over a queueing provider, a file in which `requests` wait under their ids, after
/// `good` is replaced with `bad` in it; the open must be refused.
fn refused_after(name: &str, requests: &[(&str, Request)], good: &str, bad: &str) -> OpenError {
    let scratch = Scratch::new(name);
    let clock = clock();
    let mut table = queued(&scratch, &clock, Boot::Initialise);
    for (named, request) in requests {
        let answer = table.acquire_with_id(id(named), request.clone(), None);
        assert!(
            matches!(answer, Err(AcquireError::Unconfirmed(_))),
            "{answer:?}"
        );
    }
    drop(table);
    let text = fs::read_to_string(scratch.file()).unwrap();
    let broken = text.replace(good, bad);
    assert_ne!(broken, text, "{good}");
    fs::write(scratch.file(), broken).unwrap();
    let opened = LeaseTable::open(fake(), clock, settings(), scratch.file(), Boot::Resume);
    opened.expect_err("refused")
}

/// Asserts that a `request` row that is not a valid request refuses the open as a corrupt
/// file naming the lease, and the table does not start without it: a class or request id
/// the types refuse, a probe host that is not a host id, resources that name GPUs with no
/// GPU class, a zero wall time, a `disk-gib` capability that is not a number, and a row
/// with no class at all. It catches a row read as no request at all, which would forget the
/// id and let a repeat take a second host, and a restored request no acquire would accept.
#[test]
fn a_request_row_that_is_not_a_request_refuses_the_open() {
    let faults = [
        (asked(), "class = \"whole-node\"", "class = \"Not A Class\""),
        (asked(), "id = \"r1\"", "id = \"r 1\""),
        (
            probed(),
            "probe_host = \"h1\"",
            "probe_host = \"not a host\"",
        ),
        (asked(), "gpu_class = \"g\"\n", ""),
        (asked(), "max_wall_ms = 600000", "max_wall_ms = 0"),
        (probed(), "isa = \"x86-64-v3\"", "disk-gib = \"lots\""),
    ];
    for (request, good, bad) in faults {
        let err = refused_after("request_id_bad_row", &[("r1", request)], good, bad);
        assert!(matches!(err, OpenError::Corrupt { .. }), "{bad}: {err}");
        let named = err.to_string();
        assert!(
            named.contains("lease 1-1: its request is not a valid request"),
            "{named}"
        );
    }
    let err = refused_after(
        "request_id_no_class",
        &[("r1", asked())],
        "class = \"whole-node\"\n",
        "",
    );
    assert!(matches!(err, OpenError::Corrupt { .. }), "{err}");
}

/// Asserts that a file in which two leases keep one request id is refused as corrupt,
/// naming the id. It catches a file that would answer a repeat of the id from either
/// lease, whichever the table found first.
#[test]
fn two_leases_under_one_request_id_refuse_the_open() {
    let both = [("r1", asked()), ("r2", asked())];
    let err = refused_after("request_id_twice", &both, "id = \"r2\"", "id = \"r1\"");
    assert!(matches!(err, OpenError::Corrupt { .. }), "{err}");
    let named = err.to_string();
    assert!(
        named.contains("the request id r1 is kept by two leases"),
        "{named}"
    );
}

/// Asserts which fields make the request: under one id, a request that differs from the
/// first in the class, any resource dimension, a capability, the probe host, the
/// validation run, a label or the wall time is a conflict naming the lease, each alone;
/// one that differs only in the ttl is the same request and is answered with the lease, its
/// expiry the first acquire's. It catches a field left out of the comparison (a changed
/// request would be taken for the same) and a ttl taken into it (a retry with another
/// ttl would be refused).
#[test]
fn each_field_of_the_request_counts_and_the_ttl_does_not() {
    let (mut table, _clock, _scratch) = fresh("request_id_fields");
    let first = table.acquire_with_id(id("r1"), asked(), None).unwrap();
    let base = asked;
    let with = |resources: Resources| Request {
        resources,
        ..base()
    };
    let variants = [
        Request {
            class: ClassId::new("gpu-host").unwrap(),
            ..base()
        },
        with(Resources::new(2000, 1, 1, Some("g".into())).unwrap()),
        with(Resources::new(1000, 2, 1, Some("g".into())).unwrap()),
        with(Resources::new(1000, 1, 0, None).unwrap()),
        with(Resources::new(1000, 1, 1, Some("h".into())).unwrap()),
        with(base().resources.with_disk_bytes(1)),
        Request {
            capabilities: probed().capabilities,
            ..base()
        },
        Request {
            probe_host: Some(host("h1")),
            ..base()
        },
        Request {
            validation_run: None,
            ..base()
        },
        Request {
            labels: BTreeMap::from([("team".to_owned(), "web".to_owned())]),
            ..base()
        },
        Request {
            max_wall: secs(601),
            ..base()
        },
    ];
    for variant in variants {
        let answer = table.acquire_with_id(id("r1"), variant.clone(), None);
        let named =
            matches!(answer, Err(AcquireError::RequestIdConflict(lease)) if lease == first.id);
        assert!(named, "{variant:?}: {answer:?}");
    }
    let other_ttl = table
        .acquire_with_id(id("r1"), asked(), Some(secs(30)))
        .unwrap();
    assert_eq!(other_ttl, first);
}

/// Asserts that a wall time with a part below a millisecond is kept exactly: the row holds
/// the whole milliseconds and the nanoseconds below them; after a restart a repeat of the
/// same request is answered with the waiting lease, one whose wall time differs by a
/// microsecond is a conflict, and the first tick presents the restored request. With a wall
/// time of half a millisecond, which an acquire accepts (it is positive), the restored
/// request is the same positive wall time, not zero, and the file opens. It catches a wall
/// time cut to the millisecond, which would refuse the client's own retry after a restart,
/// or read a valid half-millisecond wall time back as zero and refuse the whole file.
#[test]
fn a_wall_time_below_the_millisecond_is_kept_exactly_across_a_restart() {
    let walls = [
        (
            600_000_500,
            "max_wall_ms = 600000\nmax_wall_nanos = 500000\n",
        ),
        (500, "max_wall_ms = 0\nmax_wall_nanos = 500000\n"),
    ];
    for (micros, row) in walls {
        let scratch = Scratch::new("request_id_micros");
        let clock = clock();
        let fine = Request {
            max_wall: Duration::from_micros(micros),
            ..asked()
        };
        let mut table = queued(&scratch, &clock, Boot::Initialise);
        let lease = waiting_lease(&mut table, fine.clone());
        let repeat = table.acquire_with_id(id("r1"), fine.clone(), None);
        assert!(matches!(repeat, Err(AcquireError::Unconfirmed(same)) if same == lease));
        drop(table);
        let text = fs::read_to_string(scratch.file()).unwrap();
        assert!(text.contains(row), "{text}");

        let mut table = queued(&scratch, &clock, Boot::Resume);
        let repeat = table.acquire_with_id(id("r1"), fine.clone(), None);
        assert!(matches!(repeat, Err(AcquireError::Unconfirmed(same)) if same == lease));
        let off = Request {
            max_wall: Duration::from_micros(micros + 1),
            ..asked()
        };
        let conflict = table.acquire_with_id(id("r1"), off, None);
        assert!(matches!(conflict, Err(AcquireError::RequestIdConflict(named)) if named == lease));
        table.tick();
        assert_eq!(table.provider().starts, [(lease, Some(id("r1")))]);
    }
}

/// Asserts that the fields of a later version in a `request` row survive: a scalar and a
/// table added to the row are read, the repeat still matches (they are not compared), and
/// a write of the file keeps both. It catches a row this build rewrites without what a
/// newer build put in it.
#[test]
fn unknown_fields_of_the_request_row_survive_a_rewrite() {
    let scratch = Scratch::new("request_id_unknown");
    let clock = clock();
    let mut table = queued(&scratch, &clock, Boot::Initialise);
    let lease = waiting_lease(&mut table, asked());
    drop(table);
    let text = fs::read_to_string(scratch.file()).unwrap();
    let text = text.replace("[lease.request]\n", "[lease.request]\nfuture = \"x\"\n");
    let text = format!("{text}\n[lease.request.later]\nk = 1\n");
    fs::write(scratch.file(), text).unwrap();

    let mut table = queued(&scratch, &clock, Boot::Resume);
    assert_eq!(table.request_ids(), [(lease, id("r1"))]);
    let repeat = table.acquire_with_id(id("r1"), asked(), None);
    assert!(matches!(repeat, Err(AcquireError::Unconfirmed(same)) if same == lease));
    table.write_reservations(Vec::new()).unwrap();
    let written = fs::read_to_string(scratch.file()).unwrap();
    assert!(written.contains("future = \"x\""), "{written}");
    assert!(written.contains("k = 1"), "{written}");
}

/// Asserts that an id names nothing once the acquire under it left no lease: a refused
/// acquire records nothing, so the next one under the id is a new request; a lease that
/// expired and was freed takes its id with it, so an acquire under the id with another
/// request is a new lease, not a conflict. An acquire whose first write failed keeps its
/// pending record in memory, so a repeat answers that lease and the provider is not asked
/// again. It catches an id kept after a refusal or an expiry, and a repeat after a failed
/// write that starts a second attempt.
#[test]
fn an_id_names_nothing_once_its_lease_is_refused_or_gone() {
    let (mut table, clock, scratch) = fresh("request_id_lifecycle");
    let too_big = Request {
        resources: Resources::new(1000, 1, 9, Some("g".into())).unwrap(),
        ..asked()
    };
    let refused = table.acquire_with_id(id("r1"), too_big, None);
    assert!(
        matches!(refused, Err(AcquireError::Refused(_))),
        "{refused:?}"
    );
    let held = table.acquire_with_id(id("r1"), asked(), None).unwrap();
    clock.advance(secs(61));
    assert_eq!(table.tick().freed, [held.id]);
    let other = Request {
        max_wall: secs(601),
        ..asked()
    };
    let again = table.acquire_with_id(id("r1"), other, None).unwrap();
    assert_ne!(again.id, held.id);

    let booked = table.provider().booked().len();
    break_dir(&scratch);
    let unsaved = table.acquire_with_id(id("r2"), asked(), None);
    assert!(matches!(unsaved, Err(AcquireError::Save(_))), "{unsaved:?}");
    let repeat = table.acquire_with_id(id("r2"), asked(), None);
    assert!(
        matches!(repeat, Err(AcquireError::Unconfirmed(_))),
        "{repeat:?}"
    );
    assert_eq!(table.provider().booked().len(), booked);
}

/// Asserts a waiting lease whose request can no longer be put to the provider (the
/// provider stopped listing its class, so no start can be made): while the provider still
/// holds the request on its wait list the lease stays pending and nothing is started; once
/// the provider has let the request go, the tick observes the lease, finds nothing and
/// drops it. It catches a lease dropped while its provider still holds its request, and
/// one kept pending after the provider let it go.
#[test]
fn a_waiting_lease_that_cannot_be_presented_waits_on_its_provider() {
    let scratch = Scratch::new("request_id_unserved");
    let clock = clock();
    let mut table = queued(&scratch, &clock, Boot::Initialise);
    let lease = waiting_lease(&mut table, asked());
    table.provider_mut().serves = false;
    let report = table.tick();
    assert_eq!(report.pending, 1);
    assert_eq!(table.provider().starts.len(), 1);
    table.provider_mut().waiting.clear();
    let report = table.tick();
    assert_eq!(report.dropped, [lease]);
    assert!(table.list().is_empty());
}
