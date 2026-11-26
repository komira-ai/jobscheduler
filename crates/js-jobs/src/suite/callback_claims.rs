//! The delivery of a job's callback through the store
//! (`docs/design/job-retry-and-completion-callback.md`, section 5.5): due calls claimed by one
//! instance at a time and handed out again when a claim lapses, each answer recorded by the
//! claim that holds the row, the doubling wait after a failed try, the deadline (cut to the
//! job's retention end) past which the callback expires, and the key erased at every final state.

use std::sync::Arc;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, StoreTime, Txn};
use js_types::{CallbackAnswer, FailureClass};

use super::callbacks::{
    after, callback_row, due_callback, due_entries, ended_at, now, queue_callback,
};
use super::{CALLBACK_URL, Rig, callback_spec, job_id, rig, signing_key};
use crate::callback::{CallbackState, CallbackStatus};
use crate::codec::LATER_FORMAT;
use crate::interfere::INJECTED;
use crate::keys;
use crate::{
    CallbackClaim, InstanceId, JobCallback, JobSpec, JobStore, JobStoreError, RandomIds, Retention,
};

/// The instance `name`.
fn instance(name: &str) -> InstanceId {
    InstanceId::new(name)
}

/// The error an operation answers when the store fails it.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// Claims every callback due now for `by`.
fn claim_all<S: Store>(rig: &Rig<S>, by: &str) -> Vec<CallbackClaim> {
    rig.jobs
        .claim_callbacks(&instance(by), now(rig), usize::MAX)
        .expect("claims")
}

/// Whether the stored row of the job `id` holds the signing key's bytes.
fn row_holds_key<S: Store>(rig: &Rig<S>, id: &crate::JobId) -> bool {
    let row = rig.raw(&keys::callback(id)).expect("a callback row").value;
    let key = signing_key();
    row.windows(key.as_bytes().len())
        .any(|window| window == key.as_bytes())
}

