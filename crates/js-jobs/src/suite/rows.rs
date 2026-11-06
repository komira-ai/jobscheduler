//! The rows as bytes (B8): a field a later build added is kept through a rewrite, a field an
//! older build did not write reads as its default, a row this build cannot read is an error
//! rather than an absence, and the job list pages in key order.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use js_state::StoreTime;
use js_state::conformance::Backend;
use js_types::{ImageDigestRef, Labels, Resources};

use super::{Rig, digest, full_spec, host, job_id, rig, share};
use crate::codec::{DecodeError, FORMAT, alloc_tag, field_of, job_tag, set_field};
use crate::{JobId, JobState, JobStoreError, Needs, NetworkPolicy, NewJob};

/// A tag no build has used, standing for a field a later build adds.
const LATER_TAG: u16 = 900;

/// The bytes of that field.
const LATER_VALUE: &[u8] = b"a later build's field";

/// A job row as a build before the network policy, the ports and the files wrote it: no
/// tags 17, 18 or 19 (and no retention end). Its fields are listed in
/// `tests/golden/README.md`.
pub(super) const BEFORE_NETWORK: &[u8] =
    include_bytes!("../../tests/golden/job-before-network.bin");

/// A row with a field this build does not know reads, and a transition that rewrites the row
/// keeps the field's bytes; the same for an allocation row.
pub fn a_row_with_unknown_tags_is_read_and_rewritten_intact<B: Backend>(backend: &B) {
    let name = "a_row_with_unknown_tags_is_read_and_rewritten_intact";
    let rig = rig(backend);
    let id = rig.queue("n");
    let stored = rig.raw("job/n").expect("written").value;
    let later = set_field(&stored, LATER_TAG, LATER_VALUE).expect("the row parses");
    rig.put_raw("job/n", later);

    let read = rig
        .jobs
        .get_job(&id)
        .expect("a_row_with_unknown_tags_is_read_and_rewritten_intact: the row reads")
        .expect("present");
    assert_eq!(
        (read.spec, read.state),
        (full_spec(), JobState::Queued),
        "{name}: a field this build does not know changed the ones it knows"
    );
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("a_row_with_unknown_tags_is_read_and_rewritten_intact: the job is placed");
    let rewritten = rig.raw("job/n").expect("written").value;
    assert_eq!(
        field_of(&rewritten, LATER_TAG),
        Some(LATER_VALUE.to_vec()),
        "{name}: the placement lost the later build's field of the job"
    );
    assert_eq!(
        field_of(&rewritten, job_tag::STATE),
        Some(vec![1]),
        "{name}: the rewrite did not place the job"
    );

    let allocation = rig.raw("alloc/n").expect("written").value;
    let later = set_field(&allocation, LATER_TAG, LATER_VALUE).expect("the row parses");
    rig.put_raw("alloc/n", later);
    rig.jobs
        .end_job(&id, super::cancelled())
        .expect("a_row_with_unknown_tags_is_read_and_rewritten_intact: the job ends");
    let rewritten = rig.raw("alloc/n").expect("written").value;
    assert_eq!(
        field_of(&rewritten, LATER_TAG),
        Some(LATER_VALUE.to_vec()),
        "{name}: the release lost the later build's field of the allocation"
    );
    assert_eq!(
        field_of(&rewritten, alloc_tag::STEP),
        Some(vec![1]),
        "{name}: the rewrite did not release the allocation"
    );
}

