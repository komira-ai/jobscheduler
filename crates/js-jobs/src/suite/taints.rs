//! The taint table: a host-isolation job's placement puts `taint/<host>` in the transaction
//! that places it, a contained one writes none, a failed or lost placement leaves none, a host
//! is tainted by its first host job only, [`crate::JobStore::host_taints`] lists the table or
//! refuses a row that does not parse, the retention tick keeps every taint, and
//! [`crate::JobStore::drop_taints`] deletes only the rows it is given.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError};

use super::{Rig, full_spec, host, host_spec, rig, share, whole_host_spec};
use crate::codec::{DecodeError, FORMAT, field_of, job_host_tag, set_field};
use crate::interfere::INJECTED;
use crate::{
    AllocationStep, Booked, Constraints, EndReason, HostTaint, Isolation, JobId, JobSpec, JobState,
    JobStoreError,
};

/// A tag no build has used, standing for a field a later build adds to a taint row.
const LATER_TAG: u16 = 903;

/// The error an operation answers when its commit fails.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// The taint of a host by the job `id`.
fn taint_by(id: &JobId) -> HostTaint {
    HostTaint { job: id.clone() }
}

/// The whole-host spec, run on the host itself.
fn whole_host_job_spec() -> JobSpec {
    let spec = whole_host_spec();
    JobSpec {
        constraints: Constraints {
            isolation: Isolation::Host,
            ..spec.constraints.clone()
        },
        ..spec
    }
}

/// Records the host-isolation job `text`.
fn queue_host_job<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    rig.queue_spec(text, host_spec())
}

/// Placing a host-isolation job writes its job row, its allocation and the taint of its host
/// in one commit; the taint names the job.
pub fn a_host_isolation_placement_taints_its_host_in_the_same_commit<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_host_isolation_placement_taints_its_host_in_the_same_commit";
    let rig = rig(backend);
    let id = queue_host_job(&rig, "t-host");
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::new()),
        "{name}: a host was tainted before any placement"
    );
    let commits = rig.store.commit_stats().txns;
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("a host-isolation job is placed");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the placement and the taint took other than one commit"
    );
    for key in ["job/t-host", "alloc/t-host", "taint/host-a"] {
        assert!(
            rig.raw(key).is_some(),
            "{name}: the commit did not write {key}"
        );
    }
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([(host("host-a"), taint_by(&id))])),
        "{name}: the taint does not name the job"
    );
}

/// Placing a contained job writes no taint row and no field of isolation in the job's row.
pub fn a_contained_placement_writes_no_taint<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_contained_placement_writes_no_taint";
    let rig = rig(backend);
    let id = rig.queue_spec("t-contained", full_spec());
    let commits = rig.store.commit_stats().txns;
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("a contained job is placed");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the placement took other than one commit"
    );
    assert_eq!(
        rig.raw("taint/host-a"),
        None,
        "{name}: a contained placement tainted its host"
    );
    assert_eq!(
        rig.store.scan_prefix("taint/").expect("scans").len(),
        0,
        "{name}: a taint row exists"
    );
    let row = rig.raw("job/t-contained").expect("written").value;
    assert_eq!(
        field_of(&row, job_host_tag::ISOLATION),
        None,
        "{name}: a contained job's row holds the isolation field"
    );
    assert_eq!(
        rig.jobs
            .get_job(&id)
            .expect("reads")
            .map(|job| job.spec.constraints.isolation),
        Some(Isolation::Contained),
        "{name}: the isolation did not read back as contained"
    );
}

