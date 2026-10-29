//! The slots: every run holds one slot, a slot has one container user, and a slot is given
//! to a new run only after the cleanup of its last run passed.

use js_agent::lifecycle::{CheckFinding, SLOTS, Slots, label};

use super::{Harness, Options, job, names};

/// The slots of the containers created so far, in order of creation.
fn created_slots(h: &Harness) -> Vec<u32> {
    h.fake.created().iter().map(|spec| spec.slot).collect()
}

/// The labels a lifecycle writes for a run of 1 CPU and 1 GiB, with `slot` when given.
fn labelled<'a>(run: &'a str, slot: Option<&'a str>) -> Vec<(&'static str, &'a str)> {
    let mut labels = vec![
        (label::RUN, run),
        (label::LEASE, "lease-old"),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::GPUS, ""),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
    ];
    labels.extend(slot.map(|slot| (label::SLOT, slot)));
    labels
}

/// Asserts that two runs held at once get the slots 0 and 1, that the container is told
/// which (the field of the spec and the label that a restarted agent reads), and that the
/// agent lists both slots as busy. It catches runs that share a slot, and a slot that
/// is not written where a restart can read it.
#[test]
fn concurrent_runs_hold_different_slots() {
    let mut h = Harness::new("slots-two");
    h.start(job("r1"));
    h.start(job("r2"));
    assert_eq!(created_slots(&h), [0, 1]);
    let labels: Vec<Option<String>> = h
        .fake
        .created()
        .iter()
        .map(|spec| spec.labels.get(label::SLOT).cloned())
        .collect();
    assert_eq!(labels, [Some("0".to_owned()), Some("1".to_owned())]);
    assert_eq!(h.life.busy_slots(), [0, 1]);
    assert!(h.life.quarantined_slots().is_empty());
}

/// Asserts that a slot whose run passed its cleanup is given out again, lowest first, so
/// the next run gets the same uid again, and that a run started while another still holds
/// slot 0 does not get it. It catches a slot that is never given back and one that is
/// given to two runs.
#[test]
fn a_slot_whose_cleanup_passed_is_used_again() {
    let mut h = Harness::new("slots-reuse");
    h.start(job("r1"));
    h.start(job("r2"));
    h.start(job("r3"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.busy_slots(), [1, 2], "r1's slot is free");
    h.start(job("r4"));
    assert_eq!(created_slots(&h), [0, 1, 2, 0], "r4 has r1's slot again");
    h.start(job("r5"));
    assert_eq!(created_slots(&h)[4], 3, "and the next one a new slot");
}

/// Asserts that a slot stays busy while the cleanup of its run has not passed (a container
/// that cannot be removed), and is free on the step after the cleanup went through. It
/// catches a slot given back when the run exits, before its container and scratch are gone
/// and the checks have run.
#[test]
fn a_slot_is_busy_until_the_cleanup_passed() {
    let mut h = Harness::new("slots-cleanup");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.fake.fail("remove", 1);
    h.life.step();
    assert_eq!(h.life.busy_slots(), [0], "the container is still there");
    assert!(h.life.quarantined_slots().is_empty());
    h.life.step();
    assert!(h.life.busy_slots().is_empty());
    assert!(h.life.quarantined_slots().is_empty());
}

/// Asserts that a run whose post-job check found a leftover quarantines its slot for good
/// (the slot is not busy and not free), that the other run keeps working and gives its own
/// slot back when its cleanup passes, and that nothing new starts in the quarantined slot
/// (the host is tainted, as before). It catches a leftover process at a slot's uid that
/// the next run of that slot would inherit.
#[test]
fn a_leftover_quarantines_its_slot_and_the_others_keep_working() {
    let leftover = CheckFinding::residue("process-as-job-user", "pid 41 of user 232072");
    let mut h = Harness::with(
        "slots-quarantine",
        Options {
            findings: vec![("r1", leftover)],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.quarantined_slots(), [0]);
    assert_eq!(h.life.busy_slots(), [1], "r2 still runs in its slot");
    assert_eq!(h.held(), ["r1", "r2"]);

    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert!(events.contains(&"freed r2".to_owned()), "{events:?}");
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(
        h.life.quarantined_slots(),
        [0],
        "r1's slot is never given back"
    );
    assert_eq!(names(&h.start(job("r3"))), ["refused r3 host-tainted"]);
    assert_eq!(created_slots(&h), [0, 1]);
}

/// Asserts that a restarted agent takes the slot of an adopted container from its label and
/// keeps it busy (the next run gets another slot); that a container with no slot label (an
/// earlier agent made it) is adopted with no slot; and that a slot label that is not a
/// number, is beyond the last slot, or names a slot an adopted run holds already makes the
/// container unadoptable. It catches a restart that hands an adopted run's uid to a
/// new run.
#[test]
fn adopted_runs_keep_their_slots() {
    let mut h = Harness::new("slots-adopt");
    h.start(job("r1"));
    h.start(job("r2"));
    h.restart().unwrap();
    assert_eq!(h.life.busy_slots(), [0, 1]);
    h.start(job("r3"));
    assert_eq!(created_slots(&h), [0, 1, 2]);

    let mut h = Harness::new("slots-adopt-labels");
    let too_big = SLOTS.to_string();
    h.fake.leak(&labelled("good", Some("1")), 0);
    h.fake.leak(&labelled("text", Some("x")), 0);
    h.fake.leak(&labelled("big", Some(&too_big)), 0);
    h.fake.leak(&labelled("twice", Some("1")), 0);
    h.fake.leak(&labelled("legacy", None), 0);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "adopted good",
            "finding - unadoptable-container",
            "finding - unadoptable-container",
            "finding - unadoptable-container",
            "adopted legacy",
        ]
    );
    assert_eq!(h.held(), ["good", "legacy"]);
    assert_eq!(h.life.busy_slots(), [1], "the legacy run holds no slot");
    h.start(job("r1"));
    assert_eq!(created_slots(&h), [0]);
}

