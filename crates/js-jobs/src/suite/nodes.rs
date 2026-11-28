//! A host's presence row (`node/<host>`: [`crate::JobStore::hello_node`],
//! [`crate::JobStore::beat_node`], [`crate::JobStore::read_node`],
//! [`crate::JobStore::live_nodes`], [`crate::JobStore::forget_node`]), which two servers over
//! one store judge alike.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

use js_agent_wire::Phase;
use js_state::conformance::Backend;
use js_state::{Store, StoreError, StoreTime};
use js_types::{HostId, Resources, Shape};

use super::{Rig, host, job_id, rig};
use crate::codec::{DecodeError, LATER_FORMAT, field_of, set_field};
use crate::interfere::INJECTED;
use crate::{
    AppliedBeat, Beat, IdSource, JobStore, JobStoreError, LoadReading, NodeHello, NodeRecord,
    NodeRow, NodeState, Orphan, SessionDigest, keys,
};

/// What a host's hello announces in these checks: a session digest, a pool, a claimed shape and
/// a smaller booked one (so the two cannot be swapped unseen), labels and the phase `serving`.
fn hello() -> NodeHello {
    let claimed = Resources::new(8000, 32 << 30, 1, Some("accel-a".to_owned()))
        .expect("resources")
        .with_disk_bytes(1 << 40);
    let booked = Resources::new(4000, 16 << 30, 0, None).expect("resources");
    let capabilities = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);
    NodeHello {
        session: Some(SessionDigest::new([7; 32])),
        pool: "pool-a".to_owned(),
        claim: Some(Shape::new(claimed, capabilities.clone())),
        shape: Some(Shape::new(booked, BTreeMap::new())),
        labels: BTreeMap::from([("rack".to_owned(), "r1".to_owned())]),
        phase: Phase::Serving,
        agent_process: Some("proc-a".to_owned()),
        agent_version: Some("1.2.3".to_owned()),
    }
}

/// What a row holds of its hello: the host, the epoch, the session digest, the pool, the
/// claimed and booked shapes, the labels and the agent version.
type Announced = (
    HostId,
    u64,
    Option<SessionDigest>,
    String,
    Option<Shape>,
    Option<Shape>,
    BTreeMap<String, String>,
    Option<String>,
);

/// What `row` holds of its hello.
fn announced(row: &NodeRow) -> Announced {
    let record = &row.record;
    (
        record.host.clone(),
        record.epoch,
        record.session,
        record.pool.clone(),
        record.claim.clone(),
        record.shape.clone(),
        record.labels.clone(),
        record.agent_version.clone(),
    )
}

/// A beat's update that counts, under the job `beats`, the beats applied to the row: a beat
/// decided on a row that is not the latest would count one too few.
fn counted(row: &NodeRecord) -> NodeState {
    let mut state = row.state.clone();
    *state.missing_in_a_row.entry(job_id("beats")).or_default() += 1;
    state
}

/// How many beats [`counted`] counted on `row`.
fn count_of(row: &NodeRow) -> u32 {
    row.record
        .state
        .missing_in_a_row
        .get(&job_id("beats"))
        .copied()
        .unwrap_or(0)
}

/// The row of `host` as the store holds it.
fn stored<S: Store, I: IdSource>(jobs: &JobStore<S, I>, host: &HostId) -> NodeRow {
    jobs.read_node(host).expect("reads").expect("a row")
}

/// The answer a beat is, by name.
pub(super) fn kind(beat: &Beat) -> &'static str {
    match beat {
        Beat::Applied(_) => "applied",
        Beat::Superseded(_) => "superseded",
        Beat::StaleEpoch(_) => "stale-epoch",
        Beat::NoSession(_) => "no-session",
    }
}

/// The row a beat answered with, whatever the answer.
pub(super) fn row_of(beat: Beat) -> Option<NodeRow> {
    match beat {
        Beat::Applied(row) | Beat::Superseded(row) | Beat::StaleEpoch(row) => Some(row),
        Beat::NoSession(row) => row,
    }
}