/// A host-isolation placement whose commit fails leaves the job queued, with no allocation and
/// no taint, and the same placement then succeeds.
pub fn a_failed_host_placement_writes_no_taint<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_failed_host_placement_writes_no_taint";
    let rig = rig(backend);
    let id = queue_host_job(&rig, "t-failed");
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.place(&id, host("host-a"), share()).map(drop),
        Err(injected()),
        "{name}: a failed commit is not the store's error"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.host),
        (JobState::Queued, None),
        "{name}: the failed placement changed the job"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id),
        Ok(None),
        "{name}: the failed placement left an allocation"
    );
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::new()),
        "{name}: the failed placement tainted the host"
    );
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("the retried placement commits");
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([(host("host-a"), taint_by(&id))])),
        "{name}: the retried placement did not taint the host"
    );
}

/// A second host-isolation job placed on a host a first one tainted fails stale, stays queued
/// with no allocation, and leaves the first taint as it was; the same job places on a host not
/// yet tainted.
pub fn a_second_host_job_on_a_tainted_host_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_second_host_job_on_a_tainted_host_is_stale";
    let rig = rig(backend);
    let first = queue_host_job(&rig, "t-first");
    rig.jobs
        .place(&first, host("host-a"), share())
        .expect("the first host job is placed");
    let taint = rig.raw("taint/host-a");
    let second = queue_host_job(&rig, "t-second");
    assert_eq!(
        rig.jobs.place(&second, host("host-a"), share()).map(drop),
        Err(JobStoreError::Stale { id: second.clone() }),
        "{name}: a second host job was placed on a tainted host"
    );
    let job = rig.jobs.get_job(&second).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.host),
        (JobState::Queued, None),
        "{name}: the refused placement changed the job"
    );
    assert_eq!(
        rig.jobs.get_allocation(&second),
        Ok(None),
        "{name}: the refused placement left an allocation"
    );
    assert_eq!(
        rig.raw("taint/host-a"),
        taint,
        "{name}: the first taint was rewritten"
    );
    rig.jobs
        .place(&second, host("host-b"), share())
        .expect("a host not yet tainted takes the job");
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([
            (host("host-a"), taint_by(&first)),
            (host("host-b"), taint_by(&second)),
        ])),
        "{name}: each host is not tainted by its own job"
    );
}

/// A host-isolation job that needs the whole host taints it the same way, in one commit.
pub fn a_whole_host_job_in_host_isolation_taints_its_host<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_whole_host_job_in_host_isolation_taints_its_host";
    let rig = rig(backend);
    let id = rig.queue_spec("t-whole", whole_host_job_spec());
    let commits = rig.store.commit_stats().txns;
    let (_, allocation) = rig
        .jobs
        .place(&id, host("host-w"), Booked::WholeHost)
        .expect("a whole-host job is placed");
    assert_eq!(
        (allocation.booked, allocation.step),
        (Booked::WholeHost, AllocationStep::Booked),
        "{name}: the allocation does not book the whole host"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the placement and the taint took other than one commit"
    );
    assert!(
        rig.raw("taint/host-w").is_some(),
        "{name}: the commit did not write the taint"
    );
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([(host("host-w"), taint_by(&id))])),
        "{name}: the whole host was not tainted by the job"
    );
}

/// The error a listing answers for a row that does not parse.
fn refused(key: &str, why: DecodeError) -> Result<(), JobStoreError> {
    Err(JobStoreError::Corrupt {
        key: key.to_owned(),
        why,
    })
}

