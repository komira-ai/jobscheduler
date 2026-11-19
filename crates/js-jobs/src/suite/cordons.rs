//! The cordon table: [`crate::JobStore::cordon_host`] places, renews, extends and takes over a
//! cordon by the rule of `js_types::cordon` and writes nothing when the cordon is another
//! holder's, [`crate::JobStore::uncordon_host`] lifts by the same rule, a cordon lapses with
//! the store's clock, a request that races another is a conflict, a row that cannot be read is
//! refused, [`crate::JobStore::cordons`] lists the cordons in force and
//! [`crate::JobStore::restore_cordon`] never overwrites one; and a placement of every kind
//! commits nothing on a cordoned host and commits once the cordon has lapsed or been lifted.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError};
use js_types::cordon::{Asker, Cordon};
use js_types::{CordonHolder, HostId};

use super::{Rig, full_spec, host, rig, share};
use crate::codec::{LATER_FORMAT, encode_cordon};
use crate::interfere::INJECTED;
use crate::keys;
use crate::{CordonWrite, DecodeError, JobId, JobState, JobStoreError, Placement, UncordonWrite};

/// The holder `text`.
fn holder(text: &str) -> CordonHolder {
    CordonHolder::new(text).expect("a valid holder")
}

/// A request of the holder `text`.
fn by(text: &str) -> Asker {
    Asker::Holder(holder(text))
}

/// `secs` seconds.
fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// A cordon with `left` to run, held by `who`.
fn cordon_of(left: u64, who: Option<&str>) -> Cordon<Duration> {
    Cordon {
        until: secs(left),
        holder: who.map(holder),
    }
}

/// The error an operation answers when its commit fails.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// `held` with its time left rounded up to a whole second: a commit costs the store's clock a
/// little time, so a cordon read after a write has a few milliseconds less than it was given.
fn whole(held: Cordon<Duration>) -> Cordon<Duration> {
    Cordon {
        until: secs(
            held.until
                .as_secs()
                .saturating_add(u64::from(held.until.subsec_nanos() > 0)),
        ),
        ..held
    }
}

/// [`crate::JobStore::cordon_host`] on `rig`, with the time left [`whole`].
fn cordon_host<S: Store>(
    rig: &Rig<S>,
    host: &HostId,
    lease: Duration,
    by: &Asker,
) -> Result<CordonWrite, JobStoreError> {
    let done = rig.jobs.cordon_host(host, lease, by)?;
    Ok(CordonWrite {
        cordon: whole(done.cordon),
        ..done
    })
}

/// [`crate::JobStore::uncordon_host`] on `rig`, with the time left [`whole`].
fn uncordon_host<S: Store>(
    rig: &Rig<S>,
    host: &HostId,
    who: Option<&CordonHolder>,
) -> Result<UncordonWrite, JobStoreError> {
    let done = rig.jobs.uncordon_host(host, who)?;
    Ok(UncordonWrite {
        cordon: done.cordon.map(whole),
        ..done
    })
}

/// [`crate::JobStore::cordons`] on `rig`, with the time left [`whole`].
fn cordons<S: Store>(rig: &Rig<S>) -> Result<BTreeMap<HostId, Cordon<Duration>>, JobStoreError> {
    Ok(rig
        .jobs
        .cordons()?
        .into_iter()
        .map(|(host, held)| (host, whole(held)))
        .collect())
}

/// What a cordon of `left` seconds that was or was not in force reads as.
fn written(cordon: Cordon<Duration>, already: bool) -> CordonWrite {
    CordonWrite {
        cordon,
        already,
        held_by_other: false,
    }
}

