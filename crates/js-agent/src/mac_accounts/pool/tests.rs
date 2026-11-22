//! Tests of opening the pool, lending an account, filling it and retiring one.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::mpsc;

use super::*;
use crate::mac_accounts::tests::rig::{Rig, me, my_gid};
use crate::mac_accounts::{Proof, generated_uid};

/// The text of what an open refused for.
fn refusal(rig: &Rig, state: &Path) -> String {
    match Pool::with_settings(
        state,
        rig.settings.clone(),
        &BTreeSet::new(),
        rig.fake.clone(),
    ) {
        Ok(_) => panic!("the open was not refused"),
        Err(why) => why.to_string(),
    }
}

/// Asserts that opening the pool makes the ready accounts, one by one, each as the ledger
/// says before it is made: `made` and then `ready`, uids from the start of the range, names
/// from 1, each with its home and its record.
#[test]
fn opening_the_pool_makes_the_ready_accounts() {
    let rig = Rig::new("open", 5, 2);
    let pool = rig.open();
    assert_eq!(pool.ready_count(), 2);
    assert!(pool.held().is_empty());
    let (a, b) = (me(), me() + 1);
    assert_eq!(
        rig.lines(),
        [
            format!("made kbf-acct-1 {a}"),
            "ready kbf-acct-1".to_owned(),
            format!("made kbf-acct-2 {b}"),
            "ready kbf-acct-2".to_owned(),
        ]
    );
    assert!(rig.home(1).is_dir() && rig.home(2).is_dir());
    let users = rig.fake.state().users.clone();
    assert_eq!(users.len(), 2);
    assert_eq!(users["kbf-acct-2"].uid, b);
    assert_eq!(users["kbf-acct-2"].gid, my_gid());
    assert_eq!(users["kbf-acct-2"].home, rig.home(2));
    assert_eq!(rig.fake.take_calls()[0], "full_disk_access");
}

/// Asserts that an open without the grant is refused before anything is made, deleted or
/// even recorded (not the ledger file), and says what to do; that a grant the host could not
/// tell about is a host error; and that the Mac's open asks the same first.
#[test]
fn an_open_without_full_disk_access_makes_nothing() {
    let rig = Rig::new("grant", 3, 2);
    rig.fake.state().grant = Some(io::ErrorKind::PermissionDenied);
    let why = rig.refused();
    assert!(matches!(why, AccountError::NoFullDiskAccess), "{why:?}");
    assert!(
        why.to_string().contains("does not hold Full Disk Access")
            && why
                .to_string()
                .contains("js-agent-full-disk-access.mobileconfig"),
        "{why}"
    );
    assert_eq!(rig.fake.take_calls(), ["full_disk_access"]);
    assert!(
        !rig.dir.join("state/accounts").exists(),
        "no ledger was made"
    );

    rig.fake.state().grant = Some(io::ErrorKind::NotFound);
    let why = rig.refused();
    assert!(matches!(why, AccountError::Host { .. }), "{why:?}");
    assert_eq!(
        why.to_string(),
        "asking whether Full Disk Access is held: the privacy directory cannot be listed"
    );
    assert!(rig.fake.state().users.is_empty());

    rig.fake.state().grant = Some(io::ErrorKind::PermissionDenied);
    let with_host = Pool::with_host(
        &rig.dir.join("state"),
        Slots::new(700, 100).unwrap(),
        2,
        &BTreeSet::new(),
        rig.fake.clone(),
    );
    assert!(matches!(with_host, Err(AccountError::NoFullDiskAccess)));
    assert!(rig.fake.state().users.is_empty());
}

