//! Recording jobs: the same id with the same job is answered from the stored one, a
//! different job is a conflict, and minted ids neither repeat nor give up early.

use std::collections::BTreeSet;
use std::sync::Arc;

use js_state::conformance::Backend;
use js_types::ImageDigestRef;

use super::{digest, full_spec, host, job_id, rig, share};
use crate::ids::ScriptedIds;
use crate::{JobId, JobSpec, JobState, JobStore, JobStoreError, MINT_TRIES, NewJob, RandomIds};

/// The digits of a minted id.
const HEX_DIGITS: &[u8] = b"0123456789abcdef";

fn new_job(id: &JobId, spec: JobSpec, priority: i32) -> NewJob {
    NewJob {
        id: Some(id.clone()),
        spec,
        priority,
    }
}

/// The same id with the same job, at a different priority, after the job moved on: the
/// stored record as it is now, not created, and still one row.
pub fn same_id_same_job_returns_the_existing_row<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = job_id("a");
    let first = rig
        .jobs
        .insert_queued(new_job(&id, full_spec(), 1))
        .expect("same_id_same_job_returns_the_existing_row: the first insert");
    assert!(
        first.created,
        "same_id_same_job_returns_the_existing_row: the first insert creates"
    );
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("placed");
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("starting");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("running");
    let before = rig.raw("job/a");
    rig.tick();

    let again = rig
        .jobs
        .insert_queued(new_job(&id, full_spec(), 7))
        .expect("same_id_same_job_returns_the_existing_row: the same job again answers");
    assert!(
        !again.created,
        "same_id_same_job_returns_the_existing_row: a second insert of the same job created it"
    );
    assert_eq!(
        again.record.state,
        JobState::Running,
        "same_id_same_job_returns_the_existing_row: the answer is not the job as it is now"
    );
    assert_eq!(
        again.record.priority, 1,
        "same_id_same_job_returns_the_existing_row: the second insert's priority was taken"
    );
    assert_eq!(
        Some(again.record),
        rig.jobs.get_job(&id).expect("reads"),
        "same_id_same_job_returns_the_existing_row: the answer is not the stored record"
    );
    assert_eq!(
        rig.raw("job/a"),
        before,
        "same_id_same_job_returns_the_existing_row: the second insert wrote the row"
    );
    assert_eq!(
        rig.job_rows(),
        1,
        "same_id_same_job_returns_the_existing_row: a second row was made"
    );
}

/// The same id with a job that differs in any one field of the spec is a conflict, and the
/// stored row is untouched. Each field is changed on its own, so a comparison that skips any
/// field fails here.
pub fn same_id_different_job_is_a_conflict<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.queue("b");
    let before = rig.raw("job/b");
    let changes: [(&str, fn(&mut JobSpec)); 12] = [
        ("image digest", |spec| {
            spec.image =
                ImageDigestRef::parse(&format!("team/app:1@{}", digest('b'))).expect("an image");
        }),
        ("command", |spec| spec.command.push("again".to_owned())),
        ("args", |spec| spec.args.reverse()),
        ("an environment value", |spec| {
            spec.env.insert("MODE".to_owned(), "prod".to_owned());
        }),
        ("needs", |spec| spec.needs = crate::Needs::WholeHost),
        ("a constraint", |spec| spec.constraints.arch = None),
        ("a capability", |spec| {
            spec.constraints.capabilities.clear();
        }),
        ("wall limit", |spec| spec.wall_limit_secs += 1),
        ("network", |spec| spec.network = crate::NetworkPolicy::Open),
        ("ports", |spec| spec.ports.clear()),
        ("files", |spec| {
            spec.files.insert("more".to_owned(), Vec::new());
        }),
        ("labels", |spec| spec.labels = js_types::Labels::default()),
    ];
    for (field, change) in changes {
        let mut spec = full_spec();
        change(&mut spec);
        assert_ne!(spec, full_spec(), "the change of {field} changes the spec");
        let answer = rig.jobs.insert_queued(new_job(&id, spec, 0));
        assert_eq!(
            answer,
            Err(JobStoreError::JobIdConflict { id: id.clone() }),
            "same_id_different_job_is_a_conflict: a job with another {field} was not a conflict"
        );
        assert_eq!(
            rig.raw("job/b"),
            before,
            "same_id_different_job_is_a_conflict: the conflict over {field} wrote the row"
        );
    }
}

/// A resubmit that differs only in priority is the same job.
pub fn a_priority_only_difference_is_the_same_job<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.queue("c");
    let before = rig.raw("job/c");
    let again = rig
        .jobs
        .insert_queued(new_job(&id, full_spec(), i32::MAX))
        .expect("a_priority_only_difference_is_the_same_job: a priority change is no conflict");
    assert!(
        !again.created,
        "a_priority_only_difference_is_the_same_job: the resubmit was created"
    );
    assert_eq!(
        again.record.priority, 0,
        "a_priority_only_difference_is_the_same_job: the stored priority changed"
    );
    assert_eq!(
        rig.raw("job/c"),
        before,
        "a_priority_only_difference_is_the_same_job: the resubmit wrote the row"
    );
}

