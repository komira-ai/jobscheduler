//! More of the rows' compatibility (B8): a field a later build added survives every rewrite,
//! a row this build cannot read refuses every write, a reserved label a later build sets is an
//! unknown row, and a row under another job's key is refused.

use std::num::NonZeroUsize;

use js_state::Store;
use js_state::conformance::Backend;

use super::{Rig, full_spec, host, job_id, rig, share, whole_host_spec};
use crate::codec::{DecodeError, FORMAT, LATER_FORMAT, field_of, job_tag, set_field};
use crate::keys;
use crate::{Booked, EndReason, JobId, JobState, JobStoreError, NewJob};

/// A tag no build has used, standing for a field a later build adds.
const LATER_TAG: u16 = 901;

/// What the later field holds: bytes of any value, and none.
const LATER_VALUES: [&[u8]; 2] = [&[0, 255, 7], &[]];

/// Adds the later field holding `value` to the row `key`.
fn add_later<S: Store>(rig: &Rig<S>, key: &str, value: &[u8]) {
    let row = rig.raw(key).expect("written").value;
    rig.put_raw(
        key,
        set_field(&row, LATER_TAG, value).expect("the row parses"),
    );
}

/// The later field of the row `key`, if it holds one.
fn later<S: Store>(rig: &Rig<S>, key: &str) -> Option<Vec<u8>> {
    field_of(&rig.raw(key).expect("written").value, LATER_TAG)
}

/// Bytes this build cannot read as a row, and the error a read of the row `key` answers.
type Unreadable = (Vec<u8>, fn(String) -> JobStoreError);

/// A list value of `items`.
fn framed(items: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(&u32::try_from(item.len()).expect("small").to_be_bytes());
        out.extend_from_slice(item);
    }
    out
}

/// Every operation that rewrites a row keeps the later field of that row, with a value and
/// with none; a new placement writes a new allocation row, so the freed row it replaces is
/// not carried into it (the replacement is documented as an overwrite).
pub fn unknown_fields_survive_every_rewrite<B: Backend>(backend: &B) {
    let name = "unknown_fields_survive_every_rewrite";
    let rig = rig(backend);
    for (n, value) in LATER_VALUES.into_iter().enumerate() {
        let kept = Some(value.to_vec());
        let queued = rig.queue(&format!("u{n}-queued"));
        let job = keys::job(&queued);
        add_later(&rig, &job, value);
        rig.jobs.request_cancel(&queued).expect("cancels");
        assert_eq!(
            later(&rig, &job),
            kept,
            "{name}: a queued job's cancel lost it"
        );

        let placed = rig.placed(&format!("u{n}-placed"));
        let (job, allocation) = (keys::job(&placed), keys::allocation(&placed));
        add_later(&rig, &job, value);
        add_later(&rig, &allocation, value);
        rig.jobs.request_cancel(&placed).expect("an intent");
        assert_eq!(later(&rig, &job), kept, "{name}: the cancel intent lost it");
        rig.jobs.mark_releasing(&placed).expect("releases");
        assert_eq!(
            later(&rig, &allocation),
            kept,
            "{name}: the release lost it"
        );
        rig.jobs
            .transition(&placed, JobState::Placed, JobState::Starting)
            .expect("starting");
        assert_eq!(later(&rig, &job), kept, "{name}: a move lost it");
        rig.jobs.free(&placed, EndReason::Cancelled).expect("frees");
        assert_eq!(later(&rig, &allocation), kept, "{name}: the free lost it");

        let whole = rig.queue_spec(&format!("u{n}-whole"), whole_host_spec());
        let (job, allocation) = (keys::job(&whole), keys::allocation(&whole));
        rig.jobs
            .place(&whole, host("host-a"), Booked::WholeHost)
            .expect("placed");
        add_later(&rig, &job, value);
        add_later(&rig, &allocation, value);
        rig.jobs.requeue_failed_prepare(&whole).expect("requeued");
        assert_eq!(
            later(&rig, &job),
            kept,
            "{name}: the requeue lost the job's"
        );
        assert_eq!(
            later(&rig, &allocation),
            kept,
            "{name}: the requeue lost the allocation's"
        );
        rig.jobs
            .place(&whole, host("host-a"), Booked::WholeHost)
            .expect("placed again");
        assert_eq!(
            later(&rig, &job),
            kept,
            "{name}: the placement lost the job's"
        );
        assert_eq!(
            later(&rig, &allocation),
            None,
            "{name}: the replaced allocation's field was carried into the new one"
        );
    }
}