/// Asserts which state directories are refused, naming the directory: one that is not there,
/// a file, a link, one another owner owns, and one the group or others may write in (they
/// could replace the ledger); and that one that is the owner's alone to write is taken, with
/// any owner when none is named.
#[test]
fn the_state_directory_must_be_the_owners_and_closed_to_others() {
    let mut rig = Rig::new("state", 3, 0);
    let state = rig.dir.join("state");
    let named = |why: &str| format!("the account ledger in {}: {why}", state.display());

    let missing = rig.dir.join("missing");
    assert!(
        refusal(&rig, &missing).contains("No such file or directory"),
        "{}",
        refusal(&rig, &missing)
    );
    std::fs::write(rig.dir.join("a-file"), "").unwrap();
    assert!(refusal(&rig, &rig.dir.join("a-file")).ends_with("not a directory"));
    std::os::unix::fs::symlink(&state, rig.dir.join("a-link")).unwrap();
    assert!(refusal(&rig, &rig.dir.join("a-link")).ends_with("not a directory"));

    for mode in [0o775, 0o757, 0o777] {
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            refusal(&rig, &state),
            named(&format!("mode {mode:o} lets others write in it"))
        );
    }
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();

    rig.settings.state_owner = Some(me() + 1);
    assert_eq!(
        refusal(&rig, &state),
        named(&format!("owned by uid {}, not uid {}", me(), me() + 1))
    );
    rig.settings.state_owner = None;
    assert!(
        Pool::with_settings(
            &state,
            rig.settings.clone(),
            &BTreeSet::new(),
            rig.fake.clone()
        )
        .is_ok()
    );
    rig.settings.state_owner = Some(me());
    assert!(
        Pool::with_settings(
            &state,
            rig.settings.clone(),
            &BTreeSet::new(),
            rig.fake.clone()
        )
        .is_ok()
    );
    assert!(rig.fake.state().users.is_empty());
}

/// Asserts that a ledger that cannot be used refuses the open naming its file and line, and
/// nothing is made.
#[test]
fn a_ledger_that_cannot_be_read_refuses_the_open() {
    let rig = Rig::new("bad-ledger", 3, 2);
    rig.write_ledger("made kbf-acct-1 700\nbogus line\n");
    let why = rig.refused();
    assert!(matches!(why, AccountError::Ledger { .. }), "{why:?}");
    assert_eq!(
        why.to_string(),
        format!(
            "the account ledger in {}: line 2: unknown record \"bogus line\"",
            rig.dir.join("state/accounts").display()
        )
    );
    assert_eq!(rig.fake.take_calls(), ["full_disk_access"]);
}

/// Asserts that a job is lent the oldest ready account, recorded `lent` before it is
/// returned; that each account is lent once; and that with none ready one is made on the
/// spot, and says so.
#[test]
fn lend_takes_the_oldest_ready_account_and_makes_one_when_none_is_ready() {
    let rig = Rig::new("lend", 5, 2);
    let pool = rig.open();
    let first = pool.lend("job-1").unwrap();
    assert_eq!(
        first,
        Lent {
            account: Account {
                name: "kbf-acct-1".to_owned(),
                uid: me(),
                gid: my_gid(),
                generated_uid: generated_uid(me()),
            },
            made_now: false,
        }
    );
    assert_eq!(rig.lines().last().unwrap(), "lent kbf-acct-1 job-1");
    assert_eq!(pool.ready_count(), 1);
    let second = pool.lend("job-2").unwrap();
    assert_eq!(second.account.name, "kbf-acct-2");
    assert!(!second.made_now);
    assert_eq!(pool.ready_count(), 0);

    let third = pool.lend("job-3").unwrap();
    assert_eq!(third.account.name, "kbf-acct-3");
    assert_eq!(third.account.uid, me() + 2);
    assert!(third.made_now);
    assert_eq!(
        rig.lines()[6..],
        [
            format!("made kbf-acct-3 {}", me() + 2),
            "lent kbf-acct-3 job-3".to_owned(),
        ]
    );
    let lent = rig
        .lines()
        .iter()
        .filter(|line| line.starts_with("lent "))
        .count();
    assert_eq!(lent, 3, "each account is lent once");
}

/// Asserts that a job id that is not one to 63 of `a-z`, `0-9` and `-` is refused with
/// nothing recorded and no call to the host, and that an id that would forge another record
/// is among them.
#[test]
fn a_bad_job_id_is_refused_with_nothing_recorded() {
    let rig = Rig::new("bad-job", 3, 1);
    let pool = rig.open();
    let before = rig.ledger();
    rig.fake.take_calls();
    for bad in [
        "",
        "Job",
        "a b",
        "a_b",
        &"a".repeat(64),
        "x\nready kbf-acct-9",
    ] {
        let why = pool.lend(bad).unwrap_err();
        assert!(
            matches!(&why, AccountError::BadJob(job) if job == bad),
            "{bad:?}: {why:?}"
        );
    }
    assert_eq!(
        pool.lend("A").unwrap_err().to_string(),
        "job id \"A\" is not 1 to 63 characters of a-z, 0-9 and -"
    );
    assert_eq!(rig.ledger(), before);
    assert!(rig.fake.take_calls().is_empty());
    assert_eq!(pool.ready_count(), 1);
}

