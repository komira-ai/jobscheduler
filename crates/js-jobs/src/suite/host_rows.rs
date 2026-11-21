//! The rows of a host that a placement's own transaction re-checks ([`crate::JobStore::place_claimed`]
//! and, for the rows its commit checks absent, [`crate::JobStore::place`] and
//! [`crate::JobStore::place_with`]): the host's taint (`taint/<host>`, and the taint its binding
//! holds), its scale-down mark (`hostmark/<host>`) and the host ports its live allocations hold;
//! the scale-down mark's own reads and writes; and [`crate::JobStore::drop_taints`] under a
//! concurrent write.

use std::collections::BTreeMap;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, Txn};

use super::{Rig, cancelled, full_spec, host, host_spec, rig, share};
use crate::codec::{DecodeError, FORMAT, LATER_FORMAT, encode_taint};
use crate::interfere::INJECTED;
use crate::record::ScaleMark;
use crate::{
    DeclaredPort, EndReason, HostTaint, JobId, JobSpec, JobState, JobStoreError, Placement,
};

/// The error an operation answers when its commit fails.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// Records the job `text`, a contained job that declares the host ports `ports`.
fn ported<S: Store>(rig: &Rig<S>, text: &str, ports: &[u16]) -> JobId {
    rig.queue_spec(
        text,
        JobSpec {
            ports: ports
                .iter()
                .map(|port| DeclaredPort {
                    host: *port,
                    container: 80,
                })
                .collect(),
            ..full_spec()
        },
    )
}

/// The binding row of a host as the server stores it: a JSON object holding `fields`.
fn binding(fields: &str) -> Vec<u8> {
    format!("{{\"version\":1{fields}}}").into_bytes()
}

