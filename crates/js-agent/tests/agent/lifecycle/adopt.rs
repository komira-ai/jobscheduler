//! A restart: the containers a previous process labelled are taken over, listed first,
//! and a start for one of them is not a new run.

use js_agent::lifecycle::label;
use js_agent_wire::limits::MAX_HELD;
use js_agent_wire::{HeldRun, HeldState, Outcome};

use super::{Harness, job, names};

/// The labels a container started by the lifecycle carries, for a run that booked 1 CPU,
/// 1 GiB and GPU 1, with a wall limit of a minute.
fn labels<'a>(run: &'a str, gpus: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![
        (label::RUN, run),
        (label::LEASE, "lease-old"),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::DISK_BYTES, "0"),
        (label::GPUS, gpus),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
    ]
}

/// Asserts the work-runs-twice case: an agent that restarts while jobs run takes their
/// containers over before anything else, lists them first (before a run the server
/// starts afterwards), books what they booked, and does not run a job again when the
/// server resends its start. It catches a restarted agent that says it holds nothing,
/// which makes the server requeue running work.
#[test]
fn restart_readopts_labelled_containers_listed_first() {
    let mut h = Harness::new("adopt");
    let mut gpu = job("r1");
    gpu.gpu_indices = vec![1];
    h.start(gpu.clone());
    h.start(job("r2"));
    h.fake.exit("r2", 0);
    assert_eq!(h.fake.created().len(), 2);

    let events = h.restart().unwrap();
    assert_eq!(
        names(&events),
        ["adopted r2", "adopted r1"],
        "the exited one first"
    );
    assert_eq!(h.fake.created().len(), 2, "adopting creates nothing");
    assert_eq!(h.held(), ["r2", "r1"], "held before any heartbeat");
    assert_eq!(h.life.free().cpu_millis, 2000);
    assert_eq!(h.life.free().gpus.into_iter().collect::<Vec<_>>(), [0]);

    let resent = h.start(gpu);
    assert!(resent.is_empty());
    assert_eq!(h.fake.created().len(), 2, "the work did not run twice");
    h.start(job("r3"));
    assert_eq!(h.held(), ["r2", "r1", "r3"], "adopted runs come first");

    h.life.step();
    let result = h
        .life
        .results()
        .into_iter()
        .find(|r| r.run_id == "r2")
        .unwrap();
    assert_eq!(
        result.outcome,
        Outcome::Succeeded,
        "an adopted exit is reported"
    );
    assert_eq!(result.lease_id, "lease-r2");
    assert_eq!(
        h.life.held()[1],
        HeldRun {
            run_id: "r1".to_owned(),
            state: HeldState::Running
        }
    );
}

/// Asserts that an adopted job's wall limit counts the time it ran before the restart:
/// a container that has run for all but a second of its minute is stopped a second
/// later, as `timed_out`. It catches a restart that gives every job a fresh limit.
#[test]
fn an_adopted_job_keeps_the_time_it_has_already_used() {
    let mut h = Harness::new("adopt-wall");
    h.fake.leak(&labels("old", ""), 59_000);
    h.restart().unwrap();
    h.clock.advance(999);
    h.life.step();
    assert!(h.life.results().is_empty());
    h.clock.advance(1);
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::TimedOut);
    assert_eq!(h.life.results()[0].lease_id, "lease-old");
}

/// `base` with the label `key` set to `value`.
fn set<'a>(
    base: &[(&'static str, &'a str)],
    key: &str,
    value: &'a str,
) -> Vec<(&'static str, &'a str)> {
    base.iter()
        .map(|(k, v)| if *k == key { (*k, value) } else { (*k, *v) })
        .collect()
}

/// Asserts that a labelled container the agent cannot account for is held by no run,
/// recorded as a finding, and stopped and removed by its own id, for each way its labels
/// can be wrong: a label missing, a run id the wire would refuse (it names a path), a
/// lease id the wire would refuse, a number that is not one, a GPU list that is not one
/// or names a GPU twice, and a run id a container already adopted carries. It catches an
/// adoption that invents bookings, a run id from a label reaching a path, and a container
/// left running with nothing booked for it.
#[test]
fn containers_the_agent_cannot_account_for_are_stopped_removed_and_findings() {
    let mut h = Harness::new("unadoptable");
    let good = labels("good", "0,1");
    let kept = h.fake.leak(&good, 0);
    let without = |key: &str| -> Vec<(&str, &str)> {
        good.iter().copied().filter(|(k, _)| *k != key).collect()
    };
    let with = |key: &str, value: &'static str| set(&good, key, value);
    for broken in [
        without(label::RUN),
        without(label::LEASE),
        without(label::CPU_MILLIS),
        without(label::MEMORY_BYTES),
        without(label::GPUS),
        without(label::WALL_MS),
        with(label::RUN, "../../etc"),
        with(label::LEASE, "a b"),
        with(label::CPU_MILLIS, "lots"),
        with(label::MEMORY_BYTES, "-1"),
        with(label::GPUS, "0,x"),
        with(label::GPUS, "1,1"),
        with(label::WALL_MS, ""),
        with(label::RUN, "good"),
    ] {
        h.fake.leak(&broken, 0);
    }
    let events = names(&h.restart().unwrap());
    assert_eq!(events[0], "adopted good");
    assert_eq!(events.len(), 15);
    assert!(
        events[1..]
            .iter()
            .all(|event| event == "finding - unadoptable-container")
    );
    assert_eq!(h.held(), ["good"]);
    assert_eq!(h.fake.existing(), [kept], "only the adopted one is left");
    assert_eq!(h.fake.stops().len(), 14, "each was stopped, by its own id");
    assert!(h.fake.stops().iter().all(|(id, _)| *id != "leaked-1"));
}