/// Asserts that a loan the ledger could not record is not made: the error names the ledger,
/// and the account stays ready for the next job.
#[test]
fn a_loan_the_ledger_cannot_record_is_not_made() {
    let rig = Rig::new("lend-ledger", 3, 1);
    let pool = rig.open();
    pool.ledger().fail_writes(&rig.dir.join("state/accounts"));
    let why = pool.lend("job-1").unwrap_err();
    assert!(matches!(why, AccountError::Ledger { .. }), "{why:?}");
    assert!(why.to_string().contains("state/accounts: "), "{why}");
    assert_eq!(pool.ready_count(), 1);
}

/// Asserts that an account made while a job waited for the maker is lent, not another made
/// beside it: the lend that has the maker looks again before it makes one.
#[test]
fn a_ready_account_made_while_waiting_is_lent_not_another() {
    let rig = Rig::new("lend-waited", 3, 1);
    let pool = rig.open();
    let lent = pool.lend_or_make("job-1").unwrap();
    assert_eq!(lent.account.name, "kbf-acct-1");
    assert!(!lent.made_now);
    let made = pool.lend_or_make("job-2").unwrap();
    assert_eq!(made.account.name, "kbf-acct-2");
    assert!(made.made_now);
}

/// Asserts that a refill makes accounts until the ready count is met and returns how many,
/// none when it is met, and fails with the account's making when one cannot be made.
#[test]
fn a_refill_makes_accounts_until_enough_are_ready() {
    let rig = Rig::new("refill", 6, 3);
    let pool = rig.open();
    pool.lend("job-1").unwrap();
    pool.lend("job-2").unwrap();
    assert_eq!(pool.ready_count(), 1);
    assert_eq!(pool.refill().unwrap(), 2);
    assert_eq!(pool.ready_count(), 3);
    assert_eq!(pool.refill().unwrap(), 0);
    pool.lend("job-3").unwrap();
    rig.fake.fail("create_account");
    let why = pool.refill().unwrap_err();
    assert!(
        matches!(&why, AccountError::Make { account, step: Step::Record, .. } if account == "kbf-acct-6"),
        "{why:?}"
    );
    assert_eq!(pool.ready_count(), 2);
}

/// Asserts that a Mac's settings are the numbers of the design: the uids it is given, group
/// staff, homes in /Users, the per-user folders, the places the sweep covers (the crontab and
/// at jobs; the shared folders and launchd's per-uid files), and a state directory root owns.
#[test]
fn a_macs_settings_are_the_designs() {
    let settings = Settings::macos(Slots::new(700, 100).unwrap(), 20);
    assert_eq!((*settings.range.start(), *settings.range.end()), (700, 799));
    assert_eq!(settings.gid, 20);
    assert_eq!(settings.ready, 20);
    assert_eq!(settings.homes, Path::new("/Users"));
    assert_eq!(settings.user_folders, Path::new("/private/var/folders"));
    assert_eq!(settings.state_owner, Some(0));
    assert_eq!(settings.schedules.named, ["/private/var/at/tabs/{name}"]);
    assert_eq!(
        settings.schedules.owned,
        [Path::new("/private/var/at/jobs")]
    );
    assert_eq!(
        settings.shared.named,
        [
            "/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist",
            "/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist",
        ]
    );
    assert_eq!(
        settings.shared.owned,
        [
            Path::new("/Users/Shared"),
            Path::new("/private/tmp"),
            Path::new("/private/var/tmp"),
        ]
    );
    let swept = settings.swept_places();
    assert_eq!(swept.named.len(), 3);
    assert_eq!(swept.owned.len(), 4);
}