/// What a check reads of a beat: its answer, the count [`counted`] keeps on its row and the
/// row's last applied beat.
fn seen(beat: Beat) -> (&'static str, u32, Option<AppliedBeat>) {
    let name = kind(&beat);
    let row = row_of(beat).expect("a row");
    (name, count_of(&row), row.record.last_beat)
}

/// The row of a beat that must have been applied.
pub(super) fn applied_row(beat: Beat) -> NodeRow {
    assert_eq!(kind(&beat), "applied", "the beat was not applied: {beat:?}");
    row_of(beat).expect("a row")
}

/// A node row of format 1 holding `fields` as they are, in tag order.
fn literal(fields: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut row = vec![1];
    let mut sorted = fields.to_vec();
    sorted.sort_by_key(|(tag, _)| *tag);
    for (tag, value) in sorted {
        row.extend_from_slice(&tag.to_be_bytes());
        row.extend_from_slice(&u32::try_from(value.len()).expect("short").to_be_bytes());
        row.extend_from_slice(&value);
    }
    row
}

/// A store time as a row holds it: whole seconds and nanoseconds.
fn time_bytes(at: StoreTime) -> Vec<u8> {
    let since = at.as_duration();
    [
        since.as_secs().to_be_bytes().to_vec(),
        since.subsec_nanos().to_be_bytes().to_vec(),
    ]
    .concat()
}

/// Two servers (two job stores over one store) take the beats of one host at once, meeting at
/// a barrier before each: of two beats of one (epoch, sequence) exactly one is applied and the
/// other is [`Beat::Superseded`] with the row as the first wrote it; of two beats of adjacent
/// sequences the newer is always applied and the older only before it, so the row's sequence
/// never goes back; and every applied beat's update was made on the latest row (none is lost).
/// The row's version moves on every round, and an applied beat answers the row the store
/// holds. These hold in whatever order the two commits land; the races this test meets depend
/// on that order, and [`a_beat_or_hello_that_loses_its_version_reads_the_row_again`] arranges
/// each one. Catches, when the race is met, a beat written over a row it did not read
/// (`Cond::Any`), a lost race that applies its beat anyway, and a version answered that is not
/// the row's.
pub fn two_servers_beating_one_host_at_once_apply_each_beat_once<B: Backend>(backend: &B) {
    let name = "two_servers_beating_one_host_at_once_apply_each_beat_once";
    let rig = rig(backend);
    let other = rig.other();
    let node = host("host-a");
    let epoch = rig
        .jobs
        .hello_node(&node, &hello())
        .expect("a hello")
        .record
        .epoch;
    let mut applied = 0;
    let mut last = stored(&rig.jobs, &node);
    for round in 1..=40u64 {
        // Even rounds race one sequence; odd rounds race a sequence and the one after it.
        let seqs = [round * 2, round * 2 + round % 2];
        let barrier = Barrier::new(2);
        let [first, second] = thread::scope(|scope| {
            let one = scope.spawn(|| {
                barrier.wait();
                rig.jobs.beat_node(&node, epoch, Some(seqs[0]), counted)
            });
            let two = scope.spawn(|| {
                barrier.wait();
                other.beat_node(&node, epoch, Some(seqs[1]), counted)
            });
            [one, two].map(|beat| beat.join().expect("a beat").expect("answers"))
        });
        let row = stored(&rig.jobs, &node);
        let kinds = [kind(&first), kind(&second)];
        let rows = [first, second].map(|beat| row_of(beat).expect("a row"));
        let won = kinds.iter().filter(|kind| **kind == "applied").count();
        applied += won;
        let mut sorted = kinds;
        sorted.sort_unstable();
        if seqs[0] == seqs[1] {
            assert_eq!(
                (sorted, rows[0] == row, rows[1] == row),
                (["applied", "superseded"], true, true),
                "{name}: round {round}: not one beat applied and the other superseded by its row"
            );
        } else {
            assert_eq!(
                (
                    kinds[1],
                    rows[1] == row,
                    !["stale-epoch", "no-session"].contains(&kinds[0])
                ),
                ("applied", true, true),
                "{name}: round {round}: the newer beat was not applied last: {kinds:?}"
            );
        }
        assert_eq!(
            (row.record.last_beat, row.version > last.version),
            (
                Some(AppliedBeat {
                    epoch,
                    seq: Some(seqs[1])
                }),
                true
            ),
            "{name}: round {round}: the row's beat went back, or its version did not move on"
        );
        last = row;
    }
    assert_eq!(
        usize::try_from(count_of(&last)).expect("fits"),
        applied,
        "{name}: an applied beat's update was made on a row another beat had moved"
    );
}