/// Asserts that a container the agent cannot remove after a restart, whose labels name a
/// slot, quarantines that slot, and that removing it on a later step does not free the
/// slot (no check ran for it). It catches a slot handed out while a process of its last
/// run may still be alive.
#[test]
fn a_container_that_cannot_be_removed_quarantines_its_slot() {
    let mut h = Harness::new("slots-stuck");
    let broken: Vec<(&str, &str)> = labelled("odd", Some("3"))
        .into_iter()
        .map(|(key, value)| match key == label::CPU_MILLIS {
            true => (key, "lots"),
            false => (key, value),
        })
        .collect();
    h.fake.leak(&broken, 0);
    h.fake.fail("remove", 1);
    h.restart().unwrap();
    assert_eq!(h.life.quarantined_slots(), [3]);
    h.life.step();
    assert!(h.fake.existing().is_empty(), "removed on the step");
    assert_eq!(h.life.quarantined_slots(), [3]);
}

/// Asserts the slot table by itself: the lowest free slot first, none beyond the count, a
/// released slot back at once, a quarantined slot never again (not by release, not by
/// hold), and the slots held and quarantined listed. It catches an allocator that skips
/// or repeats a slot.
#[test]
fn the_table_gives_the_lowest_free_slot_and_never_a_quarantined_one() {
    let mut slots = Slots::new(3);
    assert!(slots.has_room());
    assert_eq!([slots.take(), slots.take(), slots.take()], [0, 1, 2]);
    assert!(!slots.has_room(), "no slot is left");
    assert_eq!(slots.held(), [0, 1, 2]);

    slots.release(1);
    assert!(slots.has_room());
    assert!(slots.is_free(1) && !slots.is_free(0));
    assert_eq!(slots.take(), 1);

    slots.quarantine(0);
    assert_eq!(slots.held(), [1, 2]);
    assert_eq!(slots.quarantined(), [0]);
    slots.release(0);
    assert!(!slots.is_free(0), "a release does not undo a quarantine");
    assert!(!slots.hold(0));
    slots.release(2);
    assert_eq!(slots.take(), 2, "and a quarantined slot is skipped");

    let mut idle = Slots::new(3);
    idle.quarantine(1);
    assert_eq!(idle.take(), 0);
    assert_eq!(idle.take(), 2, "a slot quarantined while idle is skipped");
    assert!(!idle.has_room());
}

/// Asserts that a slot is held once: `hold` takes a free slot and refuses a held one and
/// one past the count. It catches two adopted runs on one uid.
#[test]
fn a_slot_is_held_once_and_only_within_the_count() {
    let mut slots = Slots::new(2);
    assert!(slots.hold(1));
    assert!(!slots.hold(1));
    assert!(!slots.hold(2), "past the count");
    assert!(!slots.is_free(2));
    assert_eq!(slots.take(), 0);
    assert!(!slots.has_room());
}
