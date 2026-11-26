//! A job's completion callback in the store (`docs/design/job-retry-and-completion-callback.md`,
//! section 5): the key held in the `callback/` row alone, the row armed at the job's record and
//! made due in the commit of its end (whichever operation ends it), the callback as part of
//! the job's identity, and the rows an older build leaves intact.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreTime};

use super::marks::full_mark;
use super::whole::{engine_placement, requeue_with};
use super::{CALLBACK_URL, Rig, callback_spec, exited, job_id, rig, signing_key};
use crate::callback::{CallbackRecord, CallbackState};
use crate::codec::callback_fields::{decode_callback, encode_callback};
use crate::codec::{field_of, rearranged, set_field};
use crate::keys;
use crate::{
    Booked, CallbackKey, EndReason, FailedPrepare, JobCallback, JobEnd, JobId, JobSpec, JobState,
    JobStoreError, Needs, NewJob, ScriptedIds, StartFailure,
};

/// The tag of a job row's callback.
const CALLBACK_TAG: u16 = 68;

/// A tag no build has used: where a build from before the callback holds the callback's field,
/// as a field it does not know.
const UNKNOWN_TAG: u16 = 968;

/// The store's clock now.
pub(super) fn now<S: Store>(rig: &Rig<S>) -> StoreTime {
    rig.store.now().expect("the store's clock")
}

/// `secs` seconds after `from`.
pub(super) fn after(from: StoreTime, secs: u64) -> StoreTime {
    from.saturating_add(Duration::from_secs(secs))
}

/// When the job `id` ended.
pub(super) fn ended_at<S: Store>(rig: &Rig<S>, id: &JobId) -> StoreTime {
    let job = rig.jobs.get_job(id).expect("reads").expect("recorded");
    job.ended_at.expect("ended")
}

/// Records the queued job `text` with [`callback_spec`].
pub(super) fn queue_callback<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    rig.queue_spec(text, callback_spec())
}

/// Records the job `text` with [`callback_spec`] and ends it by a cancel while it is queued, so
/// its callback is due now.
pub(super) fn due_callback<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    let id = queue_callback(rig, text);
    rig.jobs.request_cancel(&id).expect("a queued job's cancel");
    id
}

/// The callback row of the job `id`.
pub(super) fn callback_row<S: Store>(rig: &Rig<S>, id: &JobId) -> CallbackRecord {
    let row = rig.raw(&keys::callback(id)).expect("a callback row");
    decode_callback(&row.value).expect("a callback row this build reads")
}

/// Writes `callback` as its job's callback row, past the job store.
pub(super) fn put_callback_row<S: Store>(rig: &Rig<S>, callback: &CallbackRecord) {
    let bytes = encode_callback(callback).expect("encodes");
    rig.put_raw(&keys::callback(&callback.job), bytes);
}

/// The keys of every due entry of a callback, in order.
pub(super) fn due_entries<S: Store>(rig: &Rig<S>) -> Vec<String> {
    let rows = rig
        .store
        .scan_prefix(keys::CALLBACK_DUE)
        .expect("reachable");
    rows.into_iter().map(|(key, _)| key).collect()
}

/// [`callback_spec`]'s callback as every job read back holds it: its key's digest.
fn stored_callback() -> JobCallback {
    JobCallback {
        url: CALLBACK_URL.to_owned(),
        key: CallbackKey::Digest(CallbackKey::Key(signing_key()).digest()),
        deadline_seconds: Some(60),
    }
}

/// Whether `bytes` holds `part` anywhere.
fn holds(bytes: &[u8], part: &[u8]) -> bool {
    bytes.windows(part.len()).any(|window| window == part)
}