/// A beat or hello whose write loses its version to another server's commit reads the row
/// again and is judged on it: a beat of the sequence the other applied is superseded (and its
/// update is not made again), a beat newer than the other's is applied over it with its update
/// made on the row the other wrote, two beats with no sequence are both applied one after the
/// other, and two hellos take two epochs. Catches a lost race answered as applied, an update
/// made on the row before the race, and two hellos that share an epoch.
pub fn a_beat_or_hello_that_loses_its_version_reads_the_row_again<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_beat_or_hello_that_loses_its_version_reads_the_row_again";
    let rig = rig(backend);
    let node = host("host-a");
    let epoch = rig
        .jobs
        .hello_node(&node, &hello())
        .expect("a hello")
        .record
        .epoch;
    let calls = Cell::new(0u32);
    let tally = |row: &NodeRecord| {
        calls.set(calls.get() + 1);
        counted(row)
    };
    let race = |seq: Option<u64>| {
        let other = rig.other();
        let node = node.clone();
        rig.store.before_next(move || {
            let beat = other
                .beat_node(&node, epoch, seq, counted)
                .expect("answers");
            assert_eq!(kind(&beat), "applied", "the other's beat");
        });
    };
    let applied = |seq| Some(AppliedBeat { epoch, seq });

    race(Some(5));
    let lost = rig
        .jobs
        .beat_node(&node, epoch, Some(5), &tally)
        .expect("answers");
    assert_eq!(
        (row_of(lost.clone()), calls.get()),
        (Some(stored(&rig.jobs, &node)), 1),
        "{name}: a superseded beat's answer is not the row, or its update was made again"
    );
    assert_eq!(
        seen(lost),
        ("superseded", 1, applied(Some(5))),
        "{name}: a beat that lost to its own sequence was not superseded"
    );

    race(Some(6));
    let won = rig
        .jobs
        .beat_node(&node, epoch, Some(7), &tally)
        .expect("answers");
    assert_eq!(
        (seen(won), calls.get()),
        (("applied", 3, applied(Some(7))), 3),
        "{name}: a newer beat was not applied over the row the other wrote"
    );

    race(None);
    let unsequenced = rig
        .jobs
        .beat_node(&node, epoch, None, counted)
        .expect("answers");
    assert_eq!(
        seen(unsequenced),
        ("applied", 5, applied(Some(7))),
        "{name}: two beats with no sequence were not both applied, or one moved the sequence"
    );

    let other = rig.other();
    let racing = node.clone();
    rig.store.before_next(move || {
        other
            .hello_node(&racing, &hello())
            .expect("the other's hello");
    });
    let second = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    assert_eq!(
        (second.record.epoch, stored(&rig.jobs, &node)),
        (epoch + 2, second.clone()),
        "{name}: a hello that lost to another did not take the epoch after it"
    );

    // Two first hellos of a host with no row: the one that loses the absence takes epoch 2.
    let other = rig.other();
    let fresh = host("host-new");
    let racing = fresh.clone();
    rig.store.before_next(move || {
        let first = other
            .hello_node(&racing, &hello())
            .expect("the other's hello");
        assert_eq!(first.record.epoch, 1, "the other's first hello");
    });
    let late = rig.jobs.hello_node(&fresh, &hello()).expect("a hello");
    assert_eq!(
        (late.record.epoch, stored(&rig.jobs, &fresh)),
        (2, late.clone()),
        "{name}: two first hellos took one epoch"
    );
}