/// A row an older build wrote, without the fields added since, reads with their defaults
/// and moves like any other.
pub fn a_row_without_the_newest_tag_reads_with_defaults<B: Backend>(backend: &B) {
    let name = "a_row_without_the_newest_tag_reads_with_defaults";
    let rig = rig(backend);
    rig.put_raw("job/golden-1", BEFORE_NETWORK.to_vec());
    let id = job_id("golden-1");
    let job = rig
        .jobs
        .get_job(&id)
        .expect("a_row_without_the_newest_tag_reads_with_defaults: an older row reads")
        .expect("present");
    assert_eq!(
        (
            job.spec.network,
            job.spec.ports.clone(),
            job.spec.files.clone(),
            job.retention_ends,
            job.client.clone(),
        ),
        (
            NetworkPolicy::None,
            Vec::new(),
            crate::JobFiles::Contents(BTreeMap::new()),
            None,
            None
        ),
        "{name}: a field the row lacks did not read as its default"
    );
    let image = ImageDigestRef::parse(&format!("team/app:1@{}", digest('c'))).expect("an image");
    let needs = Needs::Shape(Resources::new(1000, 1 << 30, 0, None).expect("resources"));
    assert_eq!(
        (
            job.id.clone(),
            job.spec.image.clone(),
            job.spec.command.clone(),
            job.spec.args.clone(),
            job.spec.env.clone(),
            job.spec.needs.clone(),
            job.spec.constraints.clone(),
            job.spec.wall_limit_secs,
            job.spec.labels.clone(),
        ),
        (
            id.clone(),
            image,
            vec!["run".to_owned()],
            Vec::new(),
            BTreeMap::new(),
            needs,
            crate::Constraints::default(),
            600,
            Labels::default(),
        ),
        "{name}: the fields the older row holds read otherwise"
    );
    assert_eq!(
        (
            job.priority,
            job.state,
            job.submitted_at,
            job.cancel_requested
        ),
        (
            3,
            JobState::Queued,
            StoreTime::from_duration(Duration::from_secs(5)),
            false
        ),
        "{name}: the older row's record reads otherwise"
    );
    let (placed, _) = rig
        .jobs
        .place(&id, host("host-a"), share())
        .expect("a_row_without_the_newest_tag_reads_with_defaults: an older row is placed");
    assert_eq!(
        rig.jobs.get_job(&id).expect("reads"),
        Some(placed),
        "{name}: the older row did not move"
    );
}

/// A row whose format byte, or one of whose enum values, is above this build's is an unknown
/// row: reads name it, a list skips it and says so, and an insert of its id is refused rather
/// than taken as absent.
pub fn a_row_with_a_higher_format_byte_is_unknown_not_missing<B: Backend>(backend: &B) {
    let name = "a_row_with_a_higher_format_byte_is_unknown_not_missing";
    let rig = rig(backend);
    let first = rig.queue("m-1");
    let last = rig.queue("m-3");
    let newer = vec![FORMAT + 1, 0, 1, 0, 0, 0, 0];
    rig.put_raw("job/m-2", newer.clone());
    let unknown = |key: &str| -> Result<(), JobStoreError> {
        Err(JobStoreError::UnknownRow {
            key: key.to_owned(),
        })
    };

    let newer_id = job_id("m-2");
    assert_eq!(
        rig.jobs.get_job(&newer_id).map(drop),
        unknown("job/m-2"),
        "{name}: a newer row read as something else"
    );
    let page = rig
        .jobs
        .list_jobs(None, None, NonZeroUsize::MIN.saturating_add(9))
        .expect("a_row_with_a_higher_format_byte_is_unknown_not_missing: the list answers");
    let listed: Vec<JobId> = page.jobs.into_iter().map(|job| job.id).collect();
    assert_eq!(
        (listed, page.skipped),
        (vec![first, last], vec!["job/m-2".to_owned()]),
        "{name}: the list did not skip the newer row and say so"
    );
    assert_eq!(
        rig.jobs
            .insert_queued(NewJob {
                id: Some(newer_id.clone()),
                spec: full_spec(),
                priority: 0,
                client: None,
            })
            .map(drop),
        unknown("job/m-2"),
        "{name}: an insert over a newer row took it as absent"
    );
    assert_eq!(
        rig.raw("job/m-2").map(|row| row.value),
        Some(newer),
        "{name}: the newer row was written over"
    );

    let known = rig.raw("job/m-1").expect("written").value;
    rig.put_raw(
        "job/m-state",
        set_field(&known, job_tag::STATE, &[200]).expect("the row parses"),
    );
    assert_eq!(
        rig.jobs.get_job(&job_id("m-state")).map(drop),
        unknown("job/m-state"),
        "{name}: a state this build does not know read as something else"
    );
    let placed = rig.placed("m-placed");
    let known = rig.raw("alloc/m-placed").expect("written").value;
    rig.put_raw(
        "alloc/m-placed",
        set_field(&known, alloc_tag::STEP, &[200]).expect("the row parses"),
    );
    assert_eq!(
        rig.jobs.get_allocation(&placed).map(drop),
        unknown("alloc/m-placed"),
        "{name}: a step this build does not know read as something else"
    );
}

