//! Behaviour of the lease table through its public interface, over the fake provider
//! and the static pool, with a clock the tests advance.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_lease::{
    AcquireError, Clock, LeaseState, LeaseTable, ManualClock, OpenError, ReleaseError,
    ReleaseOutcome, RenewError, Settings, SettingsError, SystemClock, Timestamp,
};
use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
use js_provider::{
    Call, Flavour, HostCapacity, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck,
};
use js_provider_fake::FakeProvider;
use js_types::{ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass, Resources, Shape};

const T0: u64 = 1_000_000;

/// A directory under the target directory that holds one test's lease file and is
/// emptied when the test ends.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-lease")
            .join(name);
        fs::create_dir_all(&dir).unwrap();
        let scratch = Self { dir };
        scratch.clear();
        scratch
    }

    fn file(&self) -> PathBuf {
        self.dir.join("leases.toml")
    }

    fn clear(&self) {
        for name in ["leases.toml", "leases.toml.tmp"] {
            let _ = fs::remove_file(self.dir.join(name));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.clear();
        let _ = fs::remove_dir(&self.dir);
    }
}

type Table = LeaseTable<FakeProvider, ManualClock>;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn settings() -> Settings {
    Settings::new(secs(60), secs(120)).unwrap()
}

fn clock() -> ManualClock {
    ManualClock::new(at(T0))
}

fn shape() -> Shape {
    let resources = Resources::new(8000, 8, 1, Some("g".into())).unwrap();
    Shape::new(resources, BTreeMap::new())
}

fn fake() -> FakeProvider {
    let hosts = vec![(host("h1"), shape()), (host("h2"), shape())];
    FakeProvider::new(ProviderId::new("fake").unwrap(), hosts)
}

fn request(max_wall_secs: u64) -> Request {
    Request {
        class: ClassId::new("gpu-host").unwrap(),
        resources: Resources::new(1000, 1, 1, Some("g".into())).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: secs(max_wall_secs),
        labels: BTreeMap::new(),
        validation_run: None,
    }
}

fn open(provider: FakeProvider, clock: &ManualClock, scratch: &Scratch) -> Table {
    LeaseTable::open(provider, clock.clone(), settings(), scratch.file()).unwrap()
}

fn fresh(name: &str) -> (Table, ManualClock, Scratch) {
    let scratch = Scratch::new(name);
    let clock = clock();
    (open(fake(), &clock, &scratch), clock, scratch)
}

fn states(table: &Table) -> Vec<(LeaseId, LeaseState)> {
    table.list().into_iter().map(|l| (l.id, l.state)).collect()
}

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

/// Asserts that the same flow works on the static pool, that two acquires never get the
/// same host, and that a third is refused when every host is booked.
#[test]
fn two_acquires_never_share_a_host_on_the_pool() {
    let scratch = Scratch::new("pool");
    let pool = StaticPool::from_toml(ProviderId::new("pool").unwrap(), EXAMPLE_CONFIG).unwrap();
    let mut table = LeaseTable::open(pool, clock(), settings(), scratch.file()).unwrap();
    let wanted = || Request {
        resources: Resources::new(1000, 1, 1, Some("nvidia-a100".into())).unwrap(),
        ..request(3600)
    };
    let a = table.acquire(wanted(), None).unwrap();
    let b = table.acquire(wanted(), None).unwrap();
    assert_eq!((a.host.as_str(), b.host.as_str()), ("gpu-01", "gpu-02"));
    let third = table.acquire(wanted(), None).unwrap_err();
    assert!(matches!(
        third,
        AcquireError::Refused(RefusalCode::NoCapacity)
    ));
    assert_eq!(table.provider().booked().len(), 2);

    assert_eq!(table.release(&a.id).unwrap(), ReleaseOutcome::Freed);
    let again = table.acquire(wanted(), None).unwrap();
    assert_eq!(again.host.as_str(), "gpu-01");
    assert_eq!(again.id, LeaseId::new(1, 4));
    let capacity: Vec<HostCapacity> = table.provider().capacity();
    assert!(capacity.iter().all(|c| c.leased_to.is_some()));
}