/// A claim hands out a due callback with its job, URL, key and `webhook-id` (the one minted at
/// submit), counts a try at the claim's time and holds the row for 30 s: a claim before then
/// gets nothing, one at its end gets the row again with the same `webhook-id` and the next try
/// counted, and an answer of the lapsed claim is then stale and writes nothing, while the claim
/// that holds the row is answered.
pub fn a_lapsed_callback_claim_is_taken_again<B: Backend>(backend: &B) {
    let name = "a_lapsed_callback_claim_is_taken_again";
    let rig = rig(backend);
    let id = due_callback(&rig, "lc-job");
    let webhook_id = callback_row(&rig, &id).webhook_id;
    let at = now(&rig);
    let first = rig
        .jobs
        .claim_callbacks(&instance("server-a"), at, usize::MAX)
        .expect("claims");
    assert_eq!(
        first
            .iter()
            .map(|claim| (
                &claim.job,
                claim.url.as_str(),
                &claim.key,
                &claim.webhook_id
            ))
            .collect::<Vec<_>>(),
        vec![(&id, CALLBACK_URL, &signing_key(), &webhook_id)],
        "{name}: the first claim"
    );
    let status = rig.jobs.callback_status(&id).expect("reads");
    assert_eq!(
        status.map(|status| (status.state, status.tries, status.last_try_at)),
        Some((CallbackState::Pending, 1, Some(at))),
        "{name}: the status after a claim"
    );
    assert_eq!(
        due_entries(&rig),
        vec![keys::callback_due(after(at, 30), &id)],
        "{name}: the entry of a held row"
    );
    rig.clock.advance(Duration::from_secs(29));
    assert_eq!(claim_all(&rig, "server-b"), Vec::new(), "{name}: held");
    rig.clock.advance(Duration::from_secs(1));
    let second = claim_all(&rig, "server-b");
    assert_eq!(
        second
            .iter()
            .map(|claim| (&claim.job, &claim.webhook_id))
            .collect::<Vec<_>>(),
        vec![(&id, &webhook_id)],
        "{name}: the lapsed claim was not taken again"
    );
    assert_eq!(callback_row(&rig, &id).tries, 2, "{name}: the second try");
    let before = rig.snapshot();
    let lapsed = first.into_iter().next().expect("a claim");
    assert_eq!(
        rig.jobs
            .answer_callback(lapsed, CallbackAnswer::Delivered, now(&rig)),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: the lapsed claim's answer"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a stale answer wrote");
    let holding = second.into_iter().next().expect("a claim");
    rig.jobs
        .answer_callback(holding, CallbackAnswer::Delivered, now(&rig))
        .expect("the holding claim's answer");
    assert_eq!(
        callback_row(&rig, &id).state,
        CallbackState::Delivered,
        "{name}: the holding claim's answer"
    );
}

/// No row is held by two claims: two instances claiming at once share the due rows between
/// them, each row to one, in the order of their entries, up to each one's limit (none for a
/// limit of 0, which writes nothing); a row another instance claims between a claim's read and
/// its decision is left to that instance; a claim whose decision fails to read, and one whose
/// read of the due entries fails, is the store's error; and a claim reads past rows it leaves
/// to fill its limit from the next page of entries.
pub fn one_claim_per_callback_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "one_claim_per_callback_row";
    let rig = rig(backend);
    let ids = ["oc-a", "oc-b", "oc-c"].map(|text| due_callback(&rig, text));
    let before = rig.snapshot();
    let none = rig
        .jobs
        .claim_callbacks(&instance("server-a"), now(&rig), 0);
    assert_eq!(none, Ok(Vec::new()), "{name}: a limit of 0");
    assert_eq!(rig.snapshot(), before, "{name}: a limit of 0 wrote");
    let jobs = |claims: Vec<CallbackClaim>| -> Vec<crate::JobId> {
        claims.into_iter().map(|claim| claim.job).collect()
    };
    let a = rig
        .jobs
        .claim_callbacks(&instance("server-a"), now(&rig), 1)
        .expect("claims");
    assert_eq!(jobs(a), vec![ids[0].clone()], "{name}: a limit of 1");
    // Two pages of two entries: the first holds an entry no row calls for, so the second is
    // read to fill the limit.
    rig.put_raw(&keys::callback_due(StoreTime::ZERO, &ids[1]), Vec::new());
    let b = rig
        .jobs
        .claim_callbacks(&instance("server-b"), now(&rig), 2)
        .expect("claims");
    assert_eq!(jobs(b), ids[1..].to_vec(), "{name}: the rest");
    assert_eq!(claim_all(&rig, "server-c"), Vec::new(), "{name}: all held");
    for (id, by) in ids.iter().zip(["server-a", "server-b", "server-b"]) {
        assert_eq!(
            callback_row(&rig, id).claimed_by,
            Some(instance(by)),
            "{name}: {id}"
        );
    }

    let raced = due_callback(&rig, "oc-raced");
    let other = rig.other();
    let at = now(&rig);
    rig.store.before_next(move || {
        let taken = other.claim_callbacks(&InstanceId::new("server-z"), at, 1);
        assert_eq!(taken.map(|claims| claims.len()), Ok(1));
    });
    assert_eq!(
        claim_all(&rig, "server-a"),
        Vec::new(),
        "{name}: a row claimed in between"
    );
    assert_eq!(
        callback_row(&rig, &raced).claimed_by,
        Some(instance("server-z")),
        "{name}: the row claimed in between"
    );

    let failing = due_callback(&rig, "oc-failing");
    let before = rig.snapshot();
    rig.store.fail_next_decision_get();
    assert_eq!(
        rig.jobs
            .claim_callbacks(&instance("server-a"), now(&rig), 1),
        Err(injected()),
        "{name}: a decision that fails to read"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs
            .claim_callbacks(&instance("server-a"), now(&rig), 1),
        Err(injected()),
        "{name}: a failed read of the entries"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed claim wrote");
    assert_eq!(
        callback_row(&rig, &failing).claimed_by,
        None,
        "{name}: a failed claim held the row"
    );
}

/// A claim deletes a due entry whose row is gone, and one its row no longer calls for (at the
/// row's version), and claims nothing for either; it leaves an entry that names no job, and a
/// row this build cannot read with its entry, as they are, and claims the readable rows beside
/// them.
pub fn a_claim_drops_the_entries_no_row_calls_for<B: Backend>(backend: &B) {
    let name = "a_claim_drops_the_entries_no_row_calls_for";
    let rig = rig(backend);
    let gone = due_callback(&rig, "de-gone");
    rig.delete_raw(&keys::callback(&gone));
    let moved = due_callback(&rig, "de-moved");
    let stray = keys::callback_due(StoreTime::ZERO, &moved);
    rig.put_raw(&stray, Vec::new());
    let nameless = format!(
        "{}{}",
        keys::CALLBACK_DUE,
        "00000000000000000000/not a job!"
    );
    rig.put_raw(&nameless, Vec::new());
    let unread = due_callback(&rig, "de-unread");
    rig.put_raw(&keys::callback(&unread), vec![LATER_FORMAT]);
    let claims = claim_all(&rig, "server-a");
    assert_eq!(
        claims
            .into_iter()
            .map(|claim| claim.job)
            .collect::<Vec<_>>(),
        vec![moved.clone()],
        "{name}: the claims"
    );
    let held = callback_row(&rig, &moved).claim_until.expect("held");
    let mut kept = vec![
        nameless,
        keys::callback_due(ended_at(&rig, &unread), &unread),
        keys::callback_due(held, &moved),
    ];
    kept.sort();
    assert_eq!(due_entries(&rig), kept, "{name}: the entries left");
    assert_eq!(
        rig.raw(&keys::callback(&unread)).map(|row| row.value),
        Some(vec![LATER_FORMAT]),
        "{name}: the unreadable row"
    );
}

/// Each answer of the claim that holds the row: a 2xx delivers (at the answer's time), a 410
/// abandons, and each ends the delivery with the key erased from the row and no due entry; a
/// failure keeps its class and makes the next try due after 5 s, doubling with each try to at
/// most 10 minutes; the due entry follows the next try. A status read follows the row: the
/// state, the tries, the last try, the last failure, the delivery time and the deadline. An
/// answer whose row changed before its commit is stale and writes nothing; one whose row cannot
/// be read is the store's error; a job without a callback, or whose callback row is gone, has no
/// status, and a job that does not exist is not found.
pub fn each_answer_is_recorded_by_the_claim_that_holds_the_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "each_answer_is_recorded_by_the_claim_that_holds_the_row";
    let rig = rig(backend);
    let delivered = due_callback(&rig, "ea-delivered");
    let abandoned = due_callback(&rig, "ea-abandoned");
    let at = now(&rig);
    let claims = rig
        .jobs
        .claim_callbacks(&instance("server-a"), at, usize::MAX)
        .expect("claims");
    for claim in claims {
        let answer = if claim.job == delivered {
            CallbackAnswer::Delivered
        } else {
            CallbackAnswer::Abandoned
        };
        rig.jobs
            .answer_callback(claim, answer, at)
            .expect("answers");
    }
    for (id, state, delivered_at) in [
        (&delivered, CallbackState::Delivered, Some(at)),
        (&abandoned, CallbackState::Abandoned, None),
    ] {
        let row = callback_row(&rig, id);
        assert_eq!(
            (row.state, row.key.is_some(), row.claimed_by.clone()),
            (state, false, None),
            "{name}: {id}"
        );
        assert!(!row_holds_key(&rig, id), "{name}: {id}: the key stayed");
        assert_eq!(
            rig.jobs.callback_status(id),
            Ok(Some(CallbackStatus {
                state,
                tries: 1,
                last_try_at: Some(at),
                last_failure: None,
                delivered_at,
                deadline_at: Some(after(ended_at(&rig, id), 60)),
            })),
            "{name}: {id}: the status"
        );
    }
    assert_eq!(due_entries(&rig), Vec::<String>::new(), "{name}: entries");

    // A deadline far enough off for the wait to reach its cap.
    let spec = JobSpec {
        callback: Some(JobCallback {
            deadline_seconds: None,
            ..callback_spec().callback.expect("a callback")
        }),
        ..callback_spec()
    };
    let failing = rig.queue_spec("ea-failing", spec);
    rig.jobs.request_cancel(&failing).expect("cancels");
    let classes = [FailureClass::RefusedAddress, FailureClass::Failed];
    for (n, wait) in [5, 10, 20, 40, 80, 160, 320, 600, 600]
        .into_iter()
        .enumerate()
    {
        let class = classes[n % 2];
        let claim = claim_all(&rig, "server-a").pop().expect("due");
        let at = now(&rig);
        rig.jobs
            .answer_callback(claim, CallbackAnswer::Failed(class), at)
            .expect("answers");
        let row = callback_row(&rig, &failing);
        let next = after(at, wait);
        assert_eq!(
            (row.state, row.next_try_at, row.last_failure, row.claimed_by),
            (CallbackState::Pending, Some(next), Some(class), None),
            "{name}: failure {n}"
        );
        assert_eq!(
            due_entries(&rig),
            vec![keys::callback_due(next, &failing)],
            "{name}: failure {n}: the entry"
        );
        rig.clock.advance(Duration::from_secs(wait - 1));
        assert_eq!(claim_all(&rig, "server-a"), Vec::new(), "{name}: early");
        rig.clock.advance(Duration::from_secs(1));
    }

    let raced = due_callback(&rig, "ea-raced");
    let claim = claim_all(&rig, "server-a")
        .into_iter()
        .find(|claim| claim.job == raced)
        .expect("due");
    let key = keys::callback(&raced);
    let value = rig.raw(&key).expect("a callback row").value;
    let store = rig.store.inner();
    rig.store.before_next(move || {
        let txn = Txn::new().put(key, value, Cond::Any);
        assert!(store.txn(txn).expect("reachable").is_ok());
    });
    let before = callback_row(&rig, &raced);
    assert_eq!(
        rig.jobs
            .answer_callback(claim.clone(), CallbackAnswer::Delivered, now(&rig)),
        Err(JobStoreError::Stale { id: raced.clone() }),
        "{name}: a row changed before the commit"
    );
    assert_eq!(callback_row(&rig, &raced), before, "{name}: raced");
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs
            .answer_callback(claim, CallbackAnswer::Delivered, now(&rig)),
        Err(injected()),
        "{name}: a failed read"
    );

    let plain = rig.queue("ea-plain");
    assert_eq!(rig.jobs.callback_status(&plain), Ok(None), "{name}: plain");
    let gone = queue_callback(&rig, "ea-gone");
    rig.delete_raw(&keys::callback(&gone));
    assert_eq!(rig.jobs.callback_status(&gone), Ok(None), "{name}: gone");
    let absent = job_id("ea-absent");
    assert_eq!(
        rig.jobs.callback_status(&absent),
        Err(JobStoreError::NotFound { id: absent }),
        "{name}: absent"
    );
    let armed = queue_callback(&rig, "ea-armed");
    assert_eq!(
        rig.jobs.callback_status(&armed),
        Ok(Some(CallbackStatus {
            state: CallbackState::Armed,
            tries: 0,
            last_try_at: None,
            last_failure: None,
            delivered_at: None,
            deadline_at: None,
        })),
        "{name}: armed"
    );
}