/// A row that does not parse is refused by name, by a read, a list, an insert of its id and
/// a sum of bookings.
pub fn a_row_that_does_not_parse_is_corrupt<B: Backend>(backend: &B) {
    let name = "a_row_that_does_not_parse_is_corrupt";
    let rig = rig(backend);
    rig.put_raw("job/c-bad", vec![FORMAT, 0]);
    let id = job_id("c-bad");
    let corrupt = Err(JobStoreError::Corrupt {
        key: "job/c-bad".to_owned(),
        why: DecodeError::Truncated,
    });
    assert_eq!(
        rig.jobs.get_job(&id).map(drop),
        corrupt,
        "{name}: a read of a row that does not parse"
    );
    assert_eq!(
        rig.jobs.list_jobs(None, None, NonZeroUsize::MIN).map(drop),
        corrupt,
        "{name}: a list over a row that does not parse"
    );
    assert_eq!(
        rig.jobs
            .insert_queued(NewJob {
                id: Some(id),
                spec: full_spec(),
                priority: 0,
                client: None,
            })
            .map(drop),
        corrupt,
        "{name}: an insert over a row that does not parse"
    );

    rig.put_raw("alloc/c-bad", Vec::new());
    let corrupt = Err(JobStoreError::Corrupt {
        key: "alloc/c-bad".to_owned(),
        why: DecodeError::Empty,
    });
    assert_eq!(
        rig.jobs.get_allocation(&job_id("c-bad")).map(drop),
        corrupt,
        "{name}: a read of an allocation row that does not parse"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")).map(drop),
        corrupt,
        "{name}: a sum over an allocation row that does not parse"
    );
}

/// Every page of a list, from the first, with `state` as the filter: the ids of each page.
fn pages<S: js_state::Store>(rig: &Rig<S>, state: Option<JobState>) -> Vec<Vec<JobId>> {
    let limit = NonZeroUsize::MIN.saturating_add(9);
    let ids = |page: &crate::JobPage| -> Vec<JobId> {
        page.jobs.iter().map(|job| job.id.clone()).collect()
    };
    let mut page = rig
        .jobs
        .list_jobs(state, None, limit)
        .expect("the first page");
    let mut pages = vec![ids(&page)];
    while let Some(token) = page.resume.take() {
        page = rig
            .jobs
            .list_jobs(state, Some(&token), limit)
            .expect("the next page");
        pages.push(ids(&page));
    }
    pages
}

/// 25 jobs in three states, ten to a page: the pages are ten, ten and five jobs, and they
/// concatenate to every job once, in key order; filtered by a state, to every job in it.
pub fn list_jobs_pages_in_key_order_and_filters_by_state<B: Backend>(backend: &B) {
    let name = "list_jobs_pages_in_key_order_and_filters_by_state";
    let rig = rig(backend);
    // Recorded out of key order, so a list in commit order would differ.
    let mut ids: Vec<JobId> = (0..25)
        .map(|n| rig.queue(&format!("j{:02}", (n * 7) % 25)))
        .collect();
    ids.sort();
    let mut by_state = BTreeMap::<&str, Vec<JobId>>::new();
    for (n, id) in ids.iter().enumerate() {
        let state = match n % 3 {
            0 => {
                rig.jobs.request_cancel(id).expect("cancels");
                "cancelled"
            }
            1 => {
                rig.jobs.place(id, host("host-a"), share()).expect("places");
                "placed"
            }
            _ => "queued",
        };
        by_state.entry(state).or_default().push(id.clone());
    }

    let all = pages(&rig, None);
    assert_eq!(
        all.concat(),
        ids,
        "{name}: the pages do not concatenate to every job once, in key order"
    );
    assert_eq!(
        all.iter().map(Vec::len).collect::<Vec<_>>(),
        [10, 10, 5],
        "{name}: the pages are not of the limit"
    );
    for (state, label) in [
        (JobState::Cancelled, "cancelled"),
        (JobState::Placed, "placed"),
        (JobState::Queued, "queued"),
    ] {
        assert_eq!(
            pages(&rig, Some(state)).concat(),
            by_state[label],
            "{name}: the pages filtered by {label} are not the {label} jobs"
        );
    }
    let everything = rig
        .jobs
        .list_jobs(None, None, NonZeroUsize::MAX)
        .expect("a page of the largest limit");
    assert_eq!(
        (everything.jobs.len(), everything.resume),
        (25, None),
        "{name}: a page of the largest limit is not every job"
    );
}
