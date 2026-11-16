//! A job store seen through a store whose writes also check a key
//! ([`crate::JobStore::over`], [`js_state::Checked`]).

use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::{Checked, Cond, Store, Txn};

use super::{host, rig, share};
use crate::JobStoreError;

/// A view whose store checks a key writes while the key holds the version it was made with, and
/// is `Stale` for every write once another writer has moved the key, with nothing written; the
/// job store it was made from, and reads through the view, are not held to the key. Catches a
/// view that writes after its claim was taken by another connection, and a view that is held to
/// the key for a read; and a write through the view moves the mark of the job store it was made
/// from, so a reader that cached its tables sees it.
pub fn a_view_over_a_checked_store_writes_only_while_its_key_holds<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_view_over_a_checked_store_writes_only_while_its_key_holds";
    let rig = rig(backend);
    let inner = rig.store.inner();
    inner
        .txn(Txn::new().put("node/host-a", "1", Cond::Any))
        .expect("reads")
        .expect("commits");
    let held = inner
        .get("node/host-a")
        .expect("reads")
        .expect("a row")
        .version;
    let view = rig.jobs.over(Arc::new(Checked::new(
        Arc::clone(&rig.store),
        "node/host-a",
        Cond::Version(held),
    )));
    let (first, second) = (rig.queue("v-first"), rig.queue("v-second"));

    let before = rig.jobs.written().expect("reads");
    view.place(&first, host("host-a"), share())
        .expect("a view writes while its key holds");
    assert_ne!(
        rig.jobs.written().expect("reads"),
        before,
        "{name}: a write through the view did not move the job store's mark"
    );
    inner
        .txn(Txn::new().put("node/host-a", "2", Cond::Any))
        .expect("reads")
        .expect("commits");
    let lost = view.place(&second, host("host-a"), share()).map(drop);
    assert_eq!(
        lost,
        Err(JobStoreError::Stale { id: second.clone() }),
        "{name}: a view wrote after its key moved"
    );
    assert_eq!(
        rig.jobs
            .get_job(&second)
            .expect("reads")
            .expect("a job")
            .state,
        crate::JobState::Queued,
        "{name}: a refused write changed the job"
    );
    assert!(
        view.get_job(&second)
            .expect("a read is not checked")
            .is_some()
    );
    rig.jobs
        .place(&second, host("host-a"), share())
        .expect("the job store the view was made from is not held to the key");
}