/// A cordon is placed with the holder it names and a row that expires with it, a request of the
/// holder renews it, a request that names none extends it and leaves its holder, neither
/// shortens it, and every write moves the written mark.
pub fn a_cordon_is_placed_and_renewed_by_its_holder<B: Backend>(backend: &B) {
    let name = "a_cordon_is_placed_and_renewed_by_its_holder";
    let rig = rig(backend);
    let a = host("host-a");
    let key = keys::cordon(&a);
    let before = rig.jobs.written().expect("reads");
    let opened = rig.store.now();
    assert_eq!(
        cordon_host(&rig, &a, secs(100), &by("req-1")),
        Ok(written(cordon_of(100, Some("req-1")), false)),
        "{name}: a cordon with none in force"
    );
    assert_ne!(
        rig.jobs.written().expect("reads"),
        before,
        "{name}: the cordon did not move the written mark"
    );
    let row = rig.raw(&key).expect("the cordon is a row");
    assert_eq!(
        row.expires_at,
        Some(opened.saturating_add(secs(100))),
        "{name}: the row does not expire with the cordon"
    );
    rig.clock.advance(secs(30));
    assert_eq!(
        cordon_host(&rig, &a, secs(50), &by("req-1")),
        Ok(written(cordon_of(70, Some("req-1")), true)),
        "{name}: a renewal shorter than the time left shortened it"
    );
    assert_eq!(
        cordon_host(&rig, &a, secs(200), &by("req-1")),
        Ok(written(cordon_of(200, Some("req-1")), true)),
        "{name}: the holder's renewal did not extend it"
    );
    assert_eq!(
        cordon_host(&rig, &a, secs(10), &Asker::Unnamed),
        Ok(written(cordon_of(200, Some("req-1")), true)),
        "{name}: a request that names no holder and does not extend it changed the holder"
    );
    assert_eq!(
        cordon_host(&rig, &a, secs(500), &Asker::Unnamed),
        Ok(written(cordon_of(500, None), true)),
        "{name}: a request that names no holder extended it and left it the holder's"
    );
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::from([(a, cordon_of(500, None))])),
        "{name}: the row does not hold the cordon in force"
    );
}

/// A request of another holder changes nothing: the row, its version and its expiry stay, no
/// commit is made and the written mark does not move; the answer says the cordon is held by
/// another and gives it with the time it has left.
pub fn another_holders_cordon_is_left_as_it_was<B: Backend>(backend: &B) {
    let name = "another_holders_cordon_is_left_as_it_was";
    let rig = rig(backend);
    let a = host("host-a");
    cordon_host(&rig, &a, secs(100), &by("req-1")).expect("cordons");
    rig.clock.advance(secs(10));
    let row = rig.raw(&keys::cordon(&a));
    let commits = rig.store.commit_stats().txns;
    let mark = rig.jobs.written().expect("reads");
    assert_eq!(
        cordon_host(&rig, &a, secs(1000), &by("req-2")),
        Ok(CordonWrite {
            cordon: cordon_of(90, Some("req-1")),
            already: true,
            held_by_other: true,
        }),
        "{name}: the answer"
    );
    assert_eq!(
        rig.raw(&keys::cordon(&a)),
        row,
        "{name}: the row, its version or its expiry changed"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "{name}: a request that changes nothing made a commit"
    );
    assert_eq!(
        rig.jobs.written().expect("reads"),
        mark,
        "{name}: the written mark moved"
    );
}

/// A take-over makes the cordon the asker's and extends it as a renewal does, and a cordon
/// that no request named a holder for has none, whoever asks to renew it.
pub fn a_take_over_makes_the_cordon_the_askers<B: Backend>(backend: &B) {
    let name = "a_take_over_makes_the_cordon_the_askers";
    let rig = rig(backend);
    let a = host("host-a");
    let b = host("host-b");
    cordon_host(&rig, &a, secs(100), &by("req-1")).expect("cordons");
    assert_eq!(
        cordon_host(&rig, &a, secs(40), &Asker::TakeOver(holder("req-2"))),
        Ok(written(cordon_of(100, Some("req-2")), true)),
        "{name}: a take-over"
    );
    assert_eq!(
        cordon_host(&rig, &a, secs(10), &by("req-1")),
        Ok(CordonWrite {
            cordon: cordon_of(100, Some("req-2")),
            already: true,
            held_by_other: true,
        }),
        "{name}: the earlier holder still renews"
    );
    assert_eq!(
        cordon_host(&rig, &b, secs(60), &Asker::Unnamed),
        Ok(written(cordon_of(60, None), false)),
        "{name}: an unnamed cordon"
    );
    assert_eq!(
        cordon_host(&rig, &b, secs(10), &by("req-3")),
        Ok(CordonWrite {
            cordon: cordon_of(60, None),
            already: true,
            held_by_other: true,
        }),
        "{name}: a holder renewed a cordon that has none"
    );
}