/// A hello gives the host the next epoch and starts its session afresh: the first listing
/// pending, no load, no beat applied, no container missed, while the listing-clean flag stays.
/// A beat of an epoch a later hello passed is [`Beat::StaleEpoch`], one of an epoch no hello
/// gave is [`Beat::NoSession`], and so is a beat of a host with no row; none of them writes or
/// calls its update. A hello resets the sequence: the new epoch's first beat is applied
/// whatever the old epoch's last. Catches an old session's beat applied after a newer hello,
/// a sequence carried across a hello, and a hello that keeps the last session's listing state.
pub fn a_beat_of_an_old_epoch_after_a_newer_hello_is_stale<B: Backend>(backend: &B) {
    let name = "a_beat_of_an_old_epoch_after_a_newer_hello_is_stale";
    let rig = rig(backend);
    let node = host("host-a");
    let calls = Cell::new(0u32);
    let tally = |row: &NodeRecord| {
        calls.set(calls.get() + 1);
        counted(row)
    };
    assert_eq!(
        rig.jobs.beat_node(&node, 1, Some(1), &tally),
        Ok(Beat::NoSession(None)),
        "{name}: a beat of a host with no row"
    );

    let first = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    assert_eq!(
        (
            first.record.epoch,
            first.record.last_beat,
            first.record.state.first_listing_pending
        ),
        (1, None, true),
        "{name}: a first hello"
    );
    let loaded = |row: &NodeRecord| NodeState {
        load: Some(LoadReading {
            seq: 3,
            ..LoadReading::default()
        }),
        first_listing_pending: false,
        last_listing_clean: false,
        ..tally(row)
    };
    let beaten = applied_row(
        rig.jobs
            .beat_node(&node, 1, Some(9), loaded)
            .expect("answers"),
    );
    assert_eq!(
        (calls.get(), beaten.record.state.load.map(|load| load.seq)),
        (1, Some(3)),
        "{name}: the beat's update was not written"
    );

    rig.tick();
    let at = rig.store.now().expect("the store's clock");
    let second = rig
        .jobs
        .hello_node(&node, &hello())
        .expect("a second hello");
    let state = &second.record.state;
    assert_eq!(
        (
            second.record.epoch,
            second.record.last_beat,
            state.first_listing_pending,
            state.load,
            state.missing_in_a_row.is_empty(),
            state.last_listing_clean,
            second.record.heard_at,
        ),
        (2, None, true, None, true, false, at),
        "{name}: a hello did not start the session afresh"
    );

    let before = rig.snapshot();
    let stale = rig
        .jobs
        .beat_node(&node, 1, Some(10), &tally)
        .expect("answers");
    assert_eq!(
        (kind(&stale), row_of(stale)),
        ("stale-epoch", Some(second.clone())),
        "{name}: an old epoch's beat"
    );
    let ahead = rig
        .jobs
        .beat_node(&node, 3, Some(1), &tally)
        .expect("answers");
    assert_eq!(
        ahead,
        Beat::NoSession(Some(second.clone())),
        "{name}: a beat of an epoch no hello gave"
    );
    assert_eq!(
        (rig.snapshot(), calls.get()),
        (before, 1),
        "{name}: a beat not applied wrote or made its update"
    );

    // The new epoch's beats: one with no sequence, then numbered ones, each judged only
    // against the sequences applied in this epoch.
    let at = |seq| Some(AppliedBeat { epoch: 2, seq });
    let beat = |seq: Option<u64>| {
        let answer = rig.jobs.beat_node(&node, 2, seq, &tally).expect("answers");
        (seen(answer), calls.get())
    };
    assert_eq!(
        beat(None),
        (("applied", 1, at(None)), 2),
        "{name}: the new epoch's first beat, with no sequence"
    );
    assert_eq!(
        beat(Some(1)),
        (("applied", 2, at(Some(1))), 3),
        "{name}: a numbered beat after one with none was judged by the old epoch's sequence"
    );
    assert_eq!(
        beat(Some(1)),
        (("superseded", 2, at(Some(1))), 3),
        "{name}: a repeated sequence was applied, or its update made"
    );
    assert_eq!(
        beat(Some(3)),
        (("applied", 3, at(Some(3))), 4),
        "{name}: a newer sequence was refused"
    );
    assert_eq!(
        beat(Some(2)),
        (("superseded", 3, at(Some(3))), 4),
        "{name}: an older sequence was applied and moved the row back"
    );
    assert_eq!(
        beat(None),
        (("applied", 4, at(Some(3))), 5),
        "{name}: a beat with no sequence was refused or took the sequence away"
    );
}