/// Asserts that an unadoptable container that cannot be removed is a different finding,
/// `unadoptable-container-kept`, so that an operator can tell a container still running
/// with nothing booked for it from one that was cleared away; and that a stop that fails
/// does not stop the removal, which takes a running container too. It catches a failed
/// removal reported as done.
#[test]
fn an_unadoptable_container_that_cannot_be_removed_is_a_louder_finding() {
    let mut h = Harness::new("unadoptable-kept");
    let broken = set(&labels("odd", ""), label::CPU_MILLIS, "lots");
    let stuck = h.fake.leak(&broken, 0);
    h.fake.fail("remove", 1);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted - cleanup-failed",
            "finding - unadoptable-container-kept"
        ]
    );
    assert_eq!(h.fake.existing(), [stuck], "it is still there");
    assert!(h.held().is_empty());

    h.fake.fail("stop", 1);
    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["finding - unadoptable-container"]);
    assert!(
        h.fake.existing().is_empty(),
        "removal ran though the stop failed"
    );
}

/// Asserts that a container the agent cannot remove is booked while it stays: a container
/// whose labels say what it booked holds that, one whose labels do not hold the whole
/// offer, a start that wants the room is refused, a running job taken over beside it is
/// kept, and each step stops and removes it again until the removal succeeds, which frees
/// the booking and records that it is gone. It catches a possibly running container left
/// with nothing booked and nothing retried.
#[test]
fn a_container_that_cannot_be_removed_stays_booked_and_is_retried() {
    let mut h = Harness::new("evict-retry");
    let odd = set(&labels("odd", ""), label::CPU_MILLIS, "lots");
    let stuck = h.fake.leak(&odd, 0);
    h.fake.leak(&labels("good", "0"), 0);
    h.fake.fail("remove", 2);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted - cleanup-failed",
            "finding - unadoptable-container-kept",
            "adopted good"
        ]
    );
    assert_eq!(
        h.held(),
        ["good"],
        "a stuck container evicts no running job"
    );
    assert_eq!(
        h.life.free().cpu_millis,
        0,
        "unknown booking: the whole offer"
    );
    assert!(h.life.free().gpus.is_empty());
    assert_eq!(h.life.free().memory_bytes, 0);
    assert_eq!(
        h.life.free().disk_bytes,
        0,
        "unknown booking: all the disk too"
    );
    assert!(
        matches!(
            h.start(job("late")).as_slice(),
            [js_agent::lifecycle::Event::Refused {
                code: "host-tainted",
                ..
            }]
        ),
        "the container left on the host tainted it, which is refused before capacity"
    );
    assert_eq!(h.fake.stops().len(), 1);

    assert!(
        h.life.step().is_empty(),
        "a failed retry is not reported again"
    );
    assert_eq!(h.fake.stops().len(), 2, "the stop is tried again");
    assert_eq!(h.life.free().cpu_millis, 0);
    assert!(h.fake.existing().contains(&stuck));

    let events = names(&h.life.step());
    assert_eq!(events, ["finding - unadoptable-container"]);
    assert_eq!(h.fake.stops().len(), 3);
    assert!(!h.fake.existing().contains(&stuck));
    assert_eq!(
        h.life.free().cpu_millis,
        3000,
        "only good's booking is left"
    );
    assert!(h.life.step().is_empty(), "and nothing is retried after");
    assert_eq!(h.fake.stops().len(), 3);
}

/// Asserts that a container that cannot be removed holds what its labels say it booked
/// when they do say, not the whole offer: here its lease label is wrong and the rest is
/// fine. It catches a booking that is more conservative than the evidence.
#[test]
fn a_container_that_cannot_be_removed_holds_what_its_labels_booked() {
    let mut h = Harness::new("evict-known");
    let odd = set(&labels("odd", "1"), label::LEASE, "a b");
    h.fake.leak(&odd, 0);
    h.fake.fail("remove", 1);
    h.restart().unwrap();
    let free = h.life.free();
    assert_eq!(free.cpu_millis, 3000);
    assert_eq!(free.memory_bytes, (8 << 30) - (1 << 30));
    assert_eq!(free.gpus.into_iter().collect::<Vec<_>>(), [0]);
    assert!(h.held().is_empty(), "it is not a held run");
}