/// A new job's callback goes into its armed `callback/` row with its key and a `webhook-id`
/// minted from the store's id source (16 bytes, base64url, nothing of the job), for a named
/// job and for one whose id is minted; the job row, the record the insert answers and every
/// read hold the key's digest. No row but the callback's holds the key's bytes, and the job
/// row's callback field is exactly its URL, its key's digest and its deadline. A new job that
/// names its key by its digest is refused, for both kinds of id, and nothing is written.
pub fn the_signing_key_is_held_in_the_callback_row_only<B: Backend>(backend: &B) {
    let name = "the_signing_key_is_held_in_the_callback_row_only";
    let rig = rig(backend);
    let minting = rig.minting(ScriptedIds::new([1; 16], [[2; 16], [3; 16]]));
    let named = minting
        .insert_queued(NewJob {
            id: Some(job_id("ky-named")),
            spec: callback_spec(),
            priority: 0,
            client: None,
        })
        .expect("a job with a callback is recorded");
    let unnamed = minting
        .insert_queued(NewJob {
            id: None,
            spec: callback_spec(),
            priority: 0,
            client: None,
        })
        .expect("a job with a callback and no id is recorded");
    // The named job's webhook-id takes the first draw, the minted job's id the next, and its
    // webhook-id the one after.
    let minted = JobId::minted([2; 16]);
    assert_eq!(unnamed.record.id, minted, "{name}: the minted id");
    for (inserted, webhook_id) in [
        (&named, "AQEBAQEBAQEBAQEBAQEBAQ"),
        (&unnamed, "AwMDAwMDAwMDAwMDAwMDAw"),
    ] {
        let id = &inserted.record.id;
        assert_eq!(
            inserted.record.spec.callback,
            Some(stored_callback()),
            "{name}: {id}: the record the insert answers"
        );
        let read = rig.jobs.get_job(id).expect("reads").expect("recorded");
        assert_eq!(
            read.spec.callback,
            Some(stored_callback()),
            "{name}: {id}: the job read back"
        );
        let armed = CallbackRecord::armed(
            id.clone(),
            &callback_spec().callback.expect("a callback"),
            signing_key(),
            webhook_id.to_owned(),
        );
        assert_eq!(callback_row(&rig, id), armed, "{name}: {id}: the armed row");
        let job = rig.raw(&keys::job(id)).expect("a job row").value;
        let digest = stored_callback().key.digest();
        let mut field = Vec::new();
        for item in [
            CALLBACK_URL.as_bytes(),
            digest.bytes(),
            &60u32.to_be_bytes(),
        ] {
            field.extend_from_slice(&u32::try_from(item.len()).expect("small").to_be_bytes());
            field.extend_from_slice(item);
        }
        assert_eq!(
            field_of(&job, CALLBACK_TAG),
            Some(field),
            "{name}: {id}: the job row's field"
        );
    }
    let key = signing_key().as_bytes().to_vec();
    let holders: Vec<String> = rig
        .snapshot()
        .0
        .into_iter()
        .filter(|(_, row)| holds(&row.value, &key))
        .map(|(key, _)| key)
        .collect();
    let expected = vec![keys::callback(&minted), keys::callback(&job_id("ky-named"))];
    assert_eq!(holders, expected, "{name}: the rows that hold the key");

    let before = rig.snapshot();
    let digested = JobSpec {
        callback: Some(stored_callback()),
        ..callback_spec()
    };
    let refused = rig.jobs.insert_queued(NewJob {
        id: Some(job_id("ky-digest")),
        spec: digested.clone(),
        priority: 0,
        client: None,
    });
    assert_eq!(
        refused,
        Err(JobStoreError::CallbackKeyMissing {
            id: job_id("ky-digest")
        }),
        "{name}: a named job with a digest"
    );
    let unnamed = rig.minting(ScriptedIds::new([3; 16], []));
    let refused = unnamed.insert_queued(NewJob {
        id: None,
        spec: digested,
        priority: 0,
        client: None,
    });
    assert_eq!(
        refused,
        Err(JobStoreError::CallbackKeyMissing {
            id: JobId::minted([3; 16])
        }),
        "{name}: a minted job with a digest"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a refusal wrote");
    assert_eq!(
        JobStoreError::CallbackKeyMissing {
            id: job_id("ky-digest")
        }
        .to_string(),
        "job ky-digest names its callback's key by its digest, so no delivery can be armed",
        "{name}: the refusal's text"
    );
}

/// A resubmit of a job with a callback is the same job when it asks for the same callback,
/// whether it holds the key or the digest every read returns, and writes nothing (the armed
/// row, its `webhook-id` included, stays as it is); one that asks for another URL, another key,
/// another deadline, no deadline, or no callback, is a conflict, as is a callback asked of a
/// job recorded without one, and none of them writes anything.
pub fn a_resubmit_with_another_policy_or_callback_conflicts<B: Backend>(backend: &B) {
    let name = "a_resubmit_with_another_policy_or_callback_conflicts";
    let rig = rig(backend);
    let id = queue_callback(&rig, "rs-callback");
    let plain = rig.queue("rs-plain");
    let before = rig.snapshot();
    let resubmit = |id: &JobId, spec: JobSpec| {
        rig.jobs.insert_queued(NewJob {
            id: Some(id.clone()),
            spec,
            priority: 0,
            client: None,
        })
    };
    let digested = JobSpec {
        callback: Some(stored_callback()),
        ..callback_spec()
    };
    for same in [callback_spec(), digested] {
        let inserted = resubmit(&id, same).expect("the same job");
        assert!(!inserted.created, "{name}: the same job was recorded again");
        assert_eq!(
            inserted.record.spec.callback,
            Some(stored_callback()),
            "{name}: the stored callback"
        );
    }
    let asked = callback_spec().callback.expect("a callback");
    let other_key = js_types::SigningKey::new(vec![b'o'; 32]).expect("a key");
    let others = [
        Some(JobCallback {
            url: "https://hooks.example/js-done/ref-2".to_owned(),
            ..asked.clone()
        }),
        Some(JobCallback {
            key: CallbackKey::Key(other_key),
            ..asked.clone()
        }),
        Some(JobCallback {
            deadline_seconds: Some(61),
            ..asked.clone()
        }),
        Some(JobCallback {
            deadline_seconds: None,
            ..asked.clone()
        }),
        None,
    ];
    for callback in others {
        let spec = JobSpec {
            callback,
            ..callback_spec()
        };
        assert_eq!(
            resubmit(&id, spec),
            Err(JobStoreError::JobIdConflict { id: id.clone() }),
            "{name}: another callback"
        );
    }
    assert_eq!(
        resubmit(&plain, callback_spec()),
        Err(JobStoreError::JobIdConflict { id: plain.clone() }),
        "{name}: a callback asked of a job without one"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a resubmit wrote");
}

/// Ends the job `id`, which `start` left as it needs, by one end operation of the store.
type End<S> = fn(&Rig<S>, &JobId) -> Result<(), JobStoreError>;

/// One way of ending a job with a callback: its name, how the job is made ready, and the end.
type EndCase<S> = (&'static str, fn(&Rig<S>, &str) -> JobId, End<S>);

/// A job with [`callback_spec`] placed on `host-a`.
fn placed_callback<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    let id = queue_callback(rig, text);
    let share = super::share();
    rig.jobs
        .place(&id, super::host("host-a"), share)
        .expect("placed");
    id
}

/// A job with [`callback_spec`] placed, started and running.
fn running_callback<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    let id = placed_callback(rig, text);
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("starting");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("running");
    id
}

/// A job with [`callback_spec`] that needs the whole host, placed on `host-a`.
fn placed_whole_callback<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    let spec = JobSpec {
        needs: Needs::WholeHost,
        ..callback_spec()
    };
    let id = rig.queue_spec(text, spec);
    rig.jobs
        .place_with(
            &id,
            super::host("host-a"),
            Booked::WholeHost,
            engine_placement(),
        )
        .expect("placed");
    id
}

