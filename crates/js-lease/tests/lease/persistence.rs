//! The file: restarts, its shape, newer and corrupt files, failed saves, the lock.

use std::fs;

use js_lease::{
    AcquireError, Booking, Boot, LeaseState, LeaseTable, OpenError, ReleaseError, RenewError,
    read_bookings,
};
use js_provider::{Call, Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_provider_fake::FakeProvider;
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

use crate::support::{
    Scratch, T0, Table, at, break_dir, clock, fake, fresh, host, open, open_err, open_with,
    provider_classes, request, secs, settings, states,
};

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
    drop(table);
    let empty = open(fake(), &clock, &scratch);
    assert!(empty.list().is_empty());
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
    let expected = "version = 1\nterm = 1\nnext_sequence = 2\nlease_count = 1\n\n[[lease]]\nterm = 1\n\
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
/// does not know, loads, and that the next save writes those fields back unchanged with
/// the higher version, so an older build in between destroys nothing.
#[test]
fn a_newer_file_with_unknown_fields_loads_and_keeps_them() {
    let scratch = Scratch::new("newer");
    let text = "version = 9\nterm = 4\nnext_sequence = 10\nlease_count = 1\nflavour = \"new\"\n\n\
        [[lease]]\nterm = 4\nsequence = 9\nstate = \"held\"\nhost = \"h1\"\n\
        acquired_at_ms = 1000000\nexpires_at_ms = 1060000\nends_at_ms = 4600000\n\
        priority = 3\n\n[lease.extra]\nx = \"y\"\n\n[policy]\nmode = \"strict\"\n";
    fs::write(scratch.file(), text).unwrap();
    let mut provider = fake();
    let spec = StartSpec::new(LeaseId::new(4, 9), request(60), &provider_classes());
    provider.start(spec.unwrap());
    let mut table = open(provider, &clock(), &scratch);
    assert_eq!(states(&table), vec![(LeaseId::new(4, 9), LeaseState::Held)]);
    let next = table.acquire(request(3600), None).unwrap();
    assert_eq!(next.id, LeaseId::new(5, 10));
    let rewritten = fs::read_to_string(scratch.file()).unwrap();
    assert!(rewritten.starts_with("version = 9\nterm = 5\nnext_sequence = 11\nlease_count = 2\n"));
    assert!(rewritten.contains("flavour = \"new\""));
    assert!(rewritten.contains("priority = 3"));
    assert!(rewritten.contains("[lease.extra]\nx = \"y\""));
    assert!(rewritten.contains("[policy]\nmode = \"strict\""));
}

/// Asserts that a file that is not a lease file, a lease that is not valid, a file whose
/// lease count or ids do not add up (cut short, a count too low, an id twice) and a path
/// that cannot be read each fail open with an error naming the path, and none of them
/// opens an empty table.
#[test]
fn corrupt_files_are_errors_naming_the_path() {
    let scratch = Scratch::new("corrupt");
    let path = scratch.file();
    let head =
        |count: u32| format!("version = 1\nterm = 1\nnext_sequence = 3\nlease_count = {count}\n");
    let held = "\n[[lease]]\nterm = 1\nsequence = 1\nstate = \"held\"\nhost = \"h1\"\n\
        acquired_at_ms = 1\nexpires_at_ms = 2\nends_at_ms = 3\n";
    let cases = [
        "this is { not toml".to_string(),
        "version = 1\nterm = 1\n".to_string(),
        "version = 1\nterm = 1\nnext_sequence = 2\n".to_string(),
        head(1) + &held.replace("h1", "BAD HOST"),
        head(1) + held + "[lease.labels]\n\"jobscheduler.dev/run\" = \"x\"\n",
        // Cut at a lease boundary: the count says two, one is there.
        head(2) + held,
        // A count below the leases present.
        head(0) + held,
        // The same id twice.
        head(2) + held + held,
    ];
    for text in cases {
        let text = text.as_str();
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

/// Asserts that a file that cannot be written is an error at open and that, after the
/// directory vanishes, every change reports the failed save while the table keeps
/// its in-memory state (a failed renew keeps its extended expiry) and heals when the
/// file can be written again.
#[test]
fn save_failures_are_reported_and_heal() {
    let scratch = Scratch::new("save_fail");
    fs::create_dir(scratch.dir.join("leases.toml.tmp")).unwrap();
    let err = open_err(&scratch.file());
    assert!(matches!(err, OpenError::Save(_)));
    assert!(err.to_string().contains("cannot write"));
    fs::remove_dir(scratch.dir.join("leases.toml.tmp")).unwrap();

    let clock = clock();
    let mut table = open(fake(), &clock, &scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    break_dir(&scratch);
    clock.advance(secs(10));
    assert!(matches!(
        table.renew(&lease.id, None),
        Err(RenewError::Save(_))
    ));
    assert_eq!(table.list()[0].expires_at, at(T0 + 70_000));
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

/// Asserts that a save that failed in renew is retried by the next tick even when the
/// tick changes nothing, so the file gets the renewed expiry without waiting for some
/// other change, and that a tick after that does not report an error.
#[test]
fn a_failed_save_is_retried_by_a_tick_that_changes_nothing() {
    let (mut table, clock, scratch) = fresh("retry_save");
    let lease = table.acquire(request(3600), None).unwrap();
    break_dir(&scratch);
    clock.advance(secs(10));
    assert!(matches!(
        table.renew(&lease.id, None),
        Err(RenewError::Save(_))
    ));
    assert!(table.tick().save_error.is_some());
    assert!(table.tick().save_error.is_some());

    fs::create_dir_all(&scratch.dir).unwrap();
    let report = table.tick();
    assert!(!report.changed());
    assert!(report.save_error.is_none());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("expires_at_ms = 1070000"));
    assert!(table.tick().save_error.is_none());
}

/// Asserts that a missing file is an error when the table is not asked to initialise
/// it, so a lost state volume is not a first boot, that nothing is created then, and
/// that the same call over an existing file resumes it.
#[test]
fn a_missing_file_is_an_error_unless_initialising() {
    let scratch = Scratch::new("missing");
    let path = scratch.file();
    let boot = Boot::Resume;
    let err = match LeaseTable::open(fake(), clock(), settings(), &path, boot) {
        Err(err) => err,
        Ok(_) => panic!("resumed over a missing file"),
    };
    assert!(matches!(err, OpenError::Missing { .. }));
    assert_eq!(
        err.to_string(),
        format!(
            "lease file {}: does not exist (only a first boot may create it)",
            path.display()
        )
    );
    assert!(!path.exists());

    let mut table = open(fake(), &clock(), &scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    let provider = table.into_provider();
    let resumed = LeaseTable::open(provider, clock(), settings(), &path, boot).unwrap();
    assert_eq!(states(&resumed), vec![(lease.id, LeaseState::Held)]);
}

/// Asserts that a second table over a file that is already open is refused with a
/// would-block lock error naming the lock file, leaves the first table and the file
/// alone, and that the lock is free again once the first table is gone; and that a lock
/// file that cannot be opened is an error too.
#[test]
fn a_second_table_over_the_same_file_is_refused() {
    let (mut table, clock, scratch) = fresh("locked");
    let lease = table.acquire(request(3600), None).unwrap();
    let before = fs::read_to_string(scratch.file()).unwrap();
    let err = open_err(&scratch.file());
    let lock = scratch.dir.join("leases.toml.lock");
    assert!(matches!(
        &err,
        OpenError::Lock { path, source }
            if *path == lock && source.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(err.to_string().starts_with(&format!(
        "lease file lock {}: cannot lock: ",
        lock.display()
    )));
    assert_eq!(fs::read_to_string(scratch.file()).unwrap(), before);
    assert_eq!(states(&table), vec![(lease.id, LeaseState::Held)]);

    let provider = table.into_provider();
    let table = open(provider, &clock, &scratch);
    assert_eq!(table.list().len(), 1);
    drop(table);

    scratch.clear();
    fs::create_dir(&lock).unwrap();
    assert!(matches!(open_err(&scratch.file()), OpenError::Lock { .. }));
    fs::remove_dir(&lock).unwrap();
}

/// A provider that reports every lease it observes on a different host from the one it
/// started it on.
struct Moved(FakeProvider);

impl Provider for Moved {
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
        self.0.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        let host = HostId::new("moved").unwrap();
        match self.0.observe(lease) {
            Observed::Present { .. } => Observed::Present { host },
            other => other,
        }
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.0.capacity()
    }
}

/// Asserts that a held lease observed present at open takes the host the provider
/// names now, not the one in the file.
#[test]
fn a_held_lease_takes_the_host_the_provider_names_at_open() {
    let (mut table, _clock, scratch) = fresh("rehost");
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(lease.host, host("h1"));
    let provider = table.into_provider();
    let table = open_with(Moved(provider), &scratch);
    assert_eq!(table.list()[0].host, Some(host("moved")));
}

/// Leaves a lease in the file, stops the process, puts `stale` where an interrupted save
/// left its temporary file, and opens again. Returns the table, the lease of the first
/// boot and the scratch directory.
fn reopen_over_stale_temp(name: &str, stale: &str) -> (Table, LeaseId, Scratch) {
    let (mut table, clock, scratch) = fresh(name);
    let lease = table.acquire(request(3600), None).unwrap();
    let provider = table.into_provider();
    let temp = scratch.dir.join("leases.toml.tmp");
    fs::write(&temp, stale).unwrap();

    let table = LeaseTable::open(provider, clock, settings(), scratch.file(), Boot::Resume);
    assert!(
        !temp.exists(),
        "the open save renamed the temporary file away"
    );
    (table.unwrap(), lease.id, scratch)
}

/// Asserts the crash after a temporary file was written and before the rename, when the
/// temporary file is a complete, valid document that disagrees with the real one (term
/// 50, no leases): the old file is what loads (its lease, its term plus one), the open
/// save replaces the temporary file, and the next ids follow the old file, not the stale
/// one.
#[test]
fn a_complete_stale_temp_file_is_ignored_and_replaced() {
    let stale = "version = 1\nterm = 50\nnext_sequence = 99\nlease_count = 0\n";
    let (mut table, id, scratch) = reopen_over_stale_temp("stale_complete", stale);
    assert_eq!(states(&table), vec![(id, LeaseState::Held)]);
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.starts_with("version = 1\nterm = 2\nnext_sequence = 2\nlease_count = 1\n"));
    let next = table.acquire(request(3600), None).unwrap();
    assert_eq!(next.id, LeaseId::new(2, 2));
    assert!(!scratch.dir.join("leases.toml.tmp").exists());
}

/// Asserts the crash in the middle of writing the temporary file, when what it holds is
/// cut off and not a document at all: it is ignored the same way, the open does not fail
/// on it, and the next save overwrites it.
#[test]
fn a_cut_off_stale_temp_file_is_ignored_and_replaced() {
    let stale = "version = 1\nterm = 50\nnext_sequence = 9";
    let (mut table, id, scratch) = reopen_over_stale_temp("stale_cut", stale);
    assert_eq!(states(&table), vec![(id, LeaseState::Held)]);
    let next = table.acquire(request(3600), None).unwrap();
    assert_eq!(next.id, LeaseId::new(2, 2));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("lease_count = 2\n"));
}

/// Asserts that the bookings of a file are the hosts it names, for a held lease and a
/// releasing one and not for a pending lease that has no host, in id order, and that
/// reading them takes no lock (the table that wrote the file is still open), leaves
/// the file byte for byte as it was and creates nothing.
#[test]
fn bookings_are_the_hosts_the_file_names() {
    let (mut table, _clock, scratch) = fresh("bookings");
    let held = table.acquire(request(3600), None).unwrap();
    table.provider_mut().script_unknown(Call::Start, false);
    table.acquire(request(3600), None).unwrap_err();
    let doomed = table.acquire(request(3600), None).unwrap();
    table.provider_mut().script_unknown(Call::Stop, false);
    table.release(&doomed.id).unwrap();
    let before = fs::read_to_string(scratch.file()).unwrap();

    let bookings = read_bookings(scratch.file()).unwrap();
    let expected = vec![
        Booking {
            lease: held.id,
            host: held.host,
            state: LeaseState::Held,
        },
        Booking {
            lease: doomed.id,
            host: doomed.host,
            state: LeaseState::Releasing,
        },
    ];
    assert_eq!(bookings, expected);
    assert_eq!(fs::read_to_string(scratch.file()).unwrap(), before);
    assert_eq!(table.list().len(), 3);
}

/// Asserts that a file that is not there books nothing and is not created, and that a
/// file the table would refuse (not TOML, a wrong lease count, an id twice) or cannot
/// read is the same error the open gives, naming the path.
#[test]
fn bookings_of_a_missing_file_are_none_and_of_a_bad_file_an_error() {
    let scratch = Scratch::new("bookings_bad");
    let path = scratch.file();
    assert_eq!(read_bookings(&path).unwrap(), vec![]);
    assert!(!path.exists());

    let head = "version = 1\nterm = 1\nnext_sequence = 3\n";
    let held = "\n[[lease]]\nterm = 1\nsequence = 1\nstate = \"held\"\nhost = \"h1\"\n\
        acquired_at_ms = 1\nexpires_at_ms = 2\nends_at_ms = 3\n";
    let cases = [
        "this is { not toml".to_string(),
        format!("{head}lease_count = 2\n{held}"),
        format!("{head}lease_count = 2\n{held}{held}"),
    ];
    for text in cases {
        fs::write(&path, &text).unwrap();
        let err = read_bookings(&path).unwrap_err();
        assert!(matches!(err, OpenError::Corrupt { .. }), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let err = read_bookings(&path).unwrap_err();
    assert!(matches!(err, OpenError::Read { .. }), "{err}");
    fs::remove_dir(&path).unwrap();
}
