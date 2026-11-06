//! The writer lock on SQLite: the `flock` on `<file>.lock` that a store holds while it is
//! open ([`StoreLock`]), taken once through [`WriterLock::acquire`], and adopted by
//! [`SqliteStore::open_locked`] so the file stays locked from before the open until the
//! store is dropped.

mod common;

use std::sync::Arc;

use common::scratch;
use js_state::sqlite::{Options, SqliteError, SqliteStore, StoreLock};
use js_state::{Clock, Cond, LockRefused, ManualClock, Store, Txn, WriterLock, WriterTarget};

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
// of the file's one name (a second writer naming the file another way would get its own).
#[test]
fn a_second_acquire_on_the_same_store_is_refused_as_held() {
    let dir = scratch("writer-lock-held");
    let mut held = acquire(&dir.join("state.db")).expect("the first acquire");
    assert_eq!(held.check(), Ok(()), "a SQLite lock cannot be lost");
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