/// The signing key is erased from the `callback/` row at each final state of the delivery, in
/// the commit that reaches it: delivered, abandoned and expired (at a claim past the deadline)
/// alike; the row then holds none of the key's bytes, the job row still holds its digest, and a
/// claim hands nothing out for it. A failed try, which is not final, keeps the key.
pub fn the_signing_key_is_erased_when_delivery_ends<B: Backend>(backend: &B) {
    let name = "the_signing_key_is_erased_when_delivery_ends";
    let rig = rig(backend);
    let ids = ["ke-a", "ke-b", "ke-c", "ke-d"].map(|text| due_callback(&rig, text));
    let at = now(&rig);
    let claims = rig
        .jobs
        .claim_callbacks(&instance("server-a"), at, 3)
        .expect("claims");
    let answers = [
        CallbackAnswer::Delivered,
        CallbackAnswer::Abandoned,
        CallbackAnswer::Failed(FailureClass::Failed),
    ];
    for (claim, answer) in claims.into_iter().zip(answers) {
        rig.jobs
            .answer_callback(claim, answer, at)
            .expect("answers");
    }
    assert!(
        row_holds_key(&rig, &ids[2]),
        "{name}: a failed try erased it"
    );
    rig.clock.advance(Duration::from_secs(60));
    assert_eq!(claim_all(&rig, "server-a"), Vec::new(), "{name}: expired");
    let states = [
        CallbackState::Delivered,
        CallbackState::Abandoned,
        CallbackState::Expired,
        CallbackState::Expired,
    ];
    for (id, state) in ids.iter().zip(states) {
        let row = callback_row(&rig, id);
        assert_eq!((row.state, row.key), (state, None), "{name}: {id}");
        assert!(!row_holds_key(&rig, id), "{name}: {id}: the key stayed");
        let job = rig.jobs.get_job(id).expect("reads").expect("recorded");
        let digest = callback_spec()
            .callback
            .map(|callback| callback.key.digest());
        assert_eq!(
            job.spec.callback.map(|callback| callback.key.digest()),
            digest,
            "{name}: {id}: the digest"
        );
    }
}