/// A host's row written through one server is what another reads: a hello and a beat applied
/// through one are read by the other's `read_node` and `live_nodes` with the version the write
/// answered, and the other's beat is then judged on it; the hello's agent version is in the row
/// and the beat keeps it. Catches a server that answers a host's session from what it wrote
/// itself rather than from the row, and a beat that drops the agent version.
pub fn a_beat_through_one_server_is_read_by_another<B: Backend>(backend: &B) {
    let name = "a_beat_through_one_server_is_read_by_another";
    let rig = rig(backend);
    let other = rig.other();
    let node = host("host-a");
    let grace = Duration::from_secs(15);
    let hello_row = other.hello_node(&node, &hello()).expect("a hello");
    let said = hello();
    assert_eq!(
        (announced(&hello_row), hello_row.record.state.phase),
        (
            (
                node.clone(),
                1,
                said.session,
                said.pool,
                said.claim,
                said.shape,
                said.labels,
                said.agent_version
            ),
            said.phase
        ),
        "{name}: the row is not what the hello announced"
    );
    assert_eq!(
        stored(&rig.jobs, &node),
        hello_row,
        "{name}: a hello through one server is not what the other reads"
    );
    let beat_row = applied_row(
        other
            .beat_node(&node, 1, Some(1), counted)
            .expect("answers"),
    );
    assert_eq!(
        announced(&beat_row),
        announced(&hello_row),
        "{name}: a beat changed what the hello announced"
    );
    assert_eq!(
        (
            rig.jobs.read_node(&node).expect("reads"),
            rig.jobs.live_nodes(grace).expect("reads")
        ),
        (Some(beat_row.clone()), vec![beat_row.clone()]),
        "{name}: a beat through one server is not what the other reads"
    );
    assert_eq!(
        rig.raw(&keys::node(&node)).map(|row| row.version),
        Some(beat_row.version),
        "{name}: the version a beat answered is not the row's"
    );
    assert_eq!(
        rig.jobs.beat_node(&node, 1, Some(1), counted),
        Ok(Beat::Superseded(beat_row)),
        "{name}: the other server judged a beat on something other than the row"
    );
}

/// A host's orphans (the jobs it lists that the store holds no record of) are written by a
/// settle and read back as written on every server, kept by a beat, and kept by a later hello
/// of the host, with their first-seen times and an operator's ask to end one: an orphan's age
/// is the store's lack of a record, which neither a beat nor a hello changes. Catches orphans
/// a backend drops, an age that starts again at each hello (an orphan would never reach its
/// bound), and an operator's end a hello forgets.
pub fn a_hosts_orphans_are_kept_by_its_beats_and_hellos<B: Backend>(backend: &B) {
    let name = "a_hosts_orphans_are_kept_by_its_beats_and_hellos";
    let rig = rig(backend);
    let other = rig.other();
    let node = host("host-a");
    let hello_row = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    assert!(
        hello_row.record.state.orphans.is_empty(),
        "{name}: a first hello holds orphans"
    );
    let orphans = BTreeMap::from([
        (
            "ghost-1".to_owned(),
            Orphan {
                first_seen: StoreTime::from_duration(Duration::new(40, 5)),
                end_asked: false,
            },
        ),
        (
            "ghost-2".to_owned(),
            Orphan {
                first_seen: StoreTime::from_duration(Duration::new(41, 0)),
                end_asked: true,
            },
        ),
    ]);
    let state = NodeState {
        orphans: orphans.clone(),
        ..hello_row.record.state.clone()
    };
    let settled = rig
        .jobs
        .settle_node(&hello_row, state)
        .expect("settles")
        .expect("at the version");
    assert_eq!(
        stored(&other, &node).record.state.orphans,
        orphans,
        "{name}: the other server reads other orphans than were written"
    );
    let beat_row = applied_row(
        other
            .beat_node(&node, settled.record.epoch, Some(1), |record| {
                record.state.clone()
            })
            .expect("answers"),
    );
    assert_eq!(
        beat_row.record.state.orphans, orphans,
        "{name}: a beat dropped the orphans"
    );
    let again = other.hello_node(&node, &hello()).expect("a hello");
    assert_eq!(
        (again.record.epoch, again.record.state.orphans),
        (settled.record.epoch + 1, orphans),
        "{name}: a hello dropped the orphans or their age"
    );
}