/// Asserts that a lease lapses only when the clock passes its expiry and a tick runs:
/// before the tick a renew is refused as expired, the tick stops it on the provider and
/// reports it, and then the table has nothing.
#[test]
fn expiry_by_advancing_the_clock() {
    let (mut table, clock, _scratch) = fresh("expiry");
    let lease = table.acquire(request(3600), None).unwrap();
    clock.advance(Duration::from_millis(59_999));
    let quiet = table.tick();
    assert!(!quiet.changed());
    assert_eq!(table.list().len(), 1);

    clock.advance(Duration::from_millis(1));
    let err = table.renew(&lease.id, None).unwrap_err();
    assert!(matches!(err, RenewError::Expired(id) if id == lease.id));
    let report = table.tick();
    assert_eq!(report.expired, vec![lease.id]);
    assert_eq!(report.freed, vec![lease.id]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let gone = table.renew(&lease.id, None).unwrap_err();
    assert!(matches!(gone, RenewError::Unknown(_)));
}

/// Asserts that a renew never extends past acquisition plus the maximum wall time, and
/// that at that moment the lease expires whatever was renewed.
#[test]
fn renew_is_capped_at_the_max_wall_time() {
    let (mut table, clock, _scratch) = fresh("max_wall");
    let lease = table.acquire(request(100), Some(secs(60))).unwrap();
    clock.advance(secs(50));
    assert_eq!(
        table.renew(&lease.id, Some(secs(60))).unwrap(),
        at(T0 + 100_000)
    );
    assert_eq!(
        table.renew(&lease.id, Some(secs(60))).unwrap(),
        at(T0 + 100_000)
    );
    assert_eq!(table.list()[0].ends_at, at(T0 + 100_000));
    clock.advance(secs(50));
    let err = table.renew(&lease.id, Some(secs(60))).unwrap_err();
    assert!(matches!(err, RenewError::Expired(_)));
    assert_eq!(table.tick().expired, vec![lease.id]);
}

/// Asserts that a short max wall time caps the first expiry too, that a ttl above the
/// maximum is clamped to it, and that a zero ttl is refused on acquire and on renew.
#[test]
fn ttl_is_clamped_and_must_be_positive() {
    let (mut table, _clock, _scratch) = fresh("ttl");
    let short = table.acquire(request(10), None).unwrap();
    assert_eq!(short.expires_at, at(T0 + 10_000));
    let long = table.acquire(request(3600), Some(secs(1000))).unwrap();
    assert_eq!(long.expires_at, at(T0 + 120_000));
    let zero = table
        .acquire(request(3600), Some(Duration::ZERO))
        .unwrap_err();
    assert!(matches!(zero, AcquireError::TtlNotPositive));
    let zero = table.renew(&long.id, Some(Duration::ZERO)).unwrap_err();
    assert!(matches!(zero, RenewError::TtlNotPositive));
    assert_eq!(table.list().len(), 2);
}

/// Asserts that a refusal, by the start contract or by the provider, records nothing
/// and that a renew or release of an id the table never held is an unknown-id error.
#[test]
fn refusal_records_nothing() {
    let (mut table, _clock, scratch) = fresh("refusal");
    let mut bad = request(3600);
    bad.class = ClassId::new("other").unwrap();
    let err = table.acquire(bad, None).unwrap_err();
    assert!(matches!(
        err,
        AcquireError::Refused(RefusalCode::UnknownClass)
    ));
    let mut wide = request(3600);
    wide.resources = Resources::new(99_000, 1, 1, Some("g".into())).unwrap();
    let err = table.acquire(wide, None).unwrap_err();
    assert!(matches!(err, AcquireError::Refused(RefusalCode::NoFit)));
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("[[lease]]"));

    let nobody = LeaseId::new(7, 7);
    assert!(matches!(
        table.renew(&nobody, None),
        Err(RenewError::Unknown(_))
    ));
    assert!(matches!(
        table.release(&nobody),
        Err(ReleaseError::Unknown(_))
    ));
}

/// Asserts that a start that answers Unknown and landed is recorded pending, not
/// returned as held, and that a tick resolves it to held on the host the provider names.
#[test]
fn unknown_start_that_landed_resolves_to_held() {
    let (mut table, _clock, _scratch) = fresh("unknown_landed");
    table.provider_mut().script_unknown(Call::Start, true);
    let err = table.acquire(request(3600), None).unwrap_err();
    let id = LeaseId::new(1, 1);
    assert!(matches!(err, AcquireError::Unconfirmed(got) if got == id));
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);
    assert!(matches!(
        table.renew(&id, None),
        Err(RenewError::NotHeld { .. })
    ));

    let report = table.tick();
    assert_eq!(report.confirmed, vec![id]);
    assert_eq!(report.pending, 0);
    let info = &table.list()[0];
    assert_eq!(
        (info.state, info.host.clone()),
        (LeaseState::Held, Some(host("h1")))
    );
}

