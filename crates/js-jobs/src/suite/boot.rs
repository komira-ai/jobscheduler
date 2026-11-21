//! The raise at boot while another server writes the tables: each row is raised in a
//! transaction of its own, a row another writer changed before its commit is read again and
//! decided again alone, and a raise wider than one store batch takes one batch per group.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreTime, Txn};

use super::{Rig, exited, rig};
use crate::{BOOT_GRACE, JobId, JobRecord, JobState, JobStore, RandomIds};

/// Seven days and an hour: past the retention end of a job ended at the start.
const PAST_A_WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60 + 3600);

/// The job `id` as stored, if it is.
fn stored<S: Store>(jobs: &JobStore<S>, id: &JobId) -> Option<JobRecord> {
    jobs.get_job(id).expect("reads")
}

/// The store's clock now.
fn now<S: Store>(rig: &Rig<S>) -> StoreTime {
    rig.store.now().expect("the store's clock")
}

/// A raise whose first commit four rows of another writer beat (a queued job rewritten as it
/// was, a placed job moved to starting, a queued job placed and run, an ended job deleted by
/// the other server's sweep) commits the untouched row in that batch, reads the four again
/// alone and decides each on what it holds now: the rewritten and the starting jobs get their
/// floors, the running job and the deleted one get nothing, and the other writer's changes
/// stand. Nothing is refused and a second raise at the same boot writes nothing. Catches a
/// raise that refuses or starts over when one row changes under it (on a busy store that
/// never ends), one that overwrites the other writer's change, and one that raises a row it
/// did not read again.
pub fn a_boot_raise_that_other_writers_beat_reads_each_row_again_alone<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_boot_raise_that_other_writers_beat_reads_each_row_again_alone";
    let rig = rig(backend);
    let gone = rig.running("ba-gone");
    rig.jobs.end_job(&gone, exited(0)).expect("ends");
    rig.clock.advance(PAST_A_WEEK);
    let calm = rig.queue("ba-calm");
    let raced = rig.queue("ba-raced");
    let moved = rig.placed("ba-moved");
    let run = rig.queue("ba-run");
    let boot = now(&rig);
    let floor = Some(boot.saturating_add(BOOT_GRACE));

    let other = JobStore::new(rig.store.inner(), RandomIds).expect("the other server opens");
    let inner = rig.store.inner();
    let (raced_key, moved_id, run_id, gone_id) = (
        crate::keys::job(&raced),
        moved.clone(),
        run.clone(),
        gone.clone(),
    );
    rig.store.before_next(move || {
        let row = inner.get(&raced_key).expect("reads").expect("present");
        inner
            .txn(Txn::new().put(raced_key, row.value, Cond::Any))
            .expect("reachable")
            .expect("the other server rewrites the queued job");
        other
            .transition(&moved_id, JobState::Placed, JobState::Starting)
            .expect("the other server starts the placed job");
        other
            .place(&run_id, super::host("host-b"), super::share())
            .expect("the other server places the queued job");
        other
            .transition(&run_id, JobState::Placed, JobState::Starting)
            .expect("placed to starting");
        other
            .transition(&run_id, JobState::Starting, JobState::Running)
            .expect("starting to running");
        assert_eq!(
            other.delete_swept(std::slice::from_ref(&gone_id)),
            Ok(vec![gone_id]),
            "the other server's sweep deletes the ended job past its end"
        );
    });

    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(3),
        "{name}: the untouched job and the two still waiting after the other server's writes \
         were not each raised once"
    );
    for id in [&calm, &raced, &moved] {
        assert_eq!(
            stored(&rig.jobs, id).and_then(|job| job.deadline_floor),
            floor,
            "{name}: {id} has no floor of the boot plus the grace"
        );
    }
    assert_eq!(
        stored(&rig.jobs, &moved).map(|job| job.state),
        Some(JobState::Starting),
        "{name}: the raise undid the other server's start"
    );
    let ran = stored(&rig.jobs, &run).expect("present");
    assert_eq!(
        (ran.state, ran.deadline_floor),
        (JobState::Running, None),
        "{name}: the job the other server ran was raised from what was read before"
    );
    assert_eq!(
        stored(&rig.jobs, &gone),
        None,
        "{name}: the ended job the other server deleted was written again"
    );
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(0),
        "{name}: the raise left work for a second one at the same boot"
    );
}

/// A raise of more rows than one store batch carries commits each row in a transaction of its
/// own, one store batch (one sync) per [`crate::store::GROUP`] rows, and raises every one.
/// Catches a raise that sends one batch of every waiting job (a transaction that locks them
/// all at once), and one that commits a row a round trip.
pub fn a_boot_raise_wider_than_a_batch_takes_a_batch_per_group<B: Backend>(backend: &B) {
    let name = "a_boot_raise_wider_than_a_batch_takes_a_batch_per_group";
    let rig = rig(backend);
    let rows = 2 * crate::store::GROUP + 1;
    let ids: Vec<JobId> = (0..rows)
        .map(|n| rig.queue(&format!("bw-{n:03}")))
        .collect();
    let boot = now(&rig);
    let before = rig.store.commit_stats();

    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(rows),
        "{name}: not every queued job was raised"
    );
    let after = rig.store.commit_stats();
    assert_eq!(
        (after.txns - before.txns, after.syncs - before.syncs),
        (rows as u64, 3),
        "{name}: the raise was not a transaction a row, three store batches for {rows} rows"
    );
    let floor = Some(boot.saturating_add(BOOT_GRACE));
    let unraised: Vec<&JobId> = ids
        .iter()
        .filter(|id| stored(&rig.jobs, id).and_then(|job| job.deadline_floor) != floor)
        .collect();
    assert!(
        unraised.is_empty(),
        "{name}: jobs left without a floor: {unraised:?}"
    );
}