/// Each hello writes the agent version it gives, never the one before it: a settle keeps the
/// version, a later hello giving another version replaces it, and a later hello giving none
/// leaves the row with none. Catches a version carried across a hello into the next session
/// (the listing would name an agent that no longer runs) and a settle that drops it.
pub fn each_hello_writes_its_own_agent_version<B: Backend>(backend: &B) {
    let name = "each_hello_writes_its_own_agent_version";
    let rig = rig(backend);
    let node = host("host-a");
    let version = |row: &NodeRow| row.record.agent_version.clone();
    let first = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    let settled = rig
        .jobs
        .settle_node(&first, first.record.state.clone())
        .expect("settles")
        .expect("at the version read");
    assert_eq!(
        version(&settled),
        Some("1.2.3".to_owned()),
        "{name}: a settle dropped the version"
    );
    let upgraded = NodeHello {
        agent_version: Some("1.3.0".to_owned()),
        ..hello()
    };
    let second = rig.jobs.hello_node(&node, &upgraded).expect("a hello");
    assert_eq!(
        (second.record.epoch, version(&second)),
        (2, Some("1.3.0".to_owned())),
        "{name}: a later hello's version"
    );
    let unversioned = NodeHello {
        agent_version: None,
        ..hello()
    };
    rig.jobs.hello_node(&node, &unversioned).expect("a hello");
    assert_eq!(
        version(&stored(&rig.jobs, &node)),
        None,
        "{name}: a hello giving no version kept the one before"
    );
}

/// `heard_at` is the store's clock at the hello or applied beat, and `live_nodes` judges it by
/// the store's clock: with the store's clock at 100 s (the process's own clock reads a date
/// decades on, so a stamp or comparison made by it could not give these answers), a row is
/// live while its `heard_at` is at most the grace before the store's time, and not a moment
/// past it; a row heard 100 s before is not live; reads with the store's clock standing still
/// give the same answer; a beat brings a host back. A store whose clock cannot be read fails
/// the hello, the beat and the list, and writes nothing. Catches a `heard_at` stamped from the
/// process's clock, a liveness judged by it, an off-by-one at the grace, and a stamp made from
/// no reading.
pub fn heard_at_is_the_store_clock_and_judges_live_nodes<B: Backend>(backend: &B) {
    let name = "heard_at_is_the_store_clock_and_judges_live_nodes";
    let rig = rig(backend);
    let node = host("host-a");
    let quiet = host("host-b");
    let grace = Duration::from_secs(10);
    let now = |rig: &Rig<B::Store>| rig.store.now().expect("the store's clock");
    let live = |rig: &Rig<B::Store>| -> Vec<String> {
        rig.jobs
            .live_nodes(grace)
            .expect("reads")
            .iter()
            .map(|row| row.record.host.to_string())
            .collect()
    };

    let at = now(&rig);
    let early = rig.jobs.hello_node(&quiet, &hello()).expect("a hello");
    assert_eq!(early.record.heard_at, at, "{name}: a hello's heard_at");
    rig.clock.advance(Duration::from_secs(100));
    let at = now(&rig);
    let first = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    assert_eq!(
        first.record.heard_at, at,
        "{name}: a hello's heard_at is not the store's clock"
    );
    assert_eq!(
        live(&rig),
        ["host-a"],
        "{name}: just heard, and heard 100 s ago"
    );
    assert_eq!(
        (live(&rig), stored(&rig.jobs, &node).record.heard_at),
        (vec!["host-a".to_owned()], at),
        "{name}: with the store's clock still, the answer moved"
    );

    rig.clock.advance(at.saturating_add(grace).since(now(&rig)));
    assert_eq!(live(&rig), ["host-a"], "{name}: heard the grace ago");
    rig.clock.advance(Duration::from_nanos(1));
    assert_eq!(live(&rig), Vec::<String>::new(), "{name}: past the grace");

    let at = now(&rig);
    let beaten = applied_row(
        rig.jobs
            .beat_node(&quiet, 1, None, counted)
            .expect("answers"),
    );
    assert_eq!(
        beaten.record.heard_at, at,
        "{name}: a beat's heard_at is not the store's clock"
    );
    assert_eq!(
        live(&rig),
        ["host-b"],
        "{name}: a beat did not bring the host back"
    );

    let before = rig.snapshot();
    rig.store.fail_clock(true);
    assert!(
        rig.jobs.hello_node(&node, &hello()).is_err(),
        "{name}: a hello without the clock"
    );
    assert!(
        rig.jobs.beat_node(&quiet, 1, Some(2), counted).is_err(),
        "{name}: a beat without the clock"
    );
    assert!(
        rig.jobs.live_nodes(grace).is_err(),
        "{name}: a list without the clock"
    );
    rig.store.fail_clock(false);
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a call without the clock wrote"
    );
}