/// Writes `entry`, an on-host index entry of an allocation row, past the job store.
fn stray_entry<S: Store>(rig: &Rig<S>, entry: &str) {
    rig.store
        .inner()
        .txn(Txn::new().put(entry, Vec::new(), Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// Whether the job `id` is still queued with no allocation.
fn untouched<S: Store>(rig: &Rig<S>, id: &JobId) -> bool {
    let job = rig.jobs.get_job(id).expect("reads").expect("a job");
    let allocation = rig.jobs.get_allocation(id).expect("reads");
    (job.state == JobState::Queued) & allocation.is_none()
}

/// A claimed placement on a host with a `taint/<host>` row is refused as tainted and writes
/// nothing, and so is one on a host whose binding holds a taint (a `taint_reason` that is
/// text); a binding with no reason, or a null one, does not refuse; a binding that cannot be
/// read is corrupt and nothing is written. Catches a claimed placement that ignores either
/// kind of taint, one that takes a binding without a reason for a taint, and one that places
/// on a host whose binding it cannot read.
pub fn a_claimed_placement_refuses_a_tainted_host<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_claimed_placement_refuses_a_tainted_host";
    let rig = rig(backend);
    let plain = Placement::default();
    let tainter = rig.queue_spec("ht-tainter", host_spec());
    rig.jobs
        .place(&tainter, host("host-a"), share())
        .expect("a host job is placed and taints its host");
    let contained = rig.queue("ht-contained");
    let before = rig.snapshot().0;
    let refused = |host_name: &str| {
        rig.jobs
            .place_claimed(&contained, &host(host_name), share(), &plain, None)
            .map(drop)
    };
    assert_eq!(
        refused("host-a"),
        Err(JobStoreError::HostTainted {
            id: contained.clone(),
            host: host("host-a")
        }),
        "{name}: a taint row did not refuse"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");

    rig.put_raw(
        "hosts/binding/host-b",
        binding(r#","taint_reason":"residue""#),
    );
    let before = rig.snapshot().0;
    assert_eq!(
        refused("host-b"),
        Err(JobStoreError::HostTainted {
            id: contained.clone(),
            host: host("host-b")
        }),
        "{name}: a binding's taint did not refuse"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");

    rig.put_raw("hosts/binding/host-d", b"not json".to_vec());
    let before = rig.snapshot().0;
    assert_eq!(
        refused("host-d"),
        Err(JobStoreError::Corrupt {
            key: "hosts/binding/host-d".to_owned(),
            why: DecodeError::BadValue { tag: 0 },
        }),
        "{name}: an unreadable binding did not refuse"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");
    assert!(untouched(&rig, &contained), "{name}: the job moved");

    rig.put_raw("hosts/binding/host-c", binding(r#","taint_reason":null"#));
    rig.jobs
        .place_claimed(&contained, &host("host-c"), share(), &plain, None)
        .expect("a binding whose reason is null does not refuse");
    let plain_binding = rig.queue("ht-plain");
    rig.put_raw("hosts/binding/host-e", binding(""));
    rig.jobs
        .place_claimed(&plain_binding, &host("host-e"), share(), &plain, None)
        .expect("a binding with no reason does not refuse");
    let bare = rig.queue("ht-bare");
    rig.jobs
        .place_claimed(&bare, &host("host-f"), share(), &plain, None)
        .expect("a host with no binding does not refuse");
}

/// A contained job placed with `place` or `place_with` on a host with a `taint/<host>` row is
/// stale (the commit checks the row absent) and writes nothing; once the row is dropped it is
/// placed. Catches a plain placement that puts a contained job on a tainted host.
pub fn a_plain_placement_of_a_contained_job_on_a_tainted_host_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_plain_placement_of_a_contained_job_on_a_tainted_host_is_stale";
    let rig = rig(backend);
    let a = host("host-a");
    let tainter = rig.queue_spec("pt-tainter", host_spec());
    rig.jobs
        .place(&tainter, a.clone(), share())
        .expect("a host job taints its host");
    let id = rig.queue("pt-contained");
    let before = rig.snapshot().0;
    let stale = Err(JobStoreError::Stale { id: id.clone() });
    assert_eq!(
        rig.jobs.place(&id, a.clone(), share()).map(drop),
        stale,
        "{name}: place"
    );
    assert_eq!(
        rig.jobs
            .place_with(&id, a.clone(), share(), Placement::default())
            .map(drop),
        stale,
        "{name}: place_with"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");
    rig.jobs
        .drop_taints(&[(a.clone(), tainter)])
        .expect("drops");
    rig.jobs
        .place(&id, a, share())
        .expect("a host whose taint is dropped is placed on");
}

/// A host with a `hostmark/<host>` row is placed on by no one: a claimed placement is refused
/// as marked, `place` and `place_with` are stale, nothing is written; a mark this build cannot
/// read refuses the same way; after the mark is deleted the host takes the job. Catches a
/// placement that ignores a scale-down mark (the claimed path's read or the commit's check) and
/// one that treats a mark it cannot read as no mark.
pub fn a_hostmark_refuses_every_placement<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_hostmark_refuses_every_placement";
    let rig = rig(backend);
    let plain = Placement::default();
    let a = host("host-a");
    let id = rig.queue("hm-1");
    assert_eq!(rig.jobs.mark_host(&a, ScaleMark::Draining, None), Ok(true));
    let before = rig.snapshot().0;
    let marked = Err(JobStoreError::HostMarked {
        id: id.clone(),
        host: a.clone(),
    });
    assert_eq!(
        rig.jobs
            .place_claimed(&id, &a, share(), &plain, None)
            .map(drop),
        marked,
        "{name}: place_claimed"
    );
    let stale = Err(JobStoreError::Stale { id: id.clone() });
    assert_eq!(
        rig.jobs.place(&id, a.clone(), share()).map(drop),
        stale,
        "{name}: place"
    );
    assert_eq!(
        rig.jobs
            .place_with(&id, a.clone(), share(), plain.clone())
            .map(drop),
        stale,
        "{name}: place_with"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");

    rig.put_raw("hostmark/host-b", vec![LATER_FORMAT]);
    assert_eq!(
        rig.jobs
            .place_claimed(&id, &host("host-b"), share(), &plain, None)
            .map(drop),
        Err(JobStoreError::HostMarked {
            id: id.clone(),
            host: host("host-b")
        }),
        "{name}: a mark this build cannot read did not refuse"
    );
    assert_eq!(
        rig.jobs.place(&id, host("host-b"), share()).map(drop),
        stale,
        "{name}: place over a mark this build cannot read"
    );

    let (_, at) = rig.jobs.read_host_mark(&a).expect("reads").expect("marked");
    assert_eq!(rig.jobs.unmark_host(&a, at), Ok(true));
    rig.jobs
        .place_claimed(&id, &a, share(), &plain, None)
        .expect("an unmarked host takes the job");
}

/// A claimed placement is refused when a live allocation on the host holds a host port the
/// job declares, naming the first such port, and writes nothing; another port, no port, or
/// another host is placed; once the allocation is freed its ports are free. Catches a
/// placement that lets two jobs hold one host port, and one that goes on counting the ports of
/// a freed allocation.
pub fn a_live_allocations_host_port_refuses_a_claimed_placement<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_live_allocations_host_port_refuses_a_claimed_placement";
    let rig = rig(backend);
    let plain = Placement::default();
    let a = host("host-a");
    let holder = ported(&rig, "pk-holder", &[8080, 9090]);
    rig.jobs.place(&holder, a.clone(), share()).expect("placed");
    let clash = ported(&rig, "pk-clash", &[7000, 9090, 8080]);
    let before = rig.snapshot().0;
    assert_eq!(
        rig.jobs
            .place_claimed(&clash, &a, share(), &plain, None)
            .map(drop),
        Err(JobStoreError::PortTaken {
            id: clash.clone(),
            host: a.clone(),
            port: 8080
        }),
        "{name}: a held port did not refuse"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");

    let other_port = ported(&rig, "pk-other", &[7000]);
    rig.jobs
        .place_claimed(&other_port, &a, share(), &plain, None)
        .expect("a port nobody holds is placed");
    let portless = ported(&rig, "pk-portless", &[]);
    rig.jobs
        .place_claimed(&portless, &a, share(), &plain, None)
        .expect("a job with no host port is placed");
    rig.jobs
        .place_claimed(&clash, &host("host-b"), share(), &plain, None)
        .map(drop)
        .expect("another host holds no such port");

    let freed = ported(&rig, "pk-freed", &[6000]);
    rig.jobs
        .place(&freed, host("host-c"), share())
        .expect("placed");
    let reuse = ported(&rig, "pk-reuse", &[6000]);
    assert!(
        matches!(
            rig.jobs
                .place_claimed(&reuse, &host("host-c"), share(), &plain, None),
            Err(JobStoreError::PortTaken { port: 6000, .. })
        ),
        "{name}: the live allocation's port was free"
    );
    rig.jobs.end_job(&freed, cancelled()).expect("ends");
    rig.jobs.free(&freed, EndReason::Cancelled).expect("frees");
    rig.jobs
        .place_claimed(&reuse, &host("host-c"), share(), &plain, None)
        .expect("a freed allocation's port is free");
}

/// The ports a host's allocations hold are counted from the rows its on-host entries point at,
/// and only from a live allocation of that host: an entry whose row is gone, an entry that
/// points at another host's allocation, and an entry that points at a freed allocation hold
/// nothing, and an entry that points at a row this build cannot read refuses the placement as
/// corrupt, writing nothing. Catches a port count that fails or invents a hold on a stray index
/// entry, and one that places on a host it cannot count.
pub fn stray_on_host_entries_hold_no_port_and_an_unreadable_row_refuses<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "stray_on_host_entries_hold_no_port_and_an_unreadable_row_refuses";
    let rig = rig(backend);
    let plain = Placement::default();
    let place = |job: &JobId, host_name: &str| {
        rig.jobs
            .place_claimed(job, &host(host_name), share(), &plain, None)
            .map(drop)
    };

    stray_entry(&rig, "idx/alloc/host/host-a/ghost");
    let gone = ported(&rig, "sx-gone", &[5000]);
    assert_eq!(
        place(&gone, "host-a"),
        Ok(()),
        "{name}: an entry with no row"
    );

    let elsewhere = ported(&rig, "sx-elsewhere", &[5001]);
    rig.jobs
        .place(&elsewhere, host("host-b"), share())
        .expect("placed");
    stray_entry(&rig, "idx/alloc/host/host-a/sx-elsewhere");
    let same_port = ported(&rig, "sx-same-port", &[5001]);
    assert_eq!(
        place(&same_port, "host-a"),
        Ok(()),
        "{name}: an entry of another host's allocation"
    );

    let ended = ported(&rig, "sx-ended", &[5002]);
    rig.jobs
        .place(&ended, host("host-c"), share())
        .expect("placed");
    rig.jobs.end_job(&ended, cancelled()).expect("ends");
    rig.jobs.free(&ended, EndReason::Cancelled).expect("frees");
    stray_entry(&rig, "idx/alloc/host/host-c/sx-ended");
    let after = ported(&rig, "sx-after", &[5002]);
    assert_eq!(
        place(&after, "host-c"),
        Ok(()),
        "{name}: an entry of a freed allocation"
    );

    rig.put_raw("alloc/sx-bad", b"not an allocation".to_vec());
    stray_entry(&rig, "idx/alloc/host/host-d/sx-bad");
    let blocked = ported(&rig, "sx-blocked", &[5003]);
    let before = rig.snapshot().0;
    let refused = place(&blocked, "host-d");
    assert!(
        matches!(
            refused,
            Err(JobStoreError::Corrupt { .. } | JobStoreError::UnknownRow { .. })
        ),
        "{name}: {refused:?}"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");
}

/// [`crate::JobStore::mark_host`], [`crate::JobStore::read_host_mark`],
/// [`crate::JobStore::unmark_host`] and [`crate::JobStore::host_marks`]: a mark is written at
/// its absence or at the version read and deleted at the version read, a write or delete at
/// another version changes nothing and answers `false`, a failed commit is the store's error,
/// the list holds every readable mark with its version, and a row this build cannot read (a
/// later build's, one that does not parse, one under a key that names no host) is left out of
/// the list but is an error from the read of its host. Catches a mark written over another's,
/// a delete of a mark rewritten since, and a list that fails or lies for one bad row.
pub fn host_marks_are_written_and_deleted_at_the_version_read<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "host_marks_are_written_and_deleted_at_the_version_read";
    let rig = rig(backend);
    let (a, b) = (host("host-a"), host("host-b"));
    assert_eq!(rig.jobs.host_marks(), Ok(BTreeMap::new()), "{name}: empty");
    assert_eq!(rig.jobs.read_host_mark(&a), Ok(None), "{name}: none");
    assert_eq!(
        rig.jobs.mark_host(&a, ScaleMark::Draining, None),
        Ok(true),
        "{name}: a first mark"
    );
    let (mark, first) = rig.jobs.read_host_mark(&a).expect("reads").expect("marked");
    assert_eq!(mark, ScaleMark::Draining, "{name}: the mark read");
    assert_eq!(
        rig.jobs.mark_host(&a, ScaleMark::Releasing, None),
        Ok(false),
        "{name}: a mark over one read absent"
    );
    assert_eq!(
        rig.jobs.read_host_mark(&a),
        Ok(Some((ScaleMark::Draining, first))),
        "{name}: a refused mark changed the row"
    );
    assert_eq!(
        rig.jobs.mark_host(&a, ScaleMark::Releasing, Some(first)),
        Ok(true),
        "{name}: a mark at the version read"
    );
    let (mark, second) = rig.jobs.read_host_mark(&a).expect("reads").expect("marked");
    assert_eq!((mark, second > first), (ScaleMark::Releasing, true));
    assert_eq!(
        rig.jobs.mark_host(&a, ScaleMark::Draining, Some(first)),
        Ok(false),
        "{name}: a mark at an old version"
    );
    assert_eq!(
        rig.jobs.mark_host(&b, ScaleMark::Draining, Some(first)),
        Ok(false),
        "{name}: a mark at a version of a row that is not there"
    );
    assert_eq!(rig.jobs.read_host_mark(&b), Ok(None));

    assert_eq!(rig.jobs.unmark_host(&a, first), Ok(false), "{name}: old");
    assert_eq!(rig.jobs.unmark_host(&b, first), Ok(false), "{name}: absent");
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.unmark_host(&a, second),
        Err(injected()),
        "{name}: a failed delete"
    );
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.mark_host(&b, ScaleMark::Draining, None),
        Err(injected()),
        "{name}: a failed mark"
    );
    let listed = BTreeMap::from([(a.clone(), (ScaleMark::Releasing, second))]);
    assert_eq!(
        rig.jobs.host_marks(),
        Ok(listed.clone()),
        "{name}: a failed write changed the list"
    );

    rig.put_raw("hostmark/host-later", vec![LATER_FORMAT]);
    rig.put_raw("hostmark/host-junk", vec![FORMAT]);
    rig.put_raw("hostmark/not a host", vec![FORMAT, 0, 1]);
    assert_eq!(
        rig.jobs.host_marks(),
        Ok(listed),
        "{name}: the list did not leave out the rows it cannot read"
    );
    assert_eq!(
        rig.jobs.read_host_mark(&host("host-later")),
        Err(JobStoreError::UnknownRow {
            key: "hostmark/host-later".to_owned()
        }),
        "{name}: a later build's mark"
    );
    assert!(
        matches!(
            rig.jobs.read_host_mark(&host("host-junk")),
            Err(JobStoreError::Corrupt { .. })
        ),
        "{name}: a mark that does not parse"
    );

    assert_eq!(rig.jobs.unmark_host(&a, second), Ok(true), "{name}: delete");
    assert_eq!(rig.jobs.read_host_mark(&a), Ok(None), "{name}: deleted");
}

/// [`crate::JobStore::drop_taints`] keeps a row that names another job than the one given
/// (and makes no commit for it), refuses a row it cannot read, and, when a write moves a row
/// between its read and its delete, reads again: a row rewritten for the same job is deleted
/// on the second round, one rewritten for another job is left. Catches a drop that deletes
/// another job's taint, one that gives up on a lost version, and one that deletes at a
/// version it did not read.
pub fn drop_taints_keeps_other_jobs_rows_and_reads_again_after_a_lost_version<B: Backend>(
    backend: &B,
) where
    B::Store: 'static,
{
    let name = "drop_taints_keeps_other_jobs_rows_and_reads_again_after_a_lost_version";
    let rig = rig(backend);
    let a = rig.queue_spec("dt-a", host_spec());
    let b = rig.queue_spec("dt-b", host_spec());
    rig.jobs.place(&a, host("host-a"), share()).expect("placed");
    rig.jobs.place(&b, host("host-b"), share()).expect("placed");

    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.drop_taints(&[(host("host-a"), b.clone())]),
        Ok(()),
        "{name}: another job's name"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "{name}: a drop that deletes nothing made a commit"
    );
    assert_eq!(rig.jobs.host_taints().map(|taints| taints.len()), Ok(2));

    // A write between the read and the delete, naming the same job: the delete is made again.
    let inner = rig.store.inner();
    let row = rig.raw("taint/host-a").expect("a taint row").value;
    rig.store.before_next(move || {
        inner
            .txn(Txn::new().put("taint/host-a", row, Cond::Any))
            .expect("reachable")
            .expect("commits");
    });
    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.drop_taints(&[(host("host-a"), a.clone())]),
        Ok(()),
        "{name}: a lost version"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 2,
        "{name}: the interfering write and one delete"
    );
    assert_eq!(
        rig.jobs
            .host_taints()
            .map(|taints| taints.keys().cloned().collect::<Vec<_>>()),
        Ok(vec![host("host-b")]),
        "{name}: the row was not deleted after the lost version"
    );

    // A write between the read and the delete that gives the host to another job's taint.
    let inner = rig.store.inner();
    let theirs = encode_taint(&HostTaint { job: a.clone() }).expect("encodes");
    rig.store.before_next(move || {
        inner
            .txn(Txn::new().put("taint/host-b", theirs, Cond::Any))
            .expect("reachable")
            .expect("commits");
    });
    assert_eq!(
        rig.jobs.drop_taints(&[(host("host-b"), b.clone())]),
        Ok(()),
        "{name}: a taint taken over"
    );
    assert_eq!(
        rig.jobs.host_taints(),
        Ok(BTreeMap::from([(host("host-b"), HostTaint { job: a })])),
        "{name}: another job's taint was deleted"
    );

    rig.put_raw("taint/host-bad", vec![FORMAT]);
    assert!(
        matches!(
            rig.jobs.drop_taints(&[(host("host-bad"), b)]),
            Err(JobStoreError::Corrupt { .. })
        ),
        "{name}: a row it cannot read"
    );
}