/// Asserts what a retirement reports for an account with processes: the pids that were
/// signalled, each once; what was shown gone; and the times by the host's clock (the fake's,
/// which only the deletion of a record and the making of an account advance); and that the
/// ledger says `gone` after `lent`, and the pool is filled again.
#[test]
fn a_retirement_reports_what_it_killed_and_shows_gone() {
    let rig = Rig::new("retire", 4, 1);
    rig.fake.state().cost.insert("delete_account", 5);
    rig.fake.state().cost.insert("create_account", 7);
    let pool = rig.open();
    let lent = pool.lend("job-1").unwrap();
    rig.fake.add_process(me(), 41, "sleep");
    rig.fake.add_process(me(), 40, "node");
    let retired = pool.retire(lent.account).unwrap();
    assert_eq!(
        retired,
        Retired {
            killed: vec![40, 41],
            gone: Proof::shown(),
            delete_ms: 5,
            refill_ms: 7,
        }
    );
    assert_eq!(
        rig.lines()[2..],
        [
            "lent kbf-acct-1 job-1".to_owned(),
            "gone kbf-acct-1".to_owned(),
            format!("made kbf-acct-2 {}", me() + 1),
            "ready kbf-acct-2".to_owned(),
        ]
    );
    assert_eq!(pool.ready_count(), 1);
    assert!(!rig.home(1).exists());
    assert!(!rig.fake.state().users.contains_key("kbf-acct-1"));
}

/// Asserts that only a lent (or held) account can be retired, by its name and uid: a ready
/// one, one the ledger never made, one that is gone already, a name that is not the pool's
/// and a uid that is not the account's are `not lent`, and nothing is touched.
#[test]
fn only_a_lent_account_can_be_retired() {
    let rig = Rig::new("retire-refused", 4, 2);
    let pool = rig.open();
    let lent = pool.lend("job-1").unwrap().account;
    rig.fake.take_calls();
    let refused = |account: Account| {
        let held = pool.retire(account.clone()).unwrap_err();
        assert_eq!(held.account, account);
        assert_eq!((held.step, held.why.as_str()), (Step::Ledger, "not lent"));
        held
    };
    let spare = pool.account(2, me() + 1);
    refused(Account {
        uid: me() + 3,
        ..lent.clone()
    });
    refused(Account {
        name: "kbf-acct-9".to_owned(),
        ..lent.clone()
    });
    refused(Account {
        name: "root".to_owned(),
        ..lent.clone()
    });
    // A ready account is not lent.
    assert_eq!(pool.ready_count(), 1);
    refused(spare);
    assert!(rig.fake.take_calls().is_empty());
    pool.retire(lent.clone()).unwrap();
    let again = refused(lent);
    assert_eq!(
        again.to_string(),
        format!(
            "account kbf-acct-1 (uid {}) is held at ledger: not lent",
            me()
        )
    );
}

/// Asserts that a refill that fails after a retirement is logged and is no failure of the
/// retirement: the account was deleted, and the caller must know it.
#[test]
fn a_refill_that_fails_does_not_fail_the_retirement() {
    let rig = Rig::new("retire-refill", 4, 1);
    let pool = rig.open();
    let lent = pool.lend("job-1").unwrap();
    rig.fake.fail("create_account");
    let retired = pool.retire(lent.account).unwrap();
    assert_eq!(retired.gone, Proof::shown());
    assert_eq!(pool.ready_count(), 0);
    let log = rig.log.text();
    assert!(
        log.contains("filling the pool of accounts after retiring kbf-acct-1: making account kbf-acct-2: record"),
        "{log}"
    );
    assert_eq!(rig.lines().last().unwrap(), "gone kbf-acct-2");
}

/// Asserts that a job can be lent an account, and one made on the spot, while another's
/// account is being deleted, and that the deletion finishes afterwards: the pool locks only
/// its ledger, never across a call to the host. The deletion is held at its first kill by a
/// hook until the other thread has lent; no timing is involved.
#[test]
fn a_job_is_lent_an_account_while_another_is_being_deleted() {
    let rig = Rig::new("concurrent", 6, 2);
    let pool = rig.open();
    let first = pool.lend("job-1").unwrap().account;
    let (reached_tx, reached) = mpsc::channel::<()>();
    let (go, go_rx) = mpsc::channel::<()>();
    let mut held_once = false;
    rig.fake.on("kill_all", move || {
        if !held_once {
            held_once = true;
            reached_tx.send(()).unwrap();
            go_rx.recv().unwrap();
        }
    });
    let (pool, rig) = (&pool, &rig);
    // `move`: should an assertion fail, `go` is dropped, the hook's wait ends, and the scope
    // does not wait for ever on the deletion it holds.
    std::thread::scope(move |scope| {
        let retiring = scope.spawn(|| pool.retire(first));
        reached.recv().unwrap();
        // The deletion is mid-step: the ledger still has the account lent.
        assert_eq!(rig.lines().last().unwrap(), "lent kbf-acct-1 job-1");
        let second = pool.lend("job-2").unwrap();
        assert_eq!(second.account.name, "kbf-acct-2");
        assert!(!second.made_now);
        let third = pool.lend("job-3").unwrap();
        assert!(third.made_now, "none was ready, so one was made");
        go.send(()).unwrap();
        let retired = retiring.join().unwrap().unwrap();
        assert_eq!(retired.gone, Proof::shown());
    });
    assert!(rig.lines().contains(&"gone kbf-acct-1".to_owned()));
    assert!(!rig.fake.state().users.contains_key("kbf-acct-1"));
}