/// A second writer records the id between this insert's read and its commit: the same job
/// is answered from the second writer's record, a different one is a conflict, and neither
/// writes over it.
pub fn a_concurrent_insert_answers_from_the_job_it_made<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let rig = rig(backend);
    let other = Arc::new(JobStore::new(rig.store.inner(), RandomIds).expect("a second writer"));
    let id = job_id("d");
    let racer = Arc::clone(&other);
    let raced = id.clone();
    rig.store.before_next(move || {
        racer
            .insert_queued(new_job(&raced, full_spec(), 5))
            .expect("the second writer records the job");
    });
    let answer = rig
        .jobs
        .insert_queued(new_job(&id, full_spec(), 0))
        .expect("a_concurrent_insert_answers_from_the_job_it_made: the same job answers");
    assert!(
        !answer.created,
        "a_concurrent_insert_answers_from_the_job_it_made: the insert that lost the race created"
    );
    assert_eq!(
        answer.record.priority, 5,
        "a_concurrent_insert_answers_from_the_job_it_made: the answer is not the stored job"
    );

    let id = job_id("e");
    let racer = Arc::clone(&other);
    let raced = id.clone();
    rig.store.before_next(move || {
        racer
            .insert_queued(new_job(&raced, super::whole_host_spec(), 0))
            .expect("the second writer records a different job");
    });
    let answer = rig.jobs.insert_queued(new_job(&id, full_spec(), 0));
    assert_eq!(
        answer,
        Err(JobStoreError::JobIdConflict { id: id.clone() }),
        "a_concurrent_insert_answers_from_the_job_it_made: a different job won the race"
    );
    let stored = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        stored.spec,
        super::whole_host_spec(),
        "a_concurrent_insert_answers_from_the_job_it_made: the losing insert wrote the row"
    );
}

/// Two job stores minting ids over one store, 2000 jobs each: 4000 distinct ids of 32
/// lowercase hex digits, and 4000 rows.
pub fn minted_ids_do_not_repeat_across_instances<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let second = JobStore::new(Arc::clone(&rig.store), RandomIds).expect("a second store");
    let mut ids = BTreeSet::new();
    for jobs in [&rig.jobs, &second] {
        for _ in 0..2000 {
            let inserted = jobs
                .insert_queued(NewJob {
                    id: None,
                    spec: full_spec(),
                    priority: 0,
                })
                .expect("minted_ids_do_not_repeat_across_instances: a job without an id");
            let text = inserted.record.id.as_str().to_owned();
            assert_eq!(
                text.len(),
                32,
                "minted_ids_do_not_repeat_across_instances: {text:?} is not 32 digits long"
            );
            assert!(
                text.bytes().all(|b| HEX_DIGITS.contains(&b)),
                "minted_ids_do_not_repeat_across_instances: {text:?} is not lowercase hex"
            );
            ids.insert(text);
        }
    }
    assert_eq!(
        ids.len(),
        4000,
        "minted_ids_do_not_repeat_across_instances: a minted id repeated"
    );
    assert_eq!(
        rig.job_rows(),
        4000,
        "minted_ids_do_not_repeat_across_instances: not one row per job"
    );
}

/// A minted id that is taken is minted again; a source that only gives taken ids is given
/// up on after [`MINT_TRIES`], having written nothing.
pub fn a_minted_id_collision_is_retried<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let taken = [0xaa; 16];
    let free = [0xbb; 16];
    rig.queue(JobId::minted(taken).as_str());

    let jobs = JobStore::new(Arc::clone(&rig.store), ScriptedIds::new(taken, [free]))
        .expect("a scripted store");
    let inserted = jobs
        .insert_queued(NewJob {
            id: None,
            spec: full_spec(),
            priority: 0,
        })
        .expect("a_minted_id_collision_is_retried: a taken id is minted again");
    assert_eq!(
        inserted.record.id,
        JobId::minted(free),
        "a_minted_id_collision_is_retried: the job did not get the second id"
    );

    let stuck = JobStore::new(Arc::clone(&rig.store), ScriptedIds::new(taken, []))
        .expect("a scripted store");
    let rows = rig.job_rows();
    let answer = stuck.insert_queued(NewJob {
        id: None,
        spec: full_spec(),
        priority: 0,
    });
    assert_eq!(
        answer,
        Err(JobStoreError::IdSpaceExhausted { tries: MINT_TRIES }),
        "a_minted_id_collision_is_retried: a source of taken ids was not given up on"
    );
    assert_eq!(
        rig.job_rows(),
        rows,
        "a_minted_id_collision_is_retried: giving up wrote a row"
    );
}
