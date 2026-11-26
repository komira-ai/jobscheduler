//! The life of a job's `callback/` row beside its job's
//! (`docs/design/job-retry-and-completion-callback.md`, sections 5.2 and 5.5): deleted with the
//! job's row at its retention end, its delivery deadline raised at a server's boot, and deleted
//! at boot when its job's row is gone.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};
use js_types::CallbackAnswer;

use super::callbacks::{
    after, callback_row, due_callback, due_entries, ended_at, now, put_callback_row, queue_callback,
};
use super::{callback_spec, rig};
use crate::callback::{CallbackRecord, CallbackState};
use crate::codec::{DecodeError, FORMAT, LATER_FORMAT};
use crate::keys;
use crate::{InstanceId, JobStoreError, NewJob, Retention};

/// The retention sweep deletes the `callback/` row of each job it deletes, in the commit that
/// deletes the job's row, with the due entry of a pending one: a pending row, a delivered one,
/// one this build cannot read, and none at all alike (the entry of the last two, which no row
/// names, is left to the claim that meets it). The row of a job still live, and of an ended job
/// not yet past its retention end, is kept.
pub fn the_sweep_deletes_a_callback_with_its_job<B: Backend>(backend: &B) {
    let name = "the_sweep_deletes_a_callback_with_its_job";
    let rig = rig(backend);
    let pending = due_callback(&rig, "sw-pending");
    let delivered = due_callback(&rig, "sw-delivered");
    let unread = due_callback(&rig, "sw-unread");
    let gone = due_callback(&rig, "sw-gone");
    let claims = rig
        .jobs
        .claim_callbacks(&InstanceId::new("server-a"), now(&rig), usize::MAX)
        .expect("claims");
    let claim = claims
        .into_iter()
        .find(|claim| claim.job == delivered)
        .expect("due");
    rig.jobs
        .answer_callback(claim, CallbackAnswer::Delivered, now(&rig))
        .expect("answers");
    // Both were claimed with the rest, so their entries are at the end of their claims.
    let held = |id| callback_row(&rig, id).claim_until.expect("held");
    let (gone_at, unread_at) = (held(&gone), held(&unread));
    rig.put_raw(&keys::callback(&unread), vec![LATER_FORMAT]);
    rig.delete_raw(&keys::callback(&gone));
    let live = queue_callback(&rig, "sw-live");
    rig.clock.advance(Retention::DEFAULT.window());
    let kept = due_callback(&rig, "sw-kept");
    let commits = rig.store.commit_stats().txns;
    let mut swept = rig.jobs.tick().expect("sweeps");
    swept.sort();
    let mut expected = vec![pending, delivered, unread.clone(), gone.clone()];
    expected.sort();
    assert_eq!(swept, expected, "{name}: the jobs swept");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the sweep took other than one commit"
    );
    for id in &expected {
        assert_eq!(rig.raw(&keys::job(id)), None, "{name}: {id}: the job");
        assert_eq!(
            rig.raw(&keys::callback(id)),
            None,
            "{name}: {id}: the callback"
        );
    }
    let left = vec![
        keys::callback_due(gone_at, &gone),
        keys::callback_due(unread_at, &unread),
        keys::callback_due(ended_at(&rig, &kept), &kept),
    ];
    assert_eq!(due_entries(&rig), left, "{name}: the entries left");
    for id in [&live, &kept] {
        assert!(
            rig.raw(&keys::callback(id)).is_some(),
            "{name}: {id}: a row kept"
        );
    }
}

/// A server's boot raises the deadline of each pending callback to at least the boot plus two
/// minutes, so an outage longer than a deadline expires no callback, and changes nothing else:
/// a later deadline, an armed row and a delivered one are kept as they are; a second boot at the
/// same instant writes nothing. A callback row another writer changes between the boot's read
/// and its commit is read again and raised; a row a later build wrote is left; a row that does
/// not parse fails the boot.
pub fn boot_raises_due_callback_deadlines<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "boot_raises_due_callback_deadlines";
    let rig = rig(backend);
    let raised = due_callback(&rig, "br-raised");
    let raced = due_callback(&rig, "br-raced");
    let spec = crate::JobSpec {
        callback: Some(crate::JobCallback {
            deadline_seconds: None,
            ..callback_spec().callback.expect("a callback")
        }),
        ..callback_spec()
    };
    let later = rig.queue_spec("br-later", spec);
    rig.jobs.request_cancel(&later).expect("cancels");
    let ended = ended_at(&rig, &later);
    let delivered = due_callback(&rig, "br-delivered");
    let finished = CallbackRecord {
        state: CallbackState::Delivered,
        key: None,
        next_try_at: None,
        ..callback_row(&rig, &delivered)
    };
    put_callback_row(&rig, &finished);
    rig.tick();
    let unread = due_callback(&rig, "br-unread");
    rig.put_raw(&keys::callback(&unread), vec![LATER_FORMAT]);
    rig.clock.advance(Duration::from_secs(600));
    let boot = now(&rig);
    let before = [&later, &delivered].map(|id| callback_row(&rig, id));
    let key = keys::callback(&raced);
    let value = rig.raw(&key).expect("a callback row").value;
    let store = rig.store.inner();
    rig.store.before_next(move || {
        let txn = Txn::new().put(key, value, Cond::Any);
        assert!(store.txn(txn).expect("reachable").is_ok());
    });
    // The two pending rows past their deadline; no job row is below the raise or waiting.
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(2),
        "{name}: the rows raised"
    );
    for id in [&raised, &raced] {
        assert_eq!(
            callback_row(&rig, id).deadline_at,
            Some(after(boot, 120)),
            "{name}: {id}"
        );
    }
    assert_eq!(
        callback_row(&rig, &later).deadline_at,
        Some(after(ended, 86_400)),
        "{name}: a later deadline"
    );
    assert_eq!(
        [&later, &delivered].map(|id| callback_row(&rig, id)),
        before,
        "{name}: rows that need no raise"
    );
    assert_eq!(rig.jobs.raise_at_boot(boot), Ok(0), "{name}: a second boot");
    let corrupt = queue_callback(&rig, "br-corrupt");
    let key = keys::callback(&corrupt);
    rig.put_raw(&key, vec![FORMAT, 0]);
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Err(JobStoreError::Corrupt {
            key,
            why: DecodeError::Truncated
        }),
        "{name}: a row that does not parse"
    );
}

