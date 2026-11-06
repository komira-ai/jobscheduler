//! The writer lock on SQLite: the `flock` on `<file>.lock` that a store holds while it is
//! open ([`StoreLock`]), taken once through [`WriterLock::acquire`], and adopted by
//! [`SqliteStore::open_locked`] so the file stays locked from before the open until the
//! store is dropped; and [`WriterLock::probe`], which reads the token a holder writes into
//! `<file>.lock` and never takes the lock.

mod common;

use std::sync::Arc;

use common::scratch;
use js_state::sqlite::{Options, SqliteError, SqliteStore, StoreLock};
use js_state::{
    Clock, Cond, LockRefused, LockState, ManualClock, Store, Txn, WriterLock, WriterTarget,
};

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::default())
}

fn acquire(db: &std::path::Path) -> Result<WriterLock, LockRefused> {
    WriterLock::acquire(&WriterTarget::Sqlite(db.to_owned()))
}

// Catches a lock whose `flock` outlives the value (the next writer's start refused as held
// though the holder has ended).
#[test]
fn the_writer_lock_is_released_when_the_holder_ends() {
    let dir = scratch("writer-lock-released");
    let db = dir.join("state.db");
    let first = acquire(&db).expect("the first acquire");
    assert!(StoreLock::is_held(&db).expect("probe"), "held while alive");
    drop(first);
    assert!(
        !StoreLock::is_held(&db).expect("probe"),
        "free after the drop"
    );
    let again = acquire(&db).expect("acquired again after the drop");
    drop(again);
}

// Catches a lock that is not taken, and one taken beside the spelling of the path instead
// of the file's one name (a second writer naming the file another way would get its own),
// and a SQLite lock that claims a writer generation (there is no row to hold one).
#[test]
fn a_second_acquire_on_the_same_store_is_refused_as_held() {
    let dir = scratch("writer-lock-held");
    let mut held = acquire(&dir.join("state.db")).expect("the first acquire");
    assert_eq!(held.check(), Ok(()), "a SQLite lock cannot be lost");
    assert_eq!(held.generation(), None, "a SQLite store is not fenced");
    assert_eq!(
        acquire(&dir.join("state.db")).err(),
        Some(LockRefused::Held)
    );
    let other_spelling = dir.join(".").join("state.db");
    assert_eq!(acquire(&other_spelling).err(), Some(LockRefused::Held));
    assert_eq!(
        format!("{held:?}"),
        "WriterLock { backend: \"sqlite\", .. }"
    );
    drop(held);
}

// Catches an `open_locked` that takes a second `flock` (refused as `Locked` while the
// writer lock is held) instead of adopting the one it is given; and shows the plain open
// is refused while the writer lock is held, so the adopt path is the only one.
#[test]
fn open_locked_on_a_held_writer_lock_succeeds_where_open_is_refused_as_locked() {
    let dir = scratch("writer-lock-open-locked");
    let db = dir.join("state.db");
    let lock = acquire(&dir.join(".").join("state.db")).expect("acquired");
    let refused =
        SqliteStore::open(&db, clock(), Options::default()).expect_err("the plain open is refused");
    assert!(matches!(refused, SqliteError::Locked { .. }), "{refused:?}");
    let store = SqliteStore::open_locked(&db, lock, clock(), Options::default())
        .expect("the store opens under the lock");
    assert_eq!(store.epoch(), 1);
    store
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("reachable")
        .expect("commits");
    assert!(store.get("k").expect("reads").is_some());
}