/// A cordon lapses when the store's clock reaches its end: the row is gone, no cordon is
/// listed, a request places a new one (`already` false) and a lift finds nothing.
pub fn a_cordon_lapses_with_the_store_clock<B: Backend>(backend: &B) {
    let name = "a_cordon_lapses_with_the_store_clock";
    let rig = rig(backend);
    let a = host("host-a");
    cordon_host(&rig, &a, secs(60), &by("req-1")).expect("cordons");
    rig.clock.advance(secs(59));
    assert_eq!(
        cordons(&rig).map(|all| all.len()),
        Ok(1),
        "{name}: a cordon with a second left was gone"
    );
    rig.clock.advance(secs(1));
    assert_eq!(cordons(&rig), Ok(BTreeMap::new()), "{name}: listed");
    assert_eq!(
        rig.raw(&keys::cordon(&a)),
        None,
        "{name}: the row is still read"
    );
    assert_eq!(
        uncordon_host(&rig, &a, None),
        Ok(UncordonWrite {
            lifted: false,
            held_by_other: false,
            cordon: None
        }),
        "{name}: a lift of a lapsed cordon"
    );
    assert_eq!(
        cordon_host(&rig, &a, secs(30), &by("req-2")),
        Ok(written(cordon_of(30, Some("req-2")), false)),
        "{name}: another holder's request after the lapse"
    );
}

/// A lift ends a cordon by the rule: one that names no holder ends any, one that names the
/// holder ends its cordon, one that names another leaves it and says so, and a lift with none in
/// force writes nothing.
pub fn a_lift_ends_a_cordon_by_the_rule<B: Backend>(backend: &B) {
    let name = "a_lift_ends_a_cordon_by_the_rule";
    let rig = rig(backend);
    let a = host("host-a");
    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        uncordon_host(&rig, &a, Some(&holder("req-1"))),
        Ok(UncordonWrite {
            lifted: false,
            held_by_other: false,
            cordon: None
        }),
        "{name}: a lift with no cordon"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "{name}: a lift of nothing made a commit"
    );
    cordon_host(&rig, &a, secs(100), &by("req-1")).expect("cordons");
    let row = rig.raw(&keys::cordon(&a));
    assert_eq!(
        uncordon_host(&rig, &a, Some(&holder("req-2"))),
        Ok(UncordonWrite {
            lifted: false,
            held_by_other: true,
            cordon: Some(cordon_of(100, Some("req-1")))
        }),
        "{name}: another holder's lift"
    );
    assert_eq!(rig.raw(&keys::cordon(&a)), row, "{name}: the lift wrote");
    let before = rig.jobs.written().expect("reads");
    assert_eq!(
        uncordon_host(&rig, &a, Some(&holder("req-1"))),
        Ok(UncordonWrite {
            lifted: true,
            held_by_other: false,
            cordon: None
        }),
        "{name}: the holder's lift"
    );
    assert_ne!(
        rig.jobs.written().expect("reads"),
        before,
        "{name}: the lift did not move the written mark"
    );
    assert_eq!(rig.raw(&keys::cordon(&a)), None, "{name}: the row stayed");
    cordon_host(&rig, &a, secs(100), &by("req-1")).expect("cordons");
    assert_eq!(
        uncordon_host(&rig, &a, None).map(|lift| lift.lifted),
        Ok(true),
        "{name}: a lift that names no holder"
    );
    assert_eq!(cordons(&rig), Ok(BTreeMap::new()), "{name}: left");
}

