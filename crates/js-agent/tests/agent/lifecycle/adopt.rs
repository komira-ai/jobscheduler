//! A restart: the containers a previous process labelled are taken over, listed first,
//! and a start for one of them is not a new run.

use js_agent::lifecycle::label;
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
        (label::GPUS, gpus),
        (label::WALL_MS, "60000"),
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
    assert_eq!(names(&events), ["adopted r1", "adopted r2"]);
    assert_eq!(h.fake.created().len(), 2, "adopting creates nothing");
    assert_eq!(h.held(), ["r1", "r2"], "held before any heartbeat");
    assert_eq!(h.life.free().cpu_millis, 2000);
    assert_eq!(h.life.free().gpus.into_iter().collect::<Vec<_>>(), [0]);

    let resent = h.start(gpu);
    assert!(resent.is_empty());
    assert_eq!(h.fake.created().len(), 2, "the work did not run twice");
    h.start(job("r3"));
    assert_eq!(h.held(), ["r1", "r2", "r3"], "adopted runs come first");

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
        h.life.held()[0],
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

/// Asserts that a labelled container the agent cannot account for is left alone, held
/// by no run and recorded as a finding, for each way its labels can be wrong: a label
/// missing, a run id the wire would refuse (it names a path), a lease id the wire would
/// refuse, a number that is not one, a GPU list that is not one, and a run id a
/// container already adopted carries. It catches an adoption that invents bookings, and
/// a run id from a label reaching a path.
#[test]
fn containers_the_agent_cannot_account_for_are_findings() {
    let mut h = Harness::new("unadoptable");
    let good = labels("good", "0,1");
    h.fake.leak(&good, 0);
    let without = |key: &str| -> Vec<(&str, &str)> {
        good.iter().copied().filter(|(k, _)| *k != key).collect()
    };
    let with = |key: &str, value: &'static str| -> Vec<(&str, &str)> {
        good.iter()
            .map(|(k, v)| if *k == key { (*k, value) } else { (*k, *v) })
            .collect()
    };
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
        with(label::WALL_MS, ""),
        with(label::RUN, "good"),
    ] {
        h.fake.leak(&broken, 0);
    }
    let events = names(&h.restart().unwrap());
    assert_eq!(events[0], "adopted good");
    assert_eq!(events.len(), 14);
    assert!(
        events[1..]
            .iter()
            .all(|event| event == "finding - unadoptable-container")
    );
    assert_eq!(h.held(), ["good"]);
    assert_eq!(h.fake.existing().len(), 14, "none of them was touched");
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