/// A callback past its deadline expires and is sent no more: a claim at the deadline expires
/// the row (the key erased, no due entry, no try counted) and hands nothing out; a failed try
/// answered at the deadline expires it; and one answered before it is due again no later than
/// the deadline, when a claim expires it.
pub fn a_callback_past_its_deadline_expires<B: Backend>(backend: &B) {
    let name = "a_callback_past_its_deadline_expires";
    let rig = rig(backend);
    // The entries sort by the end of each job: capped, late, then unclaimed.
    let capped = due_callback(&rig, "pd-capped");
    let late = due_callback(&rig, "pd-late");
    let unclaimed = due_callback(&rig, "pd-unclaimed");
    let mut claims = rig
        .jobs
        .claim_callbacks(&instance("server-a"), now(&rig), 2)
        .expect("claims");
    let claim_late = claims.pop().expect("late's claim");
    let claim_capped = claims.pop().expect("capped's claim");
    assert_eq!(
        (&claim_capped.job, &claim_late.job),
        (&capped, &late),
        "{name}: entry order"
    );
    rig.clock.advance(Duration::from_secs(58));
    rig.jobs
        .answer_callback(
            claim_capped,
            CallbackAnswer::Failed(FailureClass::Failed),
            now(&rig),
        )
        .expect("answers");
    assert_eq!(
        callback_row(&rig, &capped).next_try_at,
        Some(after(ended_at(&rig, &capped), 60)),
        "{name}: the next try was not cut to the deadline"
    );
    rig.clock.advance(Duration::from_secs(2));
    rig.jobs
        .answer_callback(
            claim_late,
            CallbackAnswer::Failed(FailureClass::Failed),
            now(&rig),
        )
        .expect("answers");
    assert_eq!(
        callback_row(&rig, &late).state,
        CallbackState::Expired,
        "{name}: a failure at the deadline"
    );
    let unclaimed_row = callback_row(&rig, &unclaimed);
    assert_eq!(claim_all(&rig, "server-a"), Vec::new(), "{name}: expired");
    for id in [&unclaimed, &late, &capped] {
        let row = callback_row(&rig, id);
        assert_eq!(
            (row.state, row.key, row.next_try_at, row.claimed_by),
            (CallbackState::Expired, None, None, None),
            "{name}: {id}"
        );
        assert!(!row_holds_key(&rig, id), "{name}: {id}: the key stayed");
    }
    assert_eq!(
        callback_row(&rig, &unclaimed).tries,
        unclaimed_row.tries,
        "{name}: an expiry counted a try"
    );
    assert_eq!(due_entries(&rig), Vec::<String>::new(), "{name}: entries");
}