/// A cordon, a lift or a restore that loses to a write of the row is a conflict that writes
/// nothing, and a commit the store fails is the store's error, for each.
pub fn a_cordon_request_that_races_or_fails_writes_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_cordon_request_that_races_or_fails_writes_nothing";
    let rig = rig(backend);
    let a = host("host-a");
    let conflict = JobStoreError::CordonConflict { host: a.clone() };
    let other = rig.other();
    let raced = a.clone();
    rig.store.before_next(move || {
        other
            .cordon_host(&raced, secs(10), &by("req-2"))
            .expect("the other writer cordons");
    });
    assert_eq!(
        cordon_host(&rig, &a, secs(100), &by("req-1")),
        Err(conflict.clone()),
        "{name}: a cordon over a row written since its read"
    );
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::from([(a.clone(), cordon_of(10, Some("req-2")))])),
        "{name}: the lost cordon wrote"
    );
    let other = rig.other();
    let raced = a.clone();
    rig.store.before_next(move || {
        other
            .cordon_host(&raced, secs(500), &Asker::Unnamed)
            .expect("the other writer extends");
    });
    assert_eq!(
        cordon_host(&rig, &a, secs(20), &by("req-2")),
        Err(conflict.clone()),
        "{name}: a renewal over a row written since its read"
    );
    let other = rig.other();
    let raced = a.clone();
    rig.store.before_next(move || {
        other
            .cordon_host(&raced, secs(900), &Asker::Unnamed)
            .expect("the other writer extends");
    });
    assert_eq!(
        uncordon_host(&rig, &a, None),
        Err(conflict.clone()),
        "{name}: a lift over a row written since its read"
    );
    assert_eq!(
        cordons(&rig).map(|all| all.len()),
        Ok(1),
        "{name}: the lost lift deleted"
    );
    let other = rig.other();
    let raced = a.clone();
    rig.store.before_next(move || {
        other.uncordon_host(&raced, None).expect("the other lifts");
    });
    assert_eq!(
        uncordon_host(&rig, &a, None),
        Err(conflict),
        "{name}: a lift of a row another lifted"
    );

    rig.store.fail_next();
    assert_eq!(
        cordon_host(&rig, &a, secs(5), &Asker::Unnamed),
        Err(injected()),
        "{name}: a failed cordon"
    );
    cordon_host(&rig, &a, secs(5), &Asker::Unnamed).expect("cordons");
    rig.store.fail_next();
    assert_eq!(
        uncordon_host(&rig, &a, None),
        Err(injected()),
        "{name}: a failed lift"
    );
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.restore_cordon(&a, &cordon_of(5, None)),
        Err(injected()),
        "{name}: a failed restore"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        cordon_host(&rig, &a, secs(5), &Asker::Unnamed),
        Err(injected()),
        "{name}: a failed read"
    );
    assert_eq!(
        cordons(&rig).map(|all| all.len()),
        Ok(1),
        "{name}: the failures changed the cordon"
    );
}

/// A cordon row that does not parse, that a later build wrote, or that is live with no expiry
/// is refused by a cordon and a lift of its host, and nothing is written over it; the list of
/// cordons keeps such a host out of placement (for ever for a row with no expiry), and passes
/// over a key that names no host, so one row it cannot read stops no other host's placement.
pub fn a_cordon_row_that_cannot_be_read_is_refused_and_kept_in_force<B: Backend>(backend: &B) {
    let name = "a_cordon_row_that_cannot_be_read_is_refused_and_kept_in_force";
    let rig = rig(backend);
    let a = host("host-a");
    let key = keys::cordon(&a);
    let corrupt = |why| JobStoreError::Corrupt {
        key: key.clone(),
        why,
    };
    rig.put_raw(&key, vec![0]);
    assert_eq!(
        cordon_host(&rig, &a, secs(5), &Asker::Unnamed),
        Err(corrupt(DecodeError::BadFormat)),
        "{name}: a row that does not parse"
    );
    assert_eq!(
        uncordon_host(&rig, &a, None),
        Err(corrupt(DecodeError::BadFormat)),
        "{name}: a lift over a row that does not parse"
    );
    rig.put_raw(&key, vec![LATER_FORMAT]);
    assert_eq!(
        cordon_host(&rig, &a, secs(5), &Asker::Unnamed),
        Err(JobStoreError::UnknownRow { key: key.clone() }),
        "{name}: a later build's row"
    );
    let no_expiry = encode_cordon(None).expect("encodes");
    rig.put_raw(&key, no_expiry.clone());
    assert_eq!(
        cordon_host(&rig, &a, secs(5), &Asker::Unnamed),
        Err(corrupt(DecodeError::Missing { tag: 0 })),
        "{name}: a live row with no expiry"
    );
    let for_ever = whole(Cordon {
        until: Duration::MAX,
        holder: None,
    });
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::from([(a.clone(), for_ever.clone())])),
        "{name}: the list over a row with no expiry keeps the host out"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        Some(no_expiry),
        "{name}: a refused request wrote"
    );
    for unread in [vec![0], vec![LATER_FORMAT]] {
        rig.put_raw(&key, unread);
        assert_eq!(
            cordons(&rig),
            Ok(BTreeMap::from([(a.clone(), for_ever.clone())])),
            "{name}: the list over a row it cannot read keeps the host out"
        );
    }
    rig.delete_raw(&key);
    let stray = "cordon/not a host";
    rig.put_raw(stray, vec![LATER_FORMAT]);
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::new()),
        "{name}: a row under a key that names no host is no host's cordon"
    );
}