/// A queued job with [`callback_spec`] that needs the whole host, with a mark.
fn marked_callback<S: Store>(rig: &Rig<S>, text: &str) -> JobId {
    let spec = JobSpec {
        needs: Needs::WholeHost,
        ..callback_spec()
    };
    let id = rig.queue_spec(text, spec);
    rig.jobs
        .put_mark(full_mark(rig, &id, "host-a"))
        .expect("marked");
    id
}

/// Every operation of the store that ends a job.
fn every_end<S: Store>() -> Vec<EndCase<S>> {
    vec![
        ("a queued job's cancel", queue_callback, |rig, id| {
            rig.jobs.request_cancel(id).map(drop)
        }),
        ("an exit", running_callback, |rig, id| {
            rig.jobs.end_job(id, exited(0)).map(drop)
        }),
        ("a loss", placed_callback, |rig, id| {
            rig.jobs.end_job(id, JobEnd::Lost).map(drop)
        }),
        ("no capacity", queue_callback, |rig, id| {
            let end = JobEnd::FailedToStart(StartFailure::NoCapacity);
            rig.jobs.end_job(id, end).map(drop)
        }),
        ("an end and a free", running_callback, |rig, id| {
            let end = exited(1);
            rig.jobs.end_and_free(id, end, EndReason::Exited).map(drop)
        }),
        (
            "the last failed prepare",
            placed_whole_callback,
            |rig, id| {
                let failed = FailedPrepare {
                    terminal: true,
                    ..requeue_with(rig, 3)
                };
                rig.jobs
                    .fail_prepare_with(id, failed, &|_| Vec::new())
                    .map(drop)
            },
        ),
        ("an end and an unmark", marked_callback, |rig, id| {
            rig.jobs.end_and_unmark(id, super::cancelled()).map(drop)
        }),
    ]
}