// Catches an `open_locked` that accepts any lock (the store would run with its own file
// unlocked, beside a second writer), and one that keeps a refused lock held.
#[test]
fn open_locked_refuses_a_lock_taken_on_another_file() {
    let dir = scratch("writer-lock-other-file");
    let (first, second) = (dir.join("first.db"), dir.join("second.db"));
    let lock = acquire(&first).expect("acquired");
    let refused = SqliteStore::open_locked(&second, lock, clock(), Options::default())
        .expect_err("another file's lock is refused");
    // The file does not exist yet: its one name is its directory's, resolved, and its own.
    let own_lock = dir
        .path()
        .canonicalize()
        .expect("the dir")
        .join("second.db.lock");
    assert_eq!(
        refused.to_string(),
        format!(
            "state file {}: the writer lock given is not this file's lock ({})",
            second.display(),
            own_lock.display()
        )
    );
    assert!(
        !StoreLock::is_held(&first).expect("probe"),
        "the refused lock was released"
    );
    assert!(
        !StoreLock::is_held(&second).expect("probe"),
        "nothing was left holding the other file"
    );
}

// Catches a store opened by `open_locked` that lets the lock go while it is alive (a
// second writer could then start on the file), and one that keeps it after its drop.
#[test]
fn a_second_acquire_is_still_refused_while_the_store_opened_by_open_locked_is_alive() {
    let dir = scratch("writer-lock-store-alive");
    let db = dir.join("state.db");
    let lock = acquire(&db).expect("acquired");
    let store = SqliteStore::open_locked(&db, lock, clock(), Options::default()).expect("opens");
    assert_eq!(acquire(&db).err(), Some(LockRefused::Held));
    drop(store);
    let next = acquire(&db).expect("free once the store is dropped");
    drop(next);
}

// Catches a failure to try the lock reported as `Held` (a caller would wait for a holder
// that does not exist) or without the file it was about.
#[test]
fn a_lock_that_cannot_be_tried_is_a_failure_that_names_the_file() {
    let dir = scratch("writer-lock-failed");
    let missing = dir.join("absent").join("state.db");
    assert_eq!(
        acquire(&missing).err(),
        Some(LockRefused::Failed(format!(
            "{}: No such file or directory (os error 2)",
            missing.display()
        )))
    );
    let db = dir.join("state.db");
    std::fs::write(&db, b"").expect("the file");
    std::fs::hard_link(&db, dir.join("again.db")).expect("a second name");
    assert_eq!(
        acquire(&db).err().map(|refused| refused.to_string()),
        Some(format!(
            "the store's writer lock could not be taken: state file {} has 2 names (hard \
             links); give the file one name, so one lock covers it",
            db.display()
        ))
    );
}

fn probe(db: &std::path::Path) -> LockState {
    WriterLock::probe(&WriterTarget::Sqlite(db.to_owned())).expect("the probe")
}

/// The holder text of a held probe, which fails the test on `Free`.
fn holder_of(state: LockState) -> String {
    match state {
        LockState::Held { holder } => holder.as_str().to_owned(),
        LockState::Free => panic!("the lock reads as free"),
    }
}

/// The lock file of `db` in a directory with no links: the state file's name with `.lock`.
fn lock_file(db: &std::path::Path) -> std::path::PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".lock");
    name.into()
}

// Catches a probe that reads a missing directory or lock file as an error (a host where the
// server never ran), and a holder whose drop leaves its token behind (the next probe would
// name a holder that is gone while its process lives on).
#[test]
fn a_probe_of_a_lock_nobody_holds_is_free() {
    let dir = scratch("writer-lock-probe-free");
    assert_eq!(probe(&dir.join("absent").join("state.db")), LockState::Free);
    let db = dir.join("state.db");
    assert_eq!(probe(&db), LockState::Free, "no lock file yet");
    drop(acquire(&db).expect("acquired"));
    assert_eq!(
        std::fs::read(lock_file(&db)).expect("the lock file stays"),
        b"",
        "the drop emptied the token"
    );
    assert_eq!(probe(&db), LockState::Free, "an empty lock file");
}