/// [`crate::JobStore::cordons`] lists the live rows by host with the time each has left, and
/// none that lapsed; [`crate::JobStore::restore_cordon`] writes a cordon for the time it has
/// left when none is in force, answers `false` and leaves the cordon in force otherwise, and
/// writes again once the cordon has lapsed.
pub fn restore_cordon_writes_once_and_never_overwrites<B: Backend>(backend: &B) {
    let name = "restore_cordon_writes_once_and_never_overwrites";
    let rig = rig(backend);
    let (a, b) = (host("host-a"), host("host-b"));
    assert_eq!(cordons(&rig), Ok(BTreeMap::new()), "{name}: empty");
    let mark = rig.jobs.written().expect("reads");
    assert_eq!(
        rig.jobs.restore_cordon(&a, &cordon_of(100, Some("req-1"))),
        Ok(true),
        "{name}: a restore with none in force"
    );
    assert_ne!(
        rig.jobs.written().expect("reads"),
        mark,
        "{name}: the restore did not move the written mark"
    );
    assert_eq!(
        rig.jobs.restore_cordon(&b, &cordon_of(30, None)),
        Ok(true),
        "{name}: a cordon with no holder"
    );
    let row = rig.raw(&keys::cordon(&a));
    assert_eq!(
        rig.jobs.restore_cordon(&a, &cordon_of(900, Some("req-9"))),
        Ok(false),
        "{name}: a restore over a cordon in force"
    );
    assert_eq!(rig.raw(&keys::cordon(&a)), row, "{name}: it overwrote");
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::from([
            (a.clone(), cordon_of(100, Some("req-1"))),
            (b.clone(), cordon_of(30, None)),
        ])),
        "{name}: the list"
    );
    rig.clock.advance(secs(40));
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::from([(a.clone(), cordon_of(60, Some("req-1")))])),
        "{name}: a lapsed cordon was listed"
    );
    assert_eq!(
        rig.jobs.restore_cordon(&b, &cordon_of(10, Some("req-2"))),
        Ok(true),
        "{name}: a restore after the lapse"
    );
}

/// [`crate::JobStore::restore_cordons`] carries the cordons given into the table, and marks
/// the store, the first time; a later call writes nothing, so a cordon lifted since (here, by a
/// lift of its own holder) is not written back, and a cordon given anew is not written either.
pub fn restore_cordons_carries_the_bindings_once<B: Backend>(backend: &B) {
    let name = "restore_cordons_carries_the_bindings_once";
    let rig = rig(backend);
    let (a, b) = (host("host-a"), host("host-b"));
    let first = BTreeMap::from([(a.clone(), cordon_of(100, Some("req-1")))]);
    assert_eq!(
        rig.jobs.restore_cordons(&first),
        Ok(true),
        "{name}: the first"
    );
    assert_eq!(
        cordons(&rig),
        Ok(first.clone()),
        "{name}: the first restore wrote its cordons"
    );
    assert!(
        rig.raw(keys::CORDONS_RESTORED).is_some(),
        "{name}: the store is not marked"
    );
    let lifted = uncordon_host(&rig, &a, Some(&holder("req-1")));
    assert!(lifted.expect("lifts").lifted, "{name}: the lift");
    let again = BTreeMap::from([
        (a.clone(), cordon_of(100, Some("req-1"))),
        (b.clone(), cordon_of(30, None)),
    ]);
    assert_eq!(rig.jobs.restore_cordons(&again), Ok(false), "{name}: again");
    assert_eq!(
        cordons(&rig),
        Ok(BTreeMap::new()),
        "{name}: a later restore wrote a cordon back"
    );
}

/// Records the queued job `text`.
fn queued(rig: &Rig<impl Store>, text: &str) -> JobId {
    rig.queue_spec(text, full_spec())
}