/// Asserts that an account made for a lend is never visible as ready: with none ready, the
/// first lend is held just before its account is recorded (the channel pauses it there);
/// at that moment the account is not ready, and a second lend that runs beside it makes an
/// account of its own. Each job ends with a different account, recorded lent to its own
/// job, and nothing is deleted. Catches a `ready` record that another lend takes before the
/// first job's `lent` is written, which then deletes the account the other job holds.
#[test]
fn an_account_made_for_a_lend_is_never_ready_to_another_lend() {
    let rig = Rig::new("lend-never-ready", 4, 0);
    let pool = rig.open();
    let (reached_tx, reached) = mpsc::channel::<()>();
    let (go, go_rx) = mpsc::channel::<()>();
    let mut held_once = false;
    *pool.before_record.lock().unwrap() = Some(Box::new(move |record| {
        if !held_once && matches!(record, Record::Lent(_, job) if job == "job-1") {
            held_once = true;
            reached_tx.send(()).unwrap();
            go_rx.recv().unwrap();
        }
    }));
    let (pool, rig) = (&pool, &rig);
    // `move`: should an assertion fail, `go` is dropped and the hook's wait ends.
    let (first, second) = std::thread::scope(move |scope| {
        let first = scope.spawn(|| pool.lend("job-1"));
        reached.recv().unwrap();
        assert_eq!(pool.ready_count(), 0, "the account is visible as ready");
        assert!(rig.lines().iter().all(|line| !line.starts_with("ready")));
        let second = scope.spawn(|| pool.lend("job-2"));
        go.send(()).unwrap();
        (
            first.join().unwrap().unwrap(),
            second.join().unwrap().unwrap(),
        )
    });
    assert_ne!(first.account, second.account);
    assert!(first.made_now && second.made_now);
    let lines = rig.lines();
    for (lent, job) in [(&first, "job-1"), (&second, "job-2")] {
        let line = format!("lent {} {job}", lent.account.name);
        assert!(lines.contains(&line), "{line} in {lines:?}");
    }
    assert!(lines.iter().all(|line| !line.starts_with("gone")));
    assert!(lines.iter().all(|line| !line.starts_with("ready")));
    assert_eq!(rig.fake.state().users.len(), 2);
}

/// Asserts that the next account of a slot finds the slot's per-user folder where the last one
/// left it, emptied: after a job fills the folder and the account is retired, the folder and
/// its top-level directories are still there, nothing is in them, and the next account is made
/// for the same slot (the same uid and `GeneratedUID`, a new name) with its own new home.
#[test]
fn the_next_account_of_a_slot_finds_its_folder_empty() {
    let rig = Rig::new("slot-again", 1, 0);
    let pool = rig.open();
    let first = pool.lend("job-1").unwrap().account;
    let hash = rig.folder("ab", "hash");
    std::fs::create_dir_all(hash.join("0/work/tree")).unwrap();
    std::fs::write(hash.join("0/work/tree/out"), "x").unwrap();
    std::fs::write(hash.join("T/scratch"), "x").unwrap();
    std::fs::write(rig.home(1).join("Library"), "x").unwrap();
    pool.retire(first.clone()).unwrap();
    assert!(!rig.home(1).exists(), "the home is deleted");
    assert!(hash.join("0").is_dir(), "the folder is kept");

    let second = pool.lend("job-2").unwrap().account;
    assert_eq!(
        (second.name.as_str(), second.uid, &second.generated_uid),
        ("kbf-acct-2", first.uid, &first.generated_uid)
    );
    let mut names: Vec<String> = std::fs::read_dir(&hash)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["0", "C", "T", "X"]);
    for top in ["0", "C", "T", "X"] {
        assert_eq!(
            std::fs::read_dir(hash.join(top)).unwrap().count(),
            0,
            "{top}"
        );
    }
    assert!(rig.home(2).is_dir());
    assert!(!rig.home(2).join("Library").exists(), "a new home, empty");
}