// Catches a holder that records nothing (a probe would call a running writer free), one that
// records another process or keeps an earlier token's tail, a store opened without the
// writer lock that records nothing, and a probe that looks beside the spelling of the path
// instead of the file's one name.
#[test]
fn a_probe_of_a_held_lock_names_the_holding_process() {
    let dir = scratch("writer-lock-probe-held");
    let db = dir.join("state.db");
    std::fs::write(lock_file(&db), "2147483647@99999999999999999999999999\n").expect("stale");
    let held = acquire(&db).expect("acquired over a stale token");
    let holder = holder_of(probe(&db));
    let (pid, micros) = holder.split_once('@').expect("<pid>@<micros>");
    assert_eq!(pid, std::process::id().to_string());
    assert!(
        micros.len() == 16 && micros.bytes().all(|b| b.is_ascii_digit()),
        "{holder}"
    );
    assert_eq!(
        std::fs::read_to_string(lock_file(&db)).expect("the token"),
        format!("{holder}\n"),
        "the whole content is the new token"
    );
    assert_eq!(holder_of(probe(&dir.join(".").join("state.db"))), holder);
    drop(held);
    let (store, _) = common::open_manual(&dir, Options::default());
    assert_eq!(
        holder_of(probe(&db))
            .split_once('@')
            .map(|(pid, _)| pid.to_owned()),
        Some(std::process::id().to_string()),
        "a plain open records its holder too"
    );
    drop(store);
    assert_eq!(probe(&db), LockState::Free);
}

// Catches a probe that trusts a token left by a holder that ended without its drop (a crash):
// the host would wait on a writer that is gone. A token whose process id cannot be read is
// not shown stale, so it is not called free. Neither leaves the lock taken.
#[test]
fn a_probe_of_a_token_whose_process_is_gone_is_free() {
    let dir = scratch("writer-lock-probe-gone");
    let db = dir.join("state.db");
    // No process has this id: Linux pids stop at 2^22, macOS ones below 100000.
    std::fs::write(lock_file(&db), "2147483647@1762426458155219\n").expect("a crash's token");
    assert_eq!(probe(&db), LockState::Free);
    std::fs::write(lock_file(&db), "2147483647").expect("a token with no stamp");
    assert_eq!(probe(&db), LockState::Free);
    for unreadable in ["not-a-pid@1", "0@1", "-1@1", "99999999999@1"] {
        std::fs::write(lock_file(&db), unreadable).expect("an unreadable token");
        assert_eq!(holder_of(probe(&db)), unreadable);
    }
    drop(acquire(&db).expect("nothing took the lock"));
}

// Catches a probe that takes the lock to look (a writer starting in that instant would be
// refused as held, and a holder's token would be emptied by the probe's release), or one
// that changes the token: the holder keeps its lock, its token and its store's commits.
#[test]
fn probing_a_held_lock_does_not_disturb_its_holder() {
    let dir = scratch("writer-lock-probe-undisturbed");
    let db = dir.join("state.db");
    let mut lock = acquire(&db).expect("acquired");
    let before = std::fs::read(lock_file(&db)).expect("the token");
    let holder = holder_of(probe(&db));
    for _ in 0..3 {
        assert_eq!(holder_of(probe(&db)), holder);
    }
    assert_eq!(lock.check(), Ok(()));
    assert_eq!(std::fs::read(lock_file(&db)).expect("the token"), before);
    assert_eq!(acquire(&db).err(), Some(LockRefused::Held));
    let store = SqliteStore::open_locked(&db, lock, clock(), Options::default()).expect("opens");
    assert_eq!(holder_of(probe(&db)), holder, "the store adopted the token");
    store
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("reachable")
        .expect("commits");
    drop(store);
    assert_eq!(probe(&db), LockState::Free);
}

// Catches a lock file that cannot be read reported as free (the host would go on as if no
// writer ran) or as an error that does not name the state file.
#[test]
fn a_probe_that_cannot_read_the_lock_file_fails_and_names_the_file() {
    let dir = scratch("writer-lock-probe-failed");
    let db = dir.join("state.db");
    std::fs::create_dir(lock_file(&db)).expect("a directory where the lock file goes");
    let failed = WriterLock::probe(&WriterTarget::Sqlite(db.clone())).expect_err("unreadable");
    assert_eq!(
        failed.to_string(),
        format!(
            "the store's writer lock could not be probed: the lock of {}: Is a directory (os \
             error 21)",
            db.display()
        )
    );
}