/// Asserts that the job `id` is queued with no allocation: a placement that was refused
/// committed neither row.
fn still_queued(rig: &Rig<impl Store>, id: &JobId, what: &str) {
    assert_eq!(
        rig.jobs.get_job(id).expect("reads").map(|job| job.state),
        Some(JobState::Queued),
        "{what}: the job left the queue"
    );
    assert_eq!(
        rig.raw(&keys::allocation(id)),
        None,
        "{what}: an allocation was written"
    );
}

/// Placing a job (`place`, `place_with` or `place_claimed`) on a cordoned host commits nothing
/// and leaves the job queued, a placement on another host is made, and the same placement is
/// made once the cordon has lapsed and once it was lifted.
pub fn a_placement_on_a_cordoned_host_commits_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_placement_on_a_cordoned_host_commits_nothing";
    let rig = rig(backend);
    let a = host("host-a");
    let id = queued(&rig, "cd-1");
    let plain = Placement::default();
    cordon_host(&rig, &a, secs(100), &by("req-1")).expect("cordons");
    let commits = rig.store.commit_stats().txns;
    let stale = Err(JobStoreError::Stale { id: id.clone() });
    assert_eq!(
        rig.jobs.place(&id, a.clone(), share()).map(drop),
        stale,
        "{name}: place"
    );
    still_queued(&rig, &id, "place");
    assert_eq!(
        rig.jobs
            .place_with(&id, a.clone(), share(), plain.clone())
            .map(drop),
        stale,
        "{name}: place_with"
    );
    still_queued(&rig, &id, "place_with");
    assert_eq!(
        rig.jobs
            .place_claimed(&id, &a, share(), &plain, None)
            .map(drop),
        Err(JobStoreError::HostCordoned {
            id: id.clone(),
            host: a.clone()
        }),
        "{name}: place_claimed"
    );
    still_queued(&rig, &id, "place_claimed");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "{name}: a refused placement made a commit"
    );

    rig.jobs
        .place(&id, host("host-b"), share())
        .expect("another host is placed on");
    let lapse = queued(&rig, "cd-2");
    rig.clock.advance(secs(100));
    rig.jobs
        .place(&lapse, a.clone(), share())
        .expect("a lapsed cordon does not hold");
    let with = queued(&rig, "cd-3");
    cordon_host(&rig, &a, secs(100), &Asker::Unnamed).expect("cordons");
    uncordon_host(&rig, &a, None).expect("lifts");
    rig.jobs
        .place_with(&with, a.clone(), share(), plain.clone())
        .expect("a lifted cordon does not hold");
    let claimed = queued(&rig, "cd-4");
    cordon_host(&rig, &a, secs(100), &Asker::Unnamed).expect("cordons");
    rig.clock.advance(secs(100));
    rig.jobs
        .place_claimed(&claimed, &a, share(), &plain, None)
        .expect("a lapsed cordon does not hold a claimed placement");
}

/// A cordon another server took between `place`'s reads and its commit makes the placement
/// stale and writes nothing, and one taken before a claimed placement decides makes it answer
/// that the host is cordoned and write nothing.
pub fn a_cordon_taken_during_a_placement_makes_it_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_cordon_taken_during_a_placement_makes_it_stale";
    let rig = rig(backend);
    let a = host("host-a");
    let id = queued(&rig, "cr-1");
    let other = rig.other();
    let raced = a.clone();
    rig.store.before_next(move || {
        other
            .cordon_host(&raced, secs(100), &by("req-1"))
            .expect("the other server cordons");
    });
    assert_eq!(
        rig.jobs.place(&id, a.clone(), share()).map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a placement over a cordon taken since its read"
    );
    still_queued(&rig, &id, name);
    uncordon_host(&rig, &a, None).expect("lifts");
    let claimed = queued(&rig, "cr-2");
    let other = rig.other();
    let raced = a.clone();
    rig.store.before_next(move || {
        other
            .cordon_host(&raced, secs(100), &by("req-1"))
            .expect("the other server cordons");
    });
    let answer = rig
        .jobs
        .place_claimed(&claimed, &a, share(), &Placement::default(), None);
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::HostCordoned {
            id: claimed.clone(),
            host: a.clone()
        }),
        "{name}: a claimed placement decided over a cordon another server took"
    );
    still_queued(&rig, &claimed, name);
}