/// The deadline of a callback is its job's end plus the deadline it asked for (a day when it
/// asked for none), cut to the job's retention end: under a retention of a minute, a callback
/// that asked for none, or for more than a minute, stops at the retention end, and one that
/// asked for less stops at its own.
pub fn a_deadline_is_cut_to_the_retention_end<B: Backend>(backend: &B) {
    let name = "a_deadline_is_cut_to_the_retention_end";
    let rig = rig(backend);
    let window = Retention::new(Duration::from_secs(60)).expect("a window");
    let jobs = JobStore::new(Arc::clone(&rig.store), RandomIds)
        .expect("opens")
        .with_retention(window);
    let asked = callback_spec().callback.expect("a callback");
    for (text, deadline_seconds, after_end) in [
        ("cr-default", None, 60),
        ("cr-longer", Some(3600), 60),
        ("cr-shorter", Some(30), 30),
    ] {
        let spec = JobSpec {
            callback: Some(JobCallback {
                deadline_seconds,
                ..asked.clone()
            }),
            ..callback_spec()
        };
        let id = rig.queue_spec(text, spec);
        let ended = jobs.request_cancel(&id).expect("cancels");
        let end = ended.ended_at.expect("ended");
        assert_eq!(
            callback_row(&rig, &id).deadline_at,
            Some(after(end, after_end)),
            "{name}: {text}"
        );
    }
    let default = rig.queue_spec(
        "cr-day",
        JobSpec {
            callback: Some(JobCallback {
                deadline_seconds: None,
                ..asked
            }),
            ..callback_spec()
        },
    );
    rig.jobs.request_cancel(&default).expect("cancels");
    assert_eq!(
        callback_row(&rig, &default).deadline_at,
        Some(after(ended_at(&rig, &default), 86_400)),
        "{name}: a day by default"
    );
}