/// A `callback/` row whose job row is gone, which only an older build's sweep leaves, is deleted
/// at a server's boot with its due entry, on the job's row being absent still; a job recorded
/// under its id between the boot's read and its commit keeps the row it wrote, and a row deleted
/// in between is not written again (the due entries of those two, which their rows no longer
/// call for, are left to the claim that meets them). The rows of jobs that exist are kept.
pub fn the_boot_deletes_an_orphan_callback<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "the_boot_deletes_an_orphan_callback";
    let rig = rig(backend);
    let orphan = due_callback(&rig, "or-orphan");
    let reused = due_callback(&rig, "or-reused");
    let vanished = due_callback(&rig, "or-vanished");
    // A deadline a day off, which the boot does not raise.
    let spec = crate::JobSpec {
        callback: Some(crate::JobCallback {
            deadline_seconds: None,
            ..callback_spec().callback.expect("a callback")
        }),
        ..callback_spec()
    };
    let kept = rig.queue_spec("or-kept", spec);
    rig.jobs.request_cancel(&kept).expect("cancels");
    let reused_at = ended_at(&rig, &reused);
    let vanished_at = ended_at(&rig, &vanished);
    for id in [&orphan, &reused, &vanished] {
        rig.delete_raw(&keys::job(id));
    }
    let other = rig.other();
    let store = rig.store.inner();
    let (again, gone) = (reused.clone(), keys::callback(&vanished));
    rig.store.before_next(move || {
        let job = NewJob {
            id: Some(again),
            spec: callback_spec(),
            priority: 0,
            client: None,
        };
        assert!(other.insert_queued(job).expect("records").created);
        let txn = Txn::new().delete(gone, Cond::Any);
        assert!(store.txn(txn).expect("reachable").is_ok());
    });
    let kept_row = callback_row(&rig, &kept);
    let boot = now(&rig);
    // The orphan's delete alone: the other two lost to the writes in between.
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(1),
        "{name}: the rows written"
    );
    for id in [&orphan, &vanished] {
        assert_eq!(rig.raw(&keys::callback(id)), None, "{name}: {id}");
    }
    let row = callback_row(&rig, &reused);
    assert_eq!(row.state, CallbackState::Armed, "{name}: the new job's row");
    assert_eq!(callback_row(&rig, &kept), kept_row, "{name}: a job's row");
    let at = kept_row.next_try_at.expect("due");
    let mut left = vec![
        keys::callback_due(reused_at, &reused),
        keys::callback_due(vanished_at, &vanished),
        keys::callback_due(at, &kept),
    ];
    left.sort();
    assert_eq!(due_entries(&rig), left, "{name}: the entries left");
}

/// The pending callbacks are counted from their due entries: none with no job, none for a job
/// whose callback is only armed, one for each job that ended with a callback, a claimed one
/// still counted until it is answered, and one no longer counted once it is delivered,
/// abandoned or expired. A failed read of the entries is the store's error, and no count.
/// Catches a count that takes in armed rows, or that drops a pending row while a claim holds it.
pub fn the_pending_callbacks_are_counted<B: Backend>(backend: &B) {
    let name = "the_pending_callbacks_are_counted";
    let rig = rig(backend);
    let pending = || rig.jobs.callbacks_pending().expect("counts");
    assert_eq!(pending(), 0, "{name}: no job");
    queue_callback(&rig, "pc-armed");
    assert_eq!(pending(), 0, "{name}: an armed callback");
    let delivered = due_callback(&rig, "pc-delivered");
    let abandoned = due_callback(&rig, "pc-abandoned");
    due_callback(&rig, "pc-expired");
    assert_eq!(pending(), 3, "{name}: three ended jobs");
    let by = InstanceId::new("server-a");
    let claims = rig
        .jobs
        .claim_callbacks(&by, now(&rig), usize::MAX)
        .expect("claims");
    assert_eq!(pending(), 3, "{name}: claimed, not answered");
    for claim in claims {
        let answer = match &claim.job {
            id if *id == delivered => CallbackAnswer::Delivered,
            id if *id == abandoned => CallbackAnswer::Abandoned,
            _ => continue,
        };
        rig.jobs
            .answer_callback(claim, answer, now(&rig))
            .expect("answers");
    }
    assert_eq!(pending(), 1, "{name}: one left unanswered");
    rig.clock.advance(Duration::from_secs(3600));
    let claimed = rig.jobs.claim_callbacks(&by, now(&rig), usize::MAX);
    assert_eq!(claimed, Ok(Vec::new()), "{name}: expired, not claimed");
    assert_eq!(pending(), 0, "{name}: the expired one");
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.callbacks_pending(),
        Err(JobStoreError::Store(js_state::StoreError::Failed(
            crate::interfere::INJECTED.to_owned()
        ))),
        "{name}: a failed read"
    );
}
