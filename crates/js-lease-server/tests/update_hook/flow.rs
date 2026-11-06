//! A whole update over the fakes, step after step, as the updater runs it.

use js_lease_server::update_hook::{Outcome, Step};

use crate::support::{Rig, held, st};

/// Asserts that prepare, cutover, verify and cleanup, run in order on a server that runs 1.0.0
/// and holds the writer lock, each exit 0; that the only acts are the stop, the move of the
/// link to 2.0.0 and the start, in that order; and that the record is gone at the end.
#[test]
fn an_update_restarts_the_server_in_place() {
    let rig = Rig::new("flow-update");
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    assert_eq!(rig.calls(), Vec::<String>::new());
    assert_eq!(rig.record().unwrap().holder.as_deref(), Some("old"));
    rig.store.answer([held("old"), crate::support::free()]);
    let cut = rig.run(Step::Cutover);
    assert_eq!(cut.outcome, Outcome::Success, "{cut:?}");
    assert!(!cut.report.reversible);
    assert_eq!(
        rig.calls(),
        [
            "stop".to_owned(),
            rig.relink_to(&rig.staged()),
            "start".to_owned()
        ]
    );
    *rig.units.after_start.borrow_mut() = (st("active", "running"), 0);
    rig.store.answer([held("new")]);
    let verified = rig.run(Step::Verify);
    assert_eq!(verified.outcome, Outcome::Success, "{verified:?}");
    assert!(rig.record().unwrap().verify_ok);
    assert_eq!(rig.run(Step::Cleanup).outcome, Outcome::Success);
    assert_eq!(rig.record(), None);
    assert_eq!(rig.calls(), Vec::<String>::new());
}
