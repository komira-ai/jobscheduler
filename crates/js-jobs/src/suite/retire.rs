//! The operator's retirement of a host's row ([`crate::JobStore::retire_node`]): a lost host's
//! row is deleted, so a pool at its cap admits a new id, while a live host's row, and the row
//! of a host that holds or is owed work, are kept.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError};

use super::pools::hello_in;
use super::{Rig, cancelled, host, rig, share};
use crate::codec::{encode_cordon, encode_taint};
use crate::interfere::INJECTED;
use crate::record::ScaleMark;
use crate::{EndReason, HostTaint, JobStoreError, Kept, Retire};

/// `secs` seconds.
fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// The grace the tests judge a row live by.
const GRACE: Duration = Duration::from_secs(30);

/// Says the hello of `name` on `rig`, as a host of `pool-a`.
fn hello_of<S: Store>(rig: &Rig<S>, name: &str) {
    rig.jobs
        .hello_node(&host(name), &hello_in("pool-a"))
        .expect("a hello");
}

/// Whether the row of `name` is there.
fn has_row<S: Store>(rig: &Rig<S>, name: &str) -> bool {
    rig.jobs.read_node(&host(name)).expect("reads").is_some()
}

/// A pool at its cap of one refuses a new id while the row of its one host stands; that row is
/// kept while it is heard within the grace (the boundary included) and is retired once it is
/// silent past it, and then the pool admits the new id; a second retirement of the same host
/// finds no row and deletes nothing. Catches a retirement of a live host, a full pool that
/// still counts a retired row, and a repeat that fails or writes.
pub fn retire_node_deletes_a_lost_row_so_a_full_pool_admits_a_new_id<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "retire_node_deletes_a_lost_row_so_a_full_pool_admits_a_new_id";
    let rig = rig(backend);
    let (old, new) = (host("node-old"), host("node-new"));
    let hello = hello_in("pool-a");
    rig.jobs
        .hello_node_in_pool(&old, &hello, 1)
        .expect("below the cap");
    assert_eq!(
        rig.jobs.hello_node_in_pool(&new, &hello, 1).map(drop),
        Err(JobStoreError::PoolFull {
            host: new.clone(),
            pool: "pool-a".to_owned(),
        }),
        "{name}: a full pool admitted a new id"
    );

    let heard = rig
        .jobs
        .read_node(&old)
        .expect("reads")
        .expect("a row")
        .record
        .heard_at;
    let now = rig.store.now().expect("the store's clock");
    rig.clock.advance(heard.saturating_add(GRACE).since(now));
    assert_eq!(
        rig.jobs.retire_node(&old, GRACE),
        Ok(Retire::Refused(Kept::Live)),
        "{name}: a row heard exactly the grace ago"
    );
    assert!(has_row(&rig, "node-old"), "{name}: a live row was deleted");

    rig.clock.advance(secs(1));
    assert_eq!(
        rig.jobs.retire_node(&old, GRACE),
        Ok(Retire::Retired),
        "{name}: a lost row that holds nothing"
    );
    assert!(!has_row(&rig, "node-old"), "{name}: the row was kept");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.retire_node(&old, GRACE),
        Ok(Retire::Absent),
        "{name}: a repeat"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a repeat wrote");
    let admitted = rig.jobs.hello_node_in_pool(&new, &hello, 1);
    assert_eq!(
        admitted.map(|row| row.record.epoch),
        Ok(1),
        "{name}: the new id after the retirement"
    );
}