/// Each operation that ends a job (a queued job's cancel, an exit, a loss, a start failure, an
/// end with a free, the last failed prepare, an end with an unmark) makes the job's callback
/// due in its own one commit: the first call due at the end, the deadline the end plus the
/// minute the job asked for, its due entry at the end, and nothing else of the row changed. An
/// end whose commit fails leaves the job live and its callback armed; an end of an ended job
/// changes nothing; a write that does not end the job (a start, a failed prepare that requeues)
/// leaves it armed; an end of a job without a callback writes no callback row; an end of a job
/// whose callback row is gone, or not armed, leaves that as it is.
pub fn an_end_makes_its_callback_due_in_the_same_commit<B: Backend>(backend: &B) {
    let name = "an_end_makes_its_callback_due_in_the_same_commit";
    let rig = rig(backend);
    for (n, (case, start, end)) in every_end().into_iter().enumerate() {
        let id = start(&rig, &format!("ed-{n}"));
        let armed = callback_row(&rig, &id);
        assert_eq!(armed.state, CallbackState::Armed, "{name}: {case}");
        rig.tick();
        rig.store.fail_next();
        assert!(
            end(&rig, &id).is_err(),
            "{name}: {case}: a failed commit ended it"
        );
        assert_eq!(
            callback_row(&rig, &id),
            armed,
            "{name}: {case}: a failed end"
        );
        let job = rig.jobs.get_job(&id).expect("reads").expect("recorded");
        assert!(
            !job.state.is_ended(),
            "{name}: {case}: a failed end ended it"
        );

        let commits = rig.store.commit_stats().txns;
        end(&rig, &id).expect("ends");
        assert_eq!(
            rig.store.commit_stats().txns,
            commits + 1,
            "{name}: {case}: the end took other than one commit"
        );
        let at = ended_at(&rig, &id);
        let due = CallbackRecord {
            state: CallbackState::Pending,
            next_try_at: Some(at),
            deadline_at: Some(after(at, 60)),
            ..armed
        };
        assert_eq!(callback_row(&rig, &id), due, "{name}: {case}: the due row");
        assert!(
            due_entries(&rig).contains(&keys::callback_due(at, &id)),
            "{name}: {case}: no due entry"
        );
        let before = rig.snapshot();
        let again = rig.jobs.end_job(&id, JobEnd::Lost).expect("an ended job");
        assert!(again.state.is_ended(), "{name}: {case}");
        assert_eq!(rig.snapshot(), before, "{name}: {case}: a second end wrote");
    }

    let started = placed_callback(&rig, "ed-started");
    rig.jobs
        .transition(&started, JobState::Placed, JobState::Starting)
        .expect("starting");
    let requeued = placed_whole_callback(&rig, "ed-requeued");
    rig.jobs
        .fail_prepare_with(&requeued, requeue_with(&rig, 1), &|_| Vec::new())
        .expect("requeued");
    for id in [&started, &requeued] {
        assert_eq!(
            callback_row(&rig, id).state,
            CallbackState::Armed,
            "{name}: {id}: a write that does not end the job"
        );
    }

    let plain = rig.queue("ed-plain");
    rig.jobs.request_cancel(&plain).expect("cancels");
    assert_eq!(
        rig.raw(&keys::callback(&plain)),
        None,
        "{name}: no callback"
    );

    let gone = queue_callback(&rig, "ed-gone");
    rig.delete_raw(&keys::callback(&gone));
    rig.jobs.request_cancel(&gone).expect("cancels");
    assert_eq!(rig.raw(&keys::callback(&gone)), None, "{name}: a row gone");

    let delivered = queue_callback(&rig, "ed-delivered");
    let finished = CallbackRecord {
        state: CallbackState::Delivered,
        key: None,
        ..callback_row(&rig, &delivered)
    };
    put_callback_row(&rig, &finished);
    rig.jobs.request_cancel(&delivered).expect("cancels");
    assert_eq!(
        callback_row(&rig, &delivered),
        finished,
        "{name}: a row not armed"
    );
}