/// Asserts that a restart adopts the oldest containers first, and those that have exited
/// (whose results are held) before those that run, whatever order the runtime lists them
/// in, so that an offer lowered since the last start evicts the newest jobs that no longer
/// fit. Here the runtime lists the newest first; the offer holds two of four. It catches
/// an adoption in listing order, which evicts the longest running job while keeping a
/// newer one.
#[test]
fn adoption_keeps_exited_then_oldest_and_evicts_the_newest() {
    let mut h = Harness::new("adopt-order");
    h.config.capacity.cpu_millis = 2000;
    let [new, mid, old, done] = [
        h.fake.leak(&labels("new", ""), 1_000),
        h.fake.leak(&labels("mid", ""), 5_000),
        h.fake.leak(&labels("old", ""), 9_000),
        h.fake.leak(&labels("done", ""), 100),
    ];
    h.fake.exit("done", 0);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "adopted done",
            "adopted old",
            "finding - unadoptable-container",
            "finding - unadoptable-container"
        ]
    );
    assert_eq!(h.held(), ["done", "old"]);
    let stopped: Vec<String> = h.fake.stops().into_iter().map(|(id, _)| id).collect();
    assert_eq!(stopped, [mid.clone(), new.clone()], "the newest go");
    assert_eq!(h.fake.existing(), [old, done]);
    h.life.step();
    assert_eq!(h.life.results()[0].run_id, "done", "its result is kept");
}

/// Asserts that a lone GPU list that names a device twice is not adopted, though both
/// entries fit the offer: nothing else is adopted before it to make the second entry look
/// booked. It catches a booking of one device twice that only the capacity check refuses
/// when another container happens to hold it.
#[test]
fn a_gpu_named_twice_is_not_a_booking() {
    let mut h = Harness::new("adopt-dup-gpu");
    h.fake.leak(&labels("twice", "1,1"), 0);
    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["finding - unadoptable-container"]);
    assert!(h.held().is_empty());
    assert!(h.fake.existing().is_empty());
}

/// Asserts that adopted bookings are checked against what the agent offers and against
/// each other, in the order of adoption (all of one age here, so the order the runtime
/// lists them): a second container on a GPU
/// that is already booked, one on a GPU the machine does not offer, one that books more
/// CPU than is left and one that books more memory than is left are each stopped and
/// removed with a finding, while one that books exactly what is left is adopted. It
/// catches an agent that over-commits the machine because a restart trusted its labels.
#[test]
fn adopted_bookings_are_checked_against_the_offer_and_each_other() {
    let mut h = Harness::new("adopt-capacity");
    let base = labels("a", "0");
    let fits = |run: &'static str, gpus: &'static str| {
        set(&set(&base, label::RUN, run), label::GPUS, gpus)
    };
    let a = h.fake.leak(&fits("a", "0"), 0);
    h.fake.leak(&fits("b", "0"), 0);
    h.fake.leak(&fits("c", "7"), 0);
    h.fake
        .leak(&set(&fits("d", ""), label::CPU_MILLIS, "3001"), 0);
    h.fake
        .leak(&set(&fits("e", ""), label::MEMORY_BYTES, "7516192769"), 0);
    let f = {
        let exact = set(&fits("f", "1"), label::CPU_MILLIS, "3000");
        h.fake
            .leak(&set(&exact, label::MEMORY_BYTES, "7516192768"), 0)
    };
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "adopted a",
            "finding - unadoptable-container",
            "finding - unadoptable-container",
            "finding - unadoptable-container",
            "finding - unadoptable-container",
            "adopted f"
        ]
    );
    assert_eq!(h.held(), ["a", "f"]);
    assert_eq!(h.fake.existing(), [a, f]);
    assert_eq!(h.life.free().cpu_millis, 0);
    assert!(h.life.free().gpus.is_empty());
}

/// Asserts that adoption stops at what a heartbeat can list: with two containers more
/// than the wire allows, the agent holds exactly the limit, and the two beyond it are
/// stopped and removed with a finding each. It catches a held list the server must reject.
#[test]
fn adoption_is_capped_at_what_a_heartbeat_can_list() {
    let mut h = Harness::new("adopt-cap");
    let nothing = set(
        &set(&labels("n", ""), label::CPU_MILLIS, "0"),
        label::MEMORY_BYTES,
        "0",
    );
    let ids: Vec<String> = (0..MAX_HELD + 2).map(|n| format!("n{n}")).collect();
    for id in &ids {
        h.fake.leak(&set(&nothing, label::RUN, id), 0);
    }
    let events = names(&h.restart().unwrap());
    assert_eq!(events.len(), MAX_HELD + 2);
    let findings = events.iter().filter(|e| e.starts_with("finding")).count();
    assert_eq!(findings, 2);
    assert_eq!(h.held().len(), MAX_HELD);
    assert_eq!(h.fake.existing().len(), MAX_HELD);
    assert_eq!(h.fake.stops().len(), 2);
}

/// Asserts that an agent that cannot list the containers does not start: it reports the
/// error and builds no lifecycle, so it cannot say hello with a held list it made up.
#[test]
fn an_agent_that_cannot_list_its_containers_does_not_start() {
    let mut h = Harness::new("adopt-list-fails");
    h.fake.fail("list", 1);
    assert_eq!(h.restart().err().unwrap().to_string(), "list failed");
    assert!(h.restart().is_ok(), "and once the runtime answers it does");
}