/// A node row an earlier build of the table wrote, holding only the host, epoch, pool and
/// `heard_at`, reads with every other field at its default, and is beaten and said hello to;
/// a field a later build added is read past and written back unchanged by both. A row this
/// build cannot read (a later format, one that does not parse, one under another host's key)
/// is an error to `read_node`, refused by a hello and a beat (which write nothing), left out of
/// `live_nodes`, and deleted by `forget_node` at its version. Catches a row without the
/// optional fields refused, a later build's field lost on a rewrite, and a row this build
/// cannot read written over, taken as absent, or failing the whole list.
pub fn a_node_row_without_the_optional_fields_reads_and_keeps_a_later_builds_field<B: Backend>(
    backend: &B,
) {
    let name = "a_node_row_without_the_optional_fields_reads_and_keeps_a_later_builds_field";
    let rig = rig(backend);
    let node = host("host-a");
    let key = keys::node(&node);
    let at = rig.store.now().expect("the store's clock");
    let bare = literal(&[
        (1, b"host-a".to_vec()),
        (2, 4u64.to_be_bytes().to_vec()),
        (4, b"pool-a".to_vec()),
        (5, time_bytes(at)),
    ]);
    let later_field = (900u16, b"later".to_vec());
    rig.put_raw(
        &key,
        set_field(&bare, later_field.0, &later_field.1).expect("a row"),
    );
    let read = stored(&rig.jobs, &node);
    let state = &read.record.state;
    assert_eq!(
        (
            read.record.epoch,
            read.record.session,
            read.record.claim.clone(),
            read.record.shape.clone(),
            read.record.labels.is_empty(),
            read.record.last_beat,
        ),
        (4, None, None, None, true, None),
        "{name}: the session's defaults"
    );
    assert_eq!(
        (
            state.phase,
            state.load,
            state.first_listing_pending,
            state.last_listing_clean,
            state.missing_in_a_row.is_empty()
        ),
        (Phase::Quarantined, None, false, true, true),
        "{name}: the state's defaults"
    );
    applied_row(
        rig.jobs
            .beat_node(&node, 4, Some(1), counted)
            .expect("answers"),
    );
    let kept = |rig: &Rig<B::Store>| field_of(&rig.raw(&key).expect("a row").value, later_field.0);
    assert_eq!(
        kept(&rig),
        Some(later_field.1.clone()),
        "{name}: lost by a beat"
    );
    assert_eq!(
        rig.jobs
            .hello_node(&node, &hello())
            .expect("a hello")
            .record
            .epoch,
        5,
        "{name}: a hello over an earlier row"
    );
    assert_eq!(
        kept(&rig),
        Some(later_field.1.clone()),
        "{name}: lost by a hello"
    );

    let grace = Duration::from_secs(10);
    let cases = [
        (
            "host-later",
            [&[LATER_FORMAT][..], &bare[1..]].concat(),
            JobStoreError::UnknownRow {
                key: "node/host-later".to_owned(),
            },
        ),
        (
            "host-broken",
            vec![1, 0],
            JobStoreError::Corrupt {
                key: "node/host-broken".to_owned(),
                why: DecodeError::Truncated,
            },
        ),
        (
            "host-elsewhere",
            bare.clone(),
            JobStoreError::Corrupt {
                key: "node/host-elsewhere".to_owned(),
                why: DecodeError::WrongKey,
            },
        ),
    ];
    for (text, value, error) in cases {
        let other = host(text);
        let key = keys::node(&other);
        rig.put_raw(&key, value);
        let before = rig.snapshot();
        assert_eq!(
            rig.jobs.read_node(&other),
            Err(error.clone()),
            "{name}: {text} read"
        );
        assert_eq!(
            rig.jobs.hello_node(&other, &hello()),
            Err(error.clone()),
            "{name}: {text} said hello to"
        );
        assert_eq!(
            rig.jobs.beat_node(&other, 4, Some(1), counted),
            Err(error),
            "{name}: {text} beaten"
        );
        assert_eq!(rig.snapshot(), before, "{name}: {text} was written over");
        let listed: Vec<HostId> = rig
            .jobs
            .live_nodes(grace)
            .expect("an unreadable row does not fail the list")
            .into_iter()
            .map(|row| row.record.host)
            .collect();
        assert_eq!(
            listed,
            std::slice::from_ref(&node),
            "{name}: {text} listed as live"
        );
        let version = rig.raw(&key).expect("a row").version;
        assert_eq!(
            rig.jobs.forget_node(&other, version),
            Ok(true),
            "{name}: {text} forgotten"
        );
        assert_eq!(rig.raw(&key), None, "{name}: {text} not deleted");
    }
}

