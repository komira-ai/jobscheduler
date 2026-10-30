//! A panic in a caller's closure while the store's lock is held poisons it. The guards that
//! clean up as the panic unwinds must still run: a second panic in a drop would abort the
//! process.

#[path = "treestore/common/mod.rs"]
mod common;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Rig, sized, wanted};
use js_treestore::Config;

#[test]
fn a_panic_under_the_lock_unwinds_to_the_caller_and_the_guards_clean_up() {
    let rig = Rig::new();
    let (a, b) = (sized("f", 100, 1), sized("f", 100, 2));
    rig.have(&a);
    rig.have(&b);
    let armed = Arc::new(AtomicBool::new(false));
    let clock_now = rig.now.clone();
    let clock_armed = armed.clone();
    let store = rig.open(Config {
        budget_bytes: 100,
        clock: Arc::new(move || {
            assert!(!clock_armed.load(Ordering::SeqCst), "the clock panicked");
            clock_now.load(Ordering::SeqCst)
        }),
        ..Config::default()
    });
    store.ensure(&wanted(&a)).unwrap();
    // Making room for `b` reads the clock with the state lock held.
    armed.store(true, Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| store.ensure(&wanted(&b))));
    let payload = outcome.unwrap_err();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap();
    assert!(message.contains("the clock panicked"), "{message}");
    // The extraction's files were removed by its guard.
    assert!(rig.names("stage").is_empty());
    assert!(rig.names("tmp").is_empty());
}