/// Asserts that a start that answered Unknown and did not land is dropped when the
/// provider answers Absent, and that an Unknown observation keeps the lease pending
/// and changes nothing.
#[test]
fn unknown_start_that_did_not_land_is_dropped() {
    let (mut table, _clock, _scratch) = fresh("unknown_lost");
    table.provider_mut().script_unknown(Call::Start, false);
    table.acquire(request(3600), None).unwrap_err();
    table.provider_mut().script_unknown(Call::Observe, false);
    let unsure = table.tick();
    assert_eq!((unsure.pending, unsure.changed()), (1, false));
    assert_eq!(table.list().len(), 1);

    let report = table.tick();
    assert_eq!(report.dropped, vec![LeaseId::new(1, 1)]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that a pending lease whose expiry passes before it resolves is stopped on
/// the provider, so a start that landed late does not leave a booking behind.
#[test]
fn pending_lease_past_expiry_is_stopped() {
    let (mut table, clock, _scratch) = fresh("pending_expiry");
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(request(3600), None).unwrap_err();
    assert_eq!(table.provider().booked().len(), 1);
    clock.advance(secs(60));
    let report = table.tick();
    assert_eq!(report.expired, vec![LeaseId::new(1, 1)]);
    assert_eq!(report.freed, report.expired);
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
    let mut table = LeaseTable::open(Odd(fake()), clock(), settings(), scratch.file()).unwrap();
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

/// Asserts that a restart restores leases with their states, never reuses an id, and
/// takes a new term while the sequence keeps growing.
#[test]
fn restart_keeps_leases_and_never_reuses_an_id() {
    let (mut table, clock, scratch) = fresh("restart");
    let held = table.acquire(request(3600), None).unwrap();
    table.provider_mut().script_unknown(Call::Start, false);
    table.acquire(request(3600), None).unwrap_err();
    let doomed = table.acquire(request(3600), None).unwrap();
    table.provider_mut().script_unknown(Call::Stop, false);
    table.release(&doomed.id).unwrap();
    let before = table.list();
    assert_eq!(before.len(), 3);

    let provider = table.into_provider();
    let mut table = open(provider, &clock, &scratch);
    assert_eq!(table.list(), before);
    let report = table.tick();
    assert_eq!((report.freed.len(), report.dropped.len()), (1, 1));
    let next = table.acquire(request(3600), None).unwrap();
    assert_eq!(next.id, LeaseId::new(2, 4));
    assert!(before.iter().all(|info| info.id != next.id));
    assert_eq!(table.list().len(), 2);
    assert_eq!(table.renew(&held.id, None).unwrap(), at(T0 + 60_000));
}

/// Asserts that a lease loaded as held is observed once at open: present stays held,
/// absent is dropped, and unknown keeps it.
#[test]
fn held_leases_are_observed_once_at_load() {
    let (mut table, clock, scratch) = fresh("load_observe");
    let a = table.acquire(request(3600), None).unwrap();
    let b = table.acquire(request(3600), None).unwrap();
    let mut provider = table.into_provider();

    provider.stop(&b.id);
    let table = open(provider, &clock, &scratch);
    assert_eq!(states(&table), vec![(a.id, LeaseState::Held)]);

    let mut provider = table.into_provider();
    provider.script_unknown(Call::Observe, false);
    let table = open(provider, &clock, &scratch);
    assert_eq!(states(&table), vec![(a.id, LeaseState::Held)]);
    let provider = table.into_provider();
    let table = open(provider, &clock, &scratch);
    assert_eq!(table.list().len(), 1);
    let fresh_provider = open(fake(), &clock, &scratch);
    assert!(fresh_provider.list().is_empty());
}

/// Asserts that the file is replaced whole after every change and has the documented
/// shape, with the caller's labels and the validation run.
#[test]
fn the_file_has_the_documented_shape() {
    let (mut table, _clock, scratch) = fresh("format");
    let mut asked = request(3600);
    asked.labels.insert("team".into(), "infra".into());
    asked.validation_run = Some("val-7".into());
    let lease = table.acquire(asked, None).unwrap();
    assert_eq!(lease.labels.get("team"), Some("infra"));
    assert_eq!(
        lease.labels.get("jobscheduler.dev/validation-run"),
        Some("val-7")
    );
    let text = fs::read_to_string(scratch.file()).unwrap();
    let expected = "version = 1\nterm = 1\nnext_sequence = 2\n\n[[lease]]\nterm = 1\n\
        sequence = 1\nstate = \"held\"\nhost = \"h1\"\nacquired_at_ms = 1000000\n\
        expires_at_ms = 1060000\nends_at_ms = 4600000\nvalidation_run = \"val-7\"\n\n\
        [lease.labels]\nteam = \"infra\"\n";
    assert_eq!(text, expected);
    assert!(!scratch.dir.join("leases.toml.tmp").exists());

    let provider = table.into_provider();
    let table = open(provider, &clock(), &scratch);
    assert_eq!(table.list()[0].labels, lease.labels);
}

/// Asserts that a file written by a later version, with fields and tables this build
/// does not know, loads.
#[test]
fn a_newer_file_with_unknown_fields_loads() {
    let scratch = Scratch::new("newer");
    let text = "version = 9\nterm = 4\nnext_sequence = 10\nflavour = \"new\"\n\n\
        [[lease]]\nterm = 4\nsequence = 9\nstate = \"held\"\nhost = \"h1\"\n\
        acquired_at_ms = 1000000\nexpires_at_ms = 1060000\nends_at_ms = 4600000\n\
        priority = 3\n\n[lease.extra]\nx = \"y\"\n";
    fs::write(scratch.file(), text).unwrap();
    let mut provider = fake();
    let spec = StartSpec::new(LeaseId::new(4, 9), request(60), &provider_classes());
    provider.start(spec.unwrap());
    let mut table = open(provider, &clock(), &scratch);
    assert_eq!(states(&table), vec![(LeaseId::new(4, 9), LeaseState::Held)]);
    let next = table.acquire(request(3600), None).unwrap();
    assert_eq!(next.id, LeaseId::new(5, 10));
    let rewritten = fs::read_to_string(scratch.file()).unwrap();
    assert!(rewritten.starts_with("version = 1\nterm = 5\nnext_sequence = 11\n"));
}

fn provider_classes() -> Vec<ResourceClass> {
    vec![ResourceClass::gpu_host()]
}

/// Asserts that a file that is not a lease file, a lease that is not valid, and a path
/// that cannot be read each fail open with an error naming the path, and none of them
/// opens an empty table.
#[test]
fn corrupt_files_are_errors_naming_the_path() {
    let scratch = Scratch::new("corrupt");
    let path = scratch.file();
    let cases = [
        "this is { not toml",
        "version = 1\nterm = 1\n",
        "version = 1\nterm = 1\nnext_sequence = 2\n\n[[lease]]\nterm = 1\nsequence = 1\n\
         state = \"held\"\nhost = \"BAD HOST\"\nacquired_at_ms = 1\nexpires_at_ms = 2\n\
         ends_at_ms = 3\n",
        "version = 1\nterm = 1\nnext_sequence = 2\n\n[[lease]]\nterm = 1\nsequence = 1\n\
         state = \"held\"\nacquired_at_ms = 1\nexpires_at_ms = 2\nends_at_ms = 3\n\
         [lease.labels]\n\"jobscheduler.dev/run\" = \"x\"\n",
    ];
    for text in cases {
        fs::write(&path, text).unwrap();
        let err = open_err(&path);
        assert!(matches!(err, OpenError::Corrupt { .. }), "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("lease file {}: corrupt: ", path.display()))
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
    scratch.clear();
    fs::create_dir(&path).unwrap();
    let err = open_err(&path);
    assert!(matches!(err, OpenError::Read { .. }));
    assert!(
        err.to_string()
            .starts_with(&format!("lease file {}: cannot read: ", path.display()))
    );
    fs::remove_dir(&path).unwrap();
}

fn open_err(path: &Path) -> OpenError {
    match LeaseTable::open(fake(), clock(), settings(), path) {
        Err(err) => err,
        Ok(_) => panic!("opened over {}", path.display()),
    }
}

/// Asserts that a file that cannot be written is an error at open and that, after the
/// directory vanishes, every change reports the failed save while the table keeps
/// its in-memory state and heals when the file can be written again.
#[test]
fn save_failures_are_reported_and_heal() {
    let scratch = Scratch::new("save_fail");
    let missing = scratch.dir.join("gone").join("leases.toml");
    let err = open_err(&missing);
    assert!(matches!(err, OpenError::Save(_)));
    assert!(err.to_string().contains("cannot write"));

    let clock = clock();
    let mut table = open(fake(), &clock, &scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    let broken = || {
        fs::remove_file(scratch.file()).unwrap();
        fs::remove_dir(&scratch.dir).unwrap();
    };
    broken();
    assert!(matches!(
        table.renew(&lease.id, None),
        Err(RenewError::Save(_))
    ));
    assert!(matches!(
        table.acquire(request(3600), None),
        Err(AcquireError::Save(_))
    ));
    assert_eq!(table.list().len(), 2);
    assert!(matches!(
        table.release(&lease.id),
        Err(ReleaseError::Save(_))
    ));
    assert_eq!(table.list()[0].state, LeaseState::Releasing);

    let pending = table.list()[1].id;
    let report = table.tick();
    assert_eq!(
        (report.freed, report.dropped),
        (vec![lease.id], vec![pending])
    );
    assert!(table.list().is_empty());
    let failed = report.save_error.unwrap();
    assert_eq!(failed.path, scratch.file());
    assert!(failed.to_string().contains("cannot write"));

    fs::create_dir_all(&scratch.dir).unwrap();
    table.acquire(request(3600), None).unwrap();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert_eq!(text.matches("[[lease]]").count(), 1);
}

/// Asserts that settings refuse a zero default and a default above the maximum, and
/// that the bounds read back.
#[test]
fn settings_are_checked() {
    assert_eq!(
        Settings::new(Duration::ZERO, secs(1)),
        Err(SettingsError::DefaultNotPositive)
    );
    assert_eq!(
        Settings::new(secs(2), secs(1)),
        Err(SettingsError::DefaultAboveMax)
    );
    let ok = Settings::new(secs(1), secs(1)).unwrap();
    assert_eq!((ok.default_ttl(), ok.max_ttl()), (secs(1), secs(1)));
    assert_eq!(
        SettingsError::DefaultNotPositive.to_string(),
        "the default ttl must be positive"
    );
    assert_eq!(
        SettingsError::DefaultAboveMax.to_string(),
        "the default ttl must not exceed the maximum ttl"
    );
}

/// Asserts that timestamps saturate instead of wrapping, that the manual clock moves
/// only when told to and clones share one reading, and that the system clock reads a
/// time after the epoch.
#[test]
fn clocks_and_timestamps() {
    let top = Timestamp::from_millis(u64::MAX);
    assert_eq!(top.millis(), i64::MAX as u64);
    assert_eq!(top.plus(Duration::MAX), top);
    assert_eq!(at(5).plus(Duration::from_millis(7)), at(12));

    let clock = ManualClock::new(at(10));
    let other = clock.clone();
    other.advance(Duration::from_millis(5));
    assert_eq!(clock.now(), at(15));
    clock.set(at(3));
    assert_eq!(other.now(), at(3));
    assert_eq!(ManualClock::default().now(), at(0));
    assert!(SystemClock.now().millis() > 0);
}

/// Asserts that each error and state prints a stable message.
#[test]
fn messages_are_stable() {
    let id = LeaseId::new(2, 3);
    let held = RenewError::NotHeld {
        id,
        state: LeaseState::Pending,
    };
    assert_eq!(held.to_string(), "lease 2-3 is pending, not held");
    assert_eq!(
        RenewError::Unknown(id).to_string(),
        "lease 2-3 is not known"
    );
    assert_eq!(RenewError::Expired(id).to_string(), "lease 2-3 has expired");
    assert_eq!(
        RenewError::TtlNotPositive.to_string(),
        "the ttl must be positive"
    );
    assert_eq!(
        AcquireError::TtlNotPositive.to_string(),
        "the ttl must be positive"
    );
    assert_eq!(
        AcquireError::Refused(RefusalCode::NoFit).to_string(),
        "the start was refused: no-fit"
    );
    assert_eq!(
        AcquireError::Unconfirmed(id).to_string(),
        "lease 2-3 is pending: the provider did not answer the start"
    );
    assert_eq!(
        ReleaseError::Unknown(id).to_string(),
        "lease 2-3 is not known"
    );
    let all = [LeaseState::Pending, LeaseState::Held, LeaseState::Releasing];
    let names: Vec<String> = all.iter().map(|s| s.to_string()).collect();
    assert_eq!(names, ["pending", "held", "releasing"]);
}