/// `forget_node` deletes a host's row only at the version it is given: a row a hello or beat
/// wrote since is kept, and an absent row deletes nothing. A failed commit is the store's error
/// and changes nothing, for a forget, a hello and a beat alike. Catches a forget that deletes a
/// row a beat has just written, and a failed write answered as done.
pub fn forget_node_deletes_only_the_row_it_read_and_a_failed_write_changes_nothing<B: Backend>(
    backend: &B,
) {
    let name = "forget_node_deletes_only_the_row_it_read_and_a_failed_write_changes_nothing";
    let rig = rig(backend);
    let node = host("host-a");
    let said = rig.jobs.hello_node(&node, &hello()).expect("a hello");
    let beaten = applied_row(
        rig.jobs
            .beat_node(&node, 1, Some(1), counted)
            .expect("answers"),
    );
    assert_eq!(
        rig.jobs.forget_node(&node, said.version),
        Ok(false),
        "{name}: a row beaten since was forgotten"
    );
    assert_eq!(stored(&rig.jobs, &node), beaten, "{name}: the row changed");

    let failed = Err(JobStoreError::Store(StoreError::Failed(
        INJECTED.to_owned(),
    )));
    let before = rig.snapshot();
    rig.store.fail_next();
    let hello_failed = rig.jobs.hello_node(&node, &hello()).map(drop);
    rig.store.fail_next();
    let beat_failed = rig.jobs.beat_node(&node, 1, Some(2), counted).map(drop);
    rig.store.fail_next();
    let forget_failed = rig.jobs.forget_node(&node, beaten.version).map(drop);
    assert_eq!(
        [hello_failed, beat_failed, forget_failed],
        [failed.clone(), failed.clone(), failed],
        "{name}: a failed commit is not the store's error"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a failed write changed the store"
    );

    assert_eq!(
        rig.jobs.forget_node(&node, beaten.version),
        Ok(true),
        "{name}: the row read was not forgotten"
    );
    assert_eq!(
        rig.jobs.read_node(&node),
        Ok(None),
        "{name}: a forgotten row reads"
    );
    assert_eq!(
        rig.jobs.forget_node(&node, beaten.version),
        Ok(false),
        "{name}: an absent row was forgotten"
    );
    let gone = rig
        .jobs
        .beat_node(&node, 1, Some(3), counted)
        .expect("answers");
    assert_eq!(
        (kind(&gone), row_of(gone)),
        ("no-session", None),
        "{name}: a forgotten host's beat found a session"
    );
}