/// `host_taints` lists every taint row by host, skips a tag it does not know, and answers an
/// error naming the key for a row that does not parse, one under a key that names no host and
/// one a later build wrote.
pub fn host_taints_lists_every_taint_and_refuses_a_bad_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "host_taints_lists_every_taint_and_refuses_a_bad_row";
    let rig = rig(backend);
    let a = queue_host_job(&rig, "t-list-a");
    let b = queue_host_job(&rig, "t-list-b");
    rig.jobs.place(&a, host("host-a"), share()).expect("placed");
    rig.jobs.place(&b, host("host-b"), share()).expect("placed");
    let both = BTreeMap::from([
        (host("host-a"), taint_by(&a)),
        (host("host-b"), taint_by(&b)),
    ]);
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(both.clone()),
        "{name}: a taint is missing from the list"
    );

    let known = rig.raw("taint/host-a").expect("written").value;
    rig.put_raw(
        "taint/host-c",
        set_field(&known, LATER_TAG, &[1]).expect("the row parses"),
    );
    let mut with_later = both.clone();
    with_later.insert(host("host-c"), taint_by(&a));
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(with_later),
        "{name}: a field this build does not know was not skipped"
    );
    rig.delete_raw("taint/host-c");

    rig.put_raw("taint/host-d", vec![FORMAT, 0]);
    assert_eq!(
        rig.jobs.host_taints().map(drop),
        refused("taint/host-d", DecodeError::Truncated),
        "{name}: a row that does not parse was not refused by name"
    );
    rig.delete_raw("taint/host-d");

    rig.put_raw("taint/bad host", known);
    assert_eq!(
        rig.jobs.host_taints().map(drop),
        refused("taint/bad host", DecodeError::WrongKey),
        "{name}: a row under a key that names no host was not refused by name"
    );
    rig.delete_raw("taint/bad host");

    rig.put_raw("taint/host-e", vec![FORMAT + 1]);
    assert_eq!(
        rig.jobs.host_taints().map(drop),
        Err(JobStoreError::UnknownRow {
            key: "taint/host-e".to_owned(),
        }),
        "{name}: a row a later build wrote was not refused by name"
    );
    rig.delete_raw("taint/host-e");
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(both),
        "{name}: the list is not what it was"
    );
}

/// The retention tick deletes an ended host job and its freed allocation past their windows and
/// leaves the taint of the host.
pub fn the_retention_tick_keeps_taint_rows<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "the_retention_tick_keeps_taint_rows";
    let rig = rig(backend);
    let id = queue_host_job(&rig, "t-kept");
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("placed");
    rig.jobs
        .end_and_free(&id, super::cancelled(), EndReason::Cancelled)
        .expect("ends and frees");
    rig.clock.advance(Duration::from_secs(60 * 24 * 60 * 60));
    let deleted = rig.jobs.tick().expect("the tick runs");
    assert_eq!(
        deleted,
        vec![id.clone()],
        "{name}: the job was not past its retention"
    );
    assert_eq!(
        (rig.raw("job/t-kept"), rig.raw("alloc/t-kept")),
        (None, None),
        "{name}: the tick left the ended job or its allocation"
    );
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([(host("host-a"), taint_by(&id))])),
        "{name}: the tick deleted the taint"
    );
}

/// [`crate::JobStore::drop_taints`] deletes the rows of the hosts it is given in one commit,
/// passes over a host with no row and leaves every other row; no hosts make no commit, and a
/// failed commit deletes nothing.
pub fn drop_taints_deletes_the_named_rows_in_one_commit<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "drop_taints_deletes_the_named_rows_in_one_commit";
    let rig = rig(backend);
    let a = queue_host_job(&rig, "t-drop-a");
    let b = queue_host_job(&rig, "t-drop-b");
    rig.jobs.place(&a, host("host-a"), share()).expect("placed");
    rig.jobs.place(&b, host("host-b"), share()).expect("placed");
    let commits = rig.store.commit_stats().txns;
    assert_eq!(rig.jobs.drop_taints(&[]), Ok(()));
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "{name}: no hosts made a commit"
    );

    rig.store.fail_next();
    assert_eq!(
        rig.jobs.drop_taints(&[host("host-a")]),
        Err(injected()),
        "{name}: a failed commit is not the store's error"
    );
    assert_eq!(
        rig.jobs.host_taints().map(|taints| taints.len()),
        Ok(2),
        "{name}: a failed commit deleted a taint"
    );

    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.drop_taints(&[host("host-a"), host("host-z")]),
        Ok(())
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the deletes took other than one commit"
    );
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([(host("host-b"), taint_by(&b))])),
        "{name}: the wrong rows were deleted"
    );
}