/// A lost host's row is kept, with the reason, while the host holds a live allocation (a job
/// placed there), a `taint/<host>` row, a binding with a taint, a cordon or a scale-down mark,
/// and an allocation that holds a job and a taint is refused for the job; a host whose only
/// allocation was freed is retired. Catches a retirement under a job the host still holds or a
/// taint it still owes checks for, one that counts a freed allocation as held, and a refusal
/// that names the wrong reason.
pub fn retire_node_keeps_the_row_of_a_host_that_holds_or_is_owed_work<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "retire_node_keeps_the_row_of_a_host_that_holds_or_is_owed_work";
    let rig = rig(backend);
    let kept = [
        ("host-alloc", Kept::HoldsJobs),
        ("host-both", Kept::HoldsJobs),
        ("host-taint", Kept::Tainted),
        ("host-binding", Kept::Tainted),
        ("host-cordon", Kept::Cordoned),
        ("host-mark", Kept::Marked),
    ];
    for (text, _) in &kept {
        hello_of(&rig, text);
    }
    hello_of(&rig, "host-freed");
    let live = rig.queue("rt-live");
    rig.jobs
        .place(&live, host("host-alloc"), share())
        .expect("placed");
    let both = rig.queue("rt-both");
    rig.jobs
        .place(&both, host("host-both"), share())
        .expect("placed");
    let ended = rig.queue("rt-ended");
    rig.jobs
        .place(&ended, host("host-freed"), share())
        .expect("placed");
    rig.jobs.end_job(&ended, cancelled()).expect("ends");
    rig.jobs.free(&ended, EndReason::Cancelled).expect("frees");
    for (key, job) in [("taint/host-taint", &live), ("taint/host-both", &both)] {
        let taint = encode_taint(&HostTaint { job: job.clone() }).expect("encodes");
        rig.put_raw(key, taint);
    }
    rig.put_raw(
        "hosts/binding/host-binding",
        br#"{"version":1,"taint_reason":"residue"}"#.to_vec(),
    );
    rig.put_raw("cordon/host-cordon", encode_cordon(None).expect("encodes"));
    assert_eq!(
        rig.jobs
            .mark_host(&host("host-mark"), ScaleMark::Draining, None),
        Ok(true)
    );
    rig.clock.advance(GRACE + secs(1));

    let before = rig.snapshot().0;
    for (text, reason) in kept {
        assert_eq!(
            rig.jobs.retire_node(&host(text), GRACE),
            Ok(Retire::Refused(reason)),
            "{name}: the answer for {text}"
        );
    }
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");
    assert_eq!(
        rig.jobs.retire_node(&host("host-freed"), GRACE),
        Ok(Retire::Retired),
        "{name}: a host whose allocation was freed"
    );
    assert!(!has_row(&rig, "host-freed"), "{name}: the freed host's row");
}

/// A beat that lands between the read and the delete moves the row, and the retirement reads it
/// again and finds the host live, keeping the row; a row another server deleted between the
/// read and the delete is absent; a failed commit is the store's error and deletes nothing, and
/// so is a clock that cannot be read; a binding that cannot be read keeps the row and is an
/// error naming it. Catches a retirement that deletes a row a beat has just refreshed, one that
/// reports a row it did not delete, and one that judges on a binding it could not read.
pub fn retire_node_judges_again_a_row_that_moved_or_went_and_deletes_nothing_on_a_failure<
    B: Backend,
>(
    backend: &B,
) where
    B::Store: 'static,
{
    let name = "retire_node_judges_again_a_row_that_moved_or_went_and_deletes_nothing_on_a_failure";
    let rig = rig(backend);
    let a = host("host-a");
    let epoch = rig
        .jobs
        .hello_node(&a, &hello_in("pool-a"))
        .expect("a hello")
        .record
        .epoch;
    rig.clock.advance(GRACE + secs(1));

    let other = rig.other();
    let beater = a.clone();
    rig.store.before_next(move || {
        other
            .beat_node(&beater, epoch, None, |row| row.state.clone())
            .expect("a beat");
    });
    assert_eq!(
        rig.jobs.retire_node(&a, GRACE),
        Ok(Retire::Refused(Kept::Live)),
        "{name}: a beat landed after the read"
    );
    assert!(
        has_row(&rig, "host-a"),
        "{name}: the beaten row was deleted"
    );

    rig.clock.advance(GRACE + secs(1));
    let other = rig.other();
    let gone = rig.jobs.read_node(&a).expect("reads").expect("a row");
    let deleter = a.clone();
    rig.store.before_next(move || {
        assert_eq!(other.forget_node(&deleter, gone.version), Ok(true));
    });
    assert_eq!(
        rig.jobs.retire_node(&a, GRACE),
        Ok(Retire::Absent),
        "{name}: another server deleted the row first"
    );

    hello_of(&rig, "host-b");
    rig.clock.advance(GRACE + secs(1));
    let b = host("host-b");
    let failed = Err(JobStoreError::Store(StoreError::Failed(
        INJECTED.to_owned(),
    )));
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.retire_node(&b, GRACE),
        failed,
        "{name}: a failed commit"
    );
    assert!(has_row(&rig, "host-b"), "{name}: a failed commit deleted");

    rig.put_raw("hosts/binding/host-b", b"not json".to_vec());
    assert!(
        matches!(
            rig.jobs.retire_node(&b, GRACE),
            Err(JobStoreError::Corrupt { key, .. }) if key == "hosts/binding/host-b"
        ),
        "{name}: a binding that cannot be read"
    );
    assert!(
        has_row(&rig, "host-b"),
        "{name}: the row of an unreadable binding"
    );

    rig.store.fail_clock(true);
    assert_eq!(
        rig.jobs.retire_node(&b, GRACE),
        failed,
        "{name}: a clock that cannot be read"
    );
    rig.store.fail_clock(false);
}
