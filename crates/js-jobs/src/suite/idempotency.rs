//! Recording jobs: the same id with the same job is answered from the stored one, a
//! different job is a conflict, and minted ids neither repeat nor give up early.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_state::StoreError;
use js_state::conformance::Backend;
use js_types::ImageDigestRef;

use super::{digest, full_spec, host, job_id, rig, share};
use crate::codec::{FORMAT, job_tag, rearranged};
use crate::ids::ScriptedIds;
use crate::interfere::INJECTED;
use crate::{
    ClientName, IdSource, JobId, JobSpec, JobState, JobStore, JobStoreError, MINT_TRIES, MintError,
    NewJob, RandomIds,
};

/// The digits of a minted id.
const HEX_DIGITS: &[u8] = b"0123456789abcdef";

/// A field of a spec, and how to change it.
type Change = (&'static str, fn(&mut JobSpec));

/// What the full spec needs, with the given counts.
fn needs(cpu: u64, memory: u64, accelerators: u64, class: &str, disk: u64) -> crate::Needs {
    crate::Needs::Shape(
        js_types::Resources::new(cpu, memory, accelerators, Some(class.to_owned()))
            .expect("resources")
            .with_disk_bytes(disk),
    )
}

fn new_job(id: &JobId, spec: JobSpec, priority: i32) -> NewJob {
    NewJob {
        id: Some(id.clone()),
        spec,
        priority,
        client: None,
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

    // An ended job is answered too, ended: the id is not free again until its retention ends.
    rig.jobs.end_job(&id, super::exited(0)).expect("ends");
    let ended = rig.raw("job/a");
    let after_end = rig
        .jobs
        .insert_queued(new_job(&id, full_spec(), 0))
        .expect("same_id_same_job_returns_the_existing_row: an ended job answers");
    assert_eq!(
        (after_end.created, after_end.record.state),
        (false, JobState::Succeeded),
        "same_id_same_job_returns_the_existing_row: a resubmit of an ended job queued it again"
    );
    assert_eq!(
        rig.raw("job/a"),
        ended,
        "same_id_same_job_returns_the_existing_row: a resubmit of an ended job wrote the row"
    );
}

/// The comparison is of the decoded job, not of bytes: a row holding the same job in other
/// bytes (fields in another order, an empty list left out) is the same job, and is left as
/// it is.
pub fn the_same_job_is_found_by_value_not_by_bytes<B: Backend>(backend: &B) {
    let name = "the_same_job_is_found_by_value_not_by_bytes";
    let rig = rig(backend);
    let spec = JobSpec {
        args: Vec::new(),
        ..full_spec()
    };
    let id = rig.queue_spec("bytes", spec.clone());
    let written = rig.raw("job/bytes").expect("written").value;
    let other_bytes = rearranged(&written, &[job_tag::ARGS]).expect("the row parses");
    assert_ne!(
        other_bytes, written,
        "{name}: the rearranged bytes are the same"
    );
    rig.put_raw("job/bytes", other_bytes.clone());
    let again = rig
        .jobs
        .insert_queued(new_job(&id, spec, 0))
        .expect("the_same_job_is_found_by_value_not_by_bytes: the same job answers");
    assert!(
        !again.created,
        "{name}: the same job in other bytes was a new job"
    );
    assert_eq!(
        rig.raw("job/bytes").map(|row| row.value),
        Some(other_bytes),
        "{name}: the row was written"
    );
}

/// The same id with a job that differs in any one field of the spec is a conflict, and the
/// stored row is untouched. Each field is changed on its own, so a comparison that skips any
/// field fails here.
pub fn same_id_different_job_is_a_conflict<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.queue("b");
    let before = rig.raw("job/b");
    let changes: &[Change] = &[
        ("image name", |spec| {
            spec.image =
                ImageDigestRef::parse(&format!("team/app:2@{}", digest('a'))).expect("an image");
        }),
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
        ("cpu", |spec| {
            spec.needs = needs(2001, 1 << 30, 1, "accel-a", 1 << 32)
        }),
        ("memory", |spec| {
            spec.needs = needs(2000, 1 << 31, 1, "accel-a", 1 << 32)
        }),
        ("accelerators", |spec| {
            spec.needs = needs(2000, 1 << 30, 2, "accel-a", 1 << 32)
        }),
        ("accelerator class", |spec| {
            spec.needs = needs(2000, 1 << 30, 1, "accel-b", 1 << 32)
        }),
        ("disk", |spec| {
            spec.needs = needs(2000, 1 << 30, 1, "accel-a", 1 << 33)
        }),
        ("os", |spec| spec.constraints.os = Some("macos".to_owned())),
        ("arch", |spec| spec.constraints.arch = None),
        ("exec", |spec| {
            spec.constraints.exec = Some(js_types::ExecMode::SandboxExec)
        }),
        ("isolation", |spec| {
            spec.constraints.isolation = crate::Isolation::Host;
        }),
        ("a capability", |spec| {
            spec.constraints.capabilities.clear();
        }),
        ("wall limit", |spec| spec.wall_limit_secs += 1),
        ("network", |spec| spec.network = crate::NetworkPolicy::Open),
        ("ports", |spec| spec.ports.clear()),
        ("files", |spec| {
            let mut files = spec.files.contents().cloned().unwrap_or_default();
            files.insert("more".to_owned(), Vec::new());
            spec.files = crate::JobFiles::Contents(files);
        }),
        ("labels", |spec| spec.labels = js_types::Labels::default()),
    ];
    for &(field, change) in changes {
        let mut spec = full_spec();
        change(&mut spec);
        assert_ne!(
            spec,
            full_spec(),
            "same_id_different_job_is_a_conflict: the change of {field} does not change the spec"
        );
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

/// A job recorded with a client, and the same id with the same job again from another client
/// and from none: each retry is answered from the stored record with the first client, writes
/// nothing, and the row reads back with that client; a job without an id is recorded with
/// its client. The client is not part of the idempotency comparison, and a retry never
/// overwrites it.
pub fn a_retry_by_another_client_keeps_the_first<B: Backend>(backend: &B) {
    let name = "a_retry_by_another_client_keeps_the_first";
    let rig = rig(backend);
    let id = job_id("by-a");
    let client = |text: &str| Some(ClientName::parse(text).expect("a client name"));
    let from = |client: Option<ClientName>| NewJob {
        client,
        ..new_job(&id, full_spec(), 0)
    };
    let first = rig
        .jobs
        .insert_queued(from(client("client-a")))
        .expect("the first insert");
    assert_eq!(
        (first.created, first.record.client.clone()),
        (true, client("client-a")),
        "{name}: the first insert did not record its client"
    );
    let before = rig.raw("job/by-a");
    for retry in [client("client-b"), None] {
        let again = rig
            .jobs
            .insert_queued(from(retry.clone()))
            .expect("a retry answers");
        assert_eq!(
            (again.created, again.record),
            (false, first.record.clone()),
            "{name}: a retry from {retry:?} did not answer the stored record"
        );
    }
    assert_eq!(rig.raw("job/by-a"), before, "{name}: a retry wrote the row");
    let stored = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        stored.client,
        client("client-a"),
        "{name}: the row lost its client"
    );
    let minted = rig
        .jobs
        .insert_queued(NewJob {
            id: None,
            client: client("client-c"),
            ..new_job(&id, full_spec(), 0)
        })
        .expect("a minted insert");
    assert_eq!(
        minted.record.client,
        client("client-c"),
        "{name}: a job without an id lost its client"
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
                    client: None,
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
            client: None,
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
        client: None,
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

/// One answer of an id source.
type Draw = Result<[u8; 16], MintError>;

/// What a [`Counted`] source answers, and how often it was asked.
struct Script {
    answers: Vec<Draw>,
    drawn: AtomicUsize,
}

/// A source that answers its script in order (its last answer again once the script ends)
/// and counts its draws; its clones share the count.
#[derive(Clone)]
struct Counted(Arc<Script>);

impl Counted {
    fn new(answers: Vec<Draw>) -> Counted {
        Counted(Arc::new(Script {
            answers,
            drawn: AtomicUsize::new(0),
        }))
    }

    fn draws(&self) -> usize {
        self.0.drawn.load(Ordering::SeqCst)
    }
}

impl IdSource for Counted {
    fn mint(&self) -> Result<[u8; 16], MintError> {
        let n = self.0.drawn.fetch_add(1, Ordering::SeqCst);
        self.0.answers[n.min(self.0.answers.len() - 1)]
    }
}

/// A job with no id.
fn unnamed() -> NewJob {
    NewJob {
        id: None,
        spec: full_spec(),
        priority: 0,
        client: None,
    }
}

/// The minting budget, draw by draw: a free id on the last allowed draw is taken, eight taken
/// draws give up before a ninth, a source or store failure after a collision is that failure,
/// an id another writer takes between the draw and the commit is drawn past, and a taken id
/// whose row this build cannot read stays taken. Nothing is written on a failure.
pub fn minting_stops_at_its_budget<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "minting_stops_at_its_budget";
    let rig = rig(backend);
    let taken: Vec<[u8; 16]> = (1..=8u8).map(|n| [n; 16]).collect();
    for bits in &taken {
        rig.queue(JobId::minted(*bits).as_str());
    }

    let last = [0xf0; 16];
    let source = Counted::new(
        taken[..7]
            .iter()
            .copied()
            .map(Ok)
            .chain([Ok(last)])
            .collect(),
    );
    let inserted = rig
        .minting(source.clone())
        .insert_queued(unnamed())
        .expect("minting_stops_at_its_budget: a free id on the last draw");
    assert_eq!(
        (inserted.record.id, source.draws()),
        (JobId::minted(last), MINT_TRIES),
        "{name}: the last allowed draw was not taken"
    );

    let source = Counted::new(
        taken
            .iter()
            .copied()
            .map(Ok)
            .chain([Ok([0xf1; 16])])
            .collect(),
    );
    let jobs = rig.minting(source.clone());
    let before = rig.snapshot();
    assert_eq!(
        jobs.insert_queued(unnamed()),
        Err(JobStoreError::IdSpaceExhausted { tries: MINT_TRIES }),
        "{name}: eight taken draws were not given up on"
    );
    assert_eq!(source.draws(), MINT_TRIES, "{name}: a draw past the budget");
    assert_eq!(rig.snapshot(), before, "{name}: giving up wrote");

    let failure = MintError {
        code: NonZeroU32::MIN,
    };
    let source = Counted::new(vec![Ok(taken[0]), Err(failure)]);
    let jobs = rig.minting(source.clone());
    let before = rig.snapshot();
    assert_eq!(
        jobs.insert_queued(unnamed()),
        Err(JobStoreError::Mint(failure)),
        "{name}: a source failure after a collision"
    );
    assert_eq!(
        (source.draws(), rig.snapshot()),
        (2, before),
        "{name}: a source failure after a collision wrote"
    );

    let source = Counted::new(vec![Ok(taken[0]), Ok([0xf2; 16])]);
    let jobs = rig.minting(source.clone());
    let before = rig.snapshot();
    rig.store.fail_after(1);
    assert_eq!(
        jobs.insert_queued(unnamed()),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a store failure after a collision"
    );
    assert_eq!(
        (source.draws(), rig.snapshot()),
        (2, before),
        "{name}: a store failure after a collision wrote"
    );

    let raced = [0xf3; 16];
    let after = [0xf4; 16];
    let source = Counted::new(vec![Ok(raced), Ok(after)]);
    let jobs = rig.minting(source.clone());
    let other = rig.other();
    rig.store.before_next(move || {
        other
            .insert_queued(NewJob {
                id: Some(JobId::minted(raced)),
                spec: super::whole_host_spec(),
                priority: 0,
                client: None,
            })
            .expect("the other writer takes the id");
    });
    let inserted = jobs
        .insert_queued(unnamed())
        .expect("minting_stops_at_its_budget: an id taken in the race is drawn past");
    assert_eq!(
        (inserted.record.id, source.draws()),
        (JobId::minted(after), 2),
        "{name}: the id the other writer took was not drawn past"
    );
    assert_eq!(
        rig.jobs
            .get_job(&JobId::minted(raced))
            .expect("reads")
            .map(|job| job.spec),
        Some(super::whole_host_spec()),
        "{name}: the other writer's job was written over"
    );

    let unreadable = [0xf5; 16];
    let key = crate::keys::job(&JobId::minted(unreadable));
    rig.put_raw(&key, vec![FORMAT + 1]);
    let source = Counted::new(vec![Ok(unreadable), Ok([0xf6; 16])]);
    let inserted = rig
        .minting(source)
        .insert_queued(unnamed())
        .expect("minting_stops_at_its_budget: an unreadable taken id is drawn past");
    assert_eq!(
        inserted.record.id,
        JobId::minted([0xf6; 16]),
        "{name}: the unreadable row's id was used"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        Some(vec![FORMAT + 1]),
        "{name}: the unreadable row was written over"
    );
}