/// A job row, or an allocation row, that this build cannot read (a later build's, or one that
/// does not parse) refuses every write that reads it, with that row's error, and nothing is
/// written.
pub fn a_row_this_build_cannot_read_refuses_every_write<B: Backend>(backend: &B) {
    let name = "a_row_this_build_cannot_read_refuses_every_write";
    let rig = rig(backend);
    let unreadable: [Unreadable; 2] = [
        (vec![LATER_FORMAT], |key| JobStoreError::UnknownRow { key }),
        (vec![FORMAT, 0], |key| JobStoreError::Corrupt {
            key,
            why: DecodeError::Truncated,
        }),
    ];
    for (n, (bytes, error)) in unreadable.into_iter().enumerate() {
        let id = rig.queue_spec(&format!("x{n}-job"), whole_host_spec());
        rig.jobs
            .place(&id, host("host-a"), Booked::WholeHost)
            .expect("placed");
        let key = keys::job(&id);
        rig.put_raw(&key, bytes.clone());
        let before = rig.snapshot();
        let writes = [
            (
                "a placement",
                rig.jobs.place(&id, host("host-a"), share()).map(drop),
            ),
            (
                "a move",
                rig.jobs
                    .transition(&id, JobState::Placed, JobState::Starting)
                    .map(drop),
            ),
            ("a cancel", rig.jobs.request_cancel(&id).map(drop)),
            ("a requeue", rig.jobs.requeue_failed_prepare(&id).map(drop)),
            ("a release", rig.jobs.mark_releasing(&id).map(drop)),
            (
                "an insert",
                rig.jobs
                    .insert_queued(NewJob {
                        id: Some(id.clone()),
                        spec: full_spec(),
                        priority: 0,
                        client: None,
                    })
                    .map(drop),
            ),
        ];
        for (what, answer) in writes {
            assert_eq!(
                answer,
                Err(error(key.clone())),
                "{name}: {what} over an unreadable job row"
            );
        }
        assert_eq!(rig.snapshot(), before, "{name}: a refused write wrote");

        let placed = rig.queue_spec(&format!("x{n}-alloc"), whole_host_spec());
        rig.jobs
            .place(&placed, host("host-a"), Booked::WholeHost)
            .expect("placed");
        let key = keys::allocation(&placed);
        rig.put_raw(&key, bytes.clone());
        let queued = rig.queue(&format!("x{n}-queued"));
        let queued_key = keys::allocation(&queued);
        rig.put_raw(&queued_key, bytes);
        let before = rig.snapshot();
        let writes = [
            (
                "an end",
                rig.jobs.end_job(&placed, crate::JobEnd::Lost).map(drop),
                &key,
            ),
            (
                "a free",
                rig.jobs.free(&placed, EndReason::Exited).map(drop),
                &key,
            ),
            (
                "a release",
                rig.jobs.mark_releasing(&placed).map(drop),
                &key,
            ),
            (
                "a requeue",
                rig.jobs.requeue_failed_prepare(&placed).map(drop),
                &key,
            ),
            (
                "a placement",
                rig.jobs.place(&queued, host("host-b"), share()).map(drop),
                &queued_key,
            ),
        ];
        for (what, answer, key) in writes {
            assert_eq!(
                answer,
                Err(error(key.clone())),
                "{name}: {what} over an unreadable allocation row"
            );
        }
        assert_eq!(rig.snapshot(), before, "{name}: a refused write wrote");
    }
}

/// A reserved label key this build does not know is one a later build sets: the row is an
/// unknown row, which a list skips and names while it lists the jobs around it.
pub fn a_reserved_label_of_a_later_build_is_an_unknown_row<B: Backend>(backend: &B) {
    let name = "a_reserved_label_of_a_later_build_is_an_unknown_row";
    let rig = rig(backend);
    let first = rig.queue("l-1");
    let middle = rig.queue("l-2");
    let last = rig.queue("l-3");
    let row = rig.raw("job/l-2").expect("written").value;
    let labels = framed(&[b"jobscheduler.dev/later", b"x", b"team", b"infra"]);
    rig.put_raw(
        "job/l-2",
        set_field(&row, job_tag::LABELS, &labels).expect("the row parses"),
    );
    assert_eq!(
        rig.jobs.get_job(&middle).map(drop),
        Err(JobStoreError::UnknownRow {
            key: "job/l-2".to_owned()
        }),
        "{name}: the row was read as something else"
    );
    let page = rig
        .jobs
        .list_jobs(None, None, NonZeroUsize::MIN.saturating_add(9))
        .expect("a_reserved_label_of_a_later_build_is_an_unknown_row: the list answers");
    let listed: Vec<JobId> = page.jobs.into_iter().map(|job| job.id).collect();
    assert_eq!(
        (listed, page.skipped),
        (vec![first, last], vec!["job/l-2".to_owned()]),
        "{name}: the list did not go on past the row and name it"
    );
}

/// A row stored under another job's key (a copy, a bad restore) is refused by every read,
/// list and sum that meets it, and by a write that reads it, rather than taken for the job
/// its key names.
pub fn a_row_under_another_jobs_key_is_refused<B: Backend>(backend: &B) {
    let name = "a_row_under_another_jobs_key_is_refused";
    let rig = rig(backend);
    rig.placed("k-1");
    let second = rig.queue("k-2");
    let wrong = |key: &str| JobStoreError::Corrupt {
        key: key.to_owned(),
        why: DecodeError::WrongKey,
    };
    let job_row = rig.raw("job/k-1").expect("written").value;
    rig.put_raw("job/k-2", job_row);
    assert_eq!(
        rig.jobs.get_job(&second).map(drop),
        Err(wrong("job/k-2")),
        "{name}: a read took the row for its key's job"
    );
    assert_eq!(
        rig.jobs.request_cancel(&second).map(drop),
        Err(wrong("job/k-2")),
        "{name}: a cancel took the row for its key's job"
    );
    assert_eq!(
        rig.jobs
            .list_jobs(None, None, NonZeroUsize::MIN.saturating_add(9))
            .map(drop),
        Err(wrong("job/k-2")),
        "{name}: a list took the row for its key's job"
    );

    let allocation_row = rig.raw("alloc/k-1").expect("written").value;
    rig.put_raw("alloc/k-2", allocation_row);
    assert_eq!(
        rig.jobs.get_allocation(&job_id("k-2")).map(drop),
        Err(wrong("alloc/k-2")),
        "{name}: a read took the allocation row for its key's job"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")).map(drop),
        Err(wrong("alloc/k-2")),
        "{name}: a sum counted the allocation row for its key's job"
    );
}