/// What a build from before the callback reads and writes (B8, section 7 of the design): a job
/// row whose callback field is one it does not know, which it carries into every rewrite of
/// the row, a cancel and an end included, without making the callback due; and the `callback/`
/// row and its due entry, which it neither reads nor deletes, through its writes of the job and
/// its open's index backfill. The field comes back to this build as the callback it was.
pub fn an_older_store_reader_leaves_callback_rows_intact<B: Backend>(backend: &B) {
    let name = "an_older_store_reader_leaves_callback_rows_intact";
    let rig = rig(backend);
    let due = due_callback(&rig, "ol-due");
    let row = rig.raw(&keys::callback(&due)).expect("a callback row");
    let entries = due_entries(&rig);
    let id = queue_callback(&rig, "ol-job");
    let armed = rig.raw(&keys::callback(&id)).expect("a callback row");
    let key = keys::job(&id);
    let field = field_of(&rig.raw(&key).expect("a job row").value, CALLBACK_TAG)
        .expect("the callback field");
    // The job row as the older build reads it: the field under a tag it does not know.
    let older = |row: &[u8]| {
        let without = rearranged(row, &[CALLBACK_TAG]).expect("parses");
        set_field(&without, UNKNOWN_TAG, &field).expect("parses")
    };
    let value = rig.raw(&key).expect("a job row").value;
    rig.put_raw(&key, older(&value));
    let job = rig.jobs.get_job(&id).expect("reads").expect("recorded");
    assert_eq!(job.spec.callback, None, "{name}: the older build's read");
    rig.jobs
        .place(&id, super::host("host-a"), super::share())
        .expect("placed");
    rig.jobs.end_job(&id, JobEnd::Lost).expect("lost");
    rig.reindex();
    let value = rig.raw(&key).expect("a job row").value;
    assert_eq!(
        field_of(&value, UNKNOWN_TAG),
        Some(field.clone()),
        "{name}: the field was not carried"
    );
    assert_eq!(
        rig.raw(&keys::callback(&id)),
        Some(armed),
        "{name}: the older build's end touched the row"
    );
    assert_eq!(
        rig.raw(&keys::callback(&due)),
        Some(row),
        "{name}: a due row"
    );
    assert_eq!(due_entries(&rig), entries, "{name}: the due entries");
    let back = rearranged(&value, &[UNKNOWN_TAG]).expect("parses");
    rig.put_raw(
        &key,
        set_field(&back, CALLBACK_TAG, &field).expect("parses"),
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("recorded");
    assert_eq!(
        job.spec.callback,
        Some(stored_callback()),
        "{name}: this build's read"
    );
}
