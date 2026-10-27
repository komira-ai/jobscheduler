//! What happens after the container exits: the result goes up first, the cleanup runs in
//! one order, and the slot is free only when all of it has passed.

use std::fs;

use js_agent::lifecycle::{CHECK_DETAIL_MAX, CheckFinding, code};
use js_agent_wire::{HeldRun, HeldState, Outcome};

use super::{Harness, Options, cleanup, job, names};

/// Asserts that the result exists as soon as the container has exited, while the slot is
/// still booked because the container cannot be removed, and that the slot is freed only
/// by the step that finishes the cleanup. It catches an agent that holds the result back
/// until cleanup is done (a job whose container will not die would never be reported) and
/// one that frees the slot before the cleanup has finished.
#[test]
fn result_sent_before_cleanup_and_slot_freed_after() {
    let mut h = Harness::new("result-first");
    h.start(job("r1"));
    h.fake.fail("remove", 1);
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        ["result r1", "finding r1 container-remove-failed"],
        "the result is ready in the step that saw the exit, and the cleanup stopped there"
    );
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::Succeeded);
    assert_eq!(results[0].exit_code, Some(0));
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is still booked");
    assert_eq!(
        h.life.held(),
        [HeldRun {
            run_id: "r1".to_owned(),
            state: HeldState::ExitedUnacked
        }]
    );
    let events = names(&h.life.step());
    assert_eq!(events, cleanup("r1"), "the next step finishes it, in order");
    assert_eq!(h.life.free().cpu_millis, 4000, "now the slot is free");
    assert_eq!(
        h.life.results().len(),
        1,
        "and the result waits for its ack"
    );
}

/// Asserts the order of the cleanup from the point of view of each party: the container
/// is removed while the scratch directory is still there, the checks run when both the
/// container and the scratch directory are gone, and the slot is free only after the
/// checks. It catches any reordering of the four steps.
#[test]
fn cleanup_order_container_scratch_check_free() {
    let mut h = Harness::new("order");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert_eq!(
        *h.log.borrow(),
        [
            "create r1",
            "remove c1 (scratch present)",
            "check r1 (container gone, scratch gone)"
        ]
    );
    assert!(!h.scratch_of("r1").exists());
}

/// Asserts that no outcome leaves a scratch directory behind: a success, a failure, an
/// out-of-memory kill, a stop, a cancel, a wall-time kill and a container that never
/// started. It catches a cleanup that runs on the happy path only.
#[test]
fn scratch_removed_after_every_outcome() {
    let mut h = Harness::new("every-outcome");
    for run in ["ok", "bad", "oom", "stopped", "cancelled", "slow"] {
        let mut spec = job(run);
        spec.limits.cpu_millis = 500;
        h.start(spec);
        assert!(
            h.scratch_of(run).is_dir(),
            "{run} has scratch while it runs"
        );
    }
    h.fake.exit("ok", 0);
    h.fake.exit("bad", 3);
    h.fake.exit_oom("oom");
    h.stop("stopped");
    h.cancel("cancelled");
    h.life.step();
    h.clock.advance(60_000);
    h.life.step();
    h.fake.fail("create", 1);
    h.start(job("never"));
    h.life.step();
    for run in ["ok", "bad", "oom", "stopped", "cancelled", "slow", "never"] {
        assert!(!h.scratch_of(run).exists(), "{run} left its scratch behind");
        assert!(h.fake.gone(run), "{run} left its container behind");
    }
    let by_run = |run: &str| {
        h.life
            .results()
            .into_iter()
            .find(|result| result.run_id == run)
            .map(|result| (result.outcome, result.exit_code))
    };
    assert_eq!(by_run("ok"), Some((Outcome::Succeeded, Some(0))));
    assert_eq!(by_run("bad"), Some((Outcome::Failed, Some(3))));
    assert_eq!(by_run("oom"), Some((Outcome::OutOfMemory, Some(137))));
    assert_eq!(by_run("stopped"), Some((Outcome::Killed, None)));
    assert_eq!(by_run("cancelled"), None, "a cancel reports nothing");
    assert_eq!(by_run("slow"), Some((Outcome::TimedOut, None)));
    assert_eq!(by_run("never"), Some((Outcome::FailedToStart, None)));
    assert_eq!(h.life.free().cpu_millis, 4000, "every slot came back");
}

/// Asserts that a container that cannot be removed keeps the slot booked for as long as
/// it fails, records one finding however often the step is retried, blocks a start that
/// needs the booked CPUs, and frees the slot on the step after it works. It catches a
/// slot freed on a failed cleanup, and a finding list that grows with every retry.
#[test]
fn slot_not_freed_when_remove_fails() {
    let mut h = Harness::new("remove-fails");
    let mut big = job("r1");
    big.limits.cpu_millis = 4000;
    h.start(big);
    h.fake.fail("remove", u32::MAX);
    h.fake.exit("r1", 0);
    for _ in 0..3 {
        h.life.step();
    }
    assert_eq!(h.life.free().cpu_millis, 0);
    assert_eq!(h.life.findings().len(), 1, "one finding, not one per retry");
    assert_eq!(h.life.findings()[0].code, code::CONTAINER_REMOVE_FAILED);
    assert_eq!(h.life.findings()[0].run_id.as_deref(), Some("r1"));
    let events = names(&h.start(job("r2")));
    assert_eq!(events, ["refused r2 no-capacity"]);
    h.fake.fail("remove", 0);
    assert_eq!(names(&h.life.step()), cleanup("r1"));
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that a scratch directory that cannot be removed (here a file stands where the
/// directory was) keeps the slot booked and records a finding, and that the retry frees
/// the slot once it can. It catches a cleanup that ignores a scratch failure.
#[test]
fn slot_not_freed_when_scratch_removal_fails() {
    let mut h = Harness::new("scratch-fails");
    h.start(job("r1"));
    let scratch = h.scratch_of("r1");
    fs::remove_dir_all(&scratch).unwrap();
    fs::write(&scratch, "not a directory").unwrap();
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "container removed r1",
            "finding r1 scratch-failed"
        ]
    );
    assert_eq!(h.life.free().cpu_millis, 3000);
    assert!(h.fake.gone("r1"), "the container is not removed twice");
    fs::remove_file(&scratch).unwrap();
    assert_eq!(
        names(&h.life.step()),
        ["scratch removed r1", "checks passed r1", "freed r1"]
    );
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that a post-job check that finds something keeps the slot booked for good,
/// is not run again, and leaves the finding on the job. The slot is the next change's
/// to taint. It catches a check result that is dropped.
#[test]
fn a_check_finding_blocks_the_slot() {
    let mut h = Harness::with(
        "check-finding",
        Options {
            findings: vec![("r1", CheckFinding::residue("stray-process", "pid 41"))],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            vec![
                "result r1",
                "container removed r1",
                "scratch removed r1",
                "finding r1 stray-process",
                "result r2"
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
            cleanup("r2")
        ]
        .concat()
    );
    h.life.step();
    h.life.step();
    let checks: Vec<String> = h
        .log
        .borrow()
        .iter()
        .filter(|line| line.starts_with("check r1"))
        .cloned()
        .collect();
    assert_eq!(
        checks.len(),
        1,
        "a blocked slot is not re-checked: {checks:?}"
    );
    assert_eq!(h.life.findings()[0].message, "pid 41");
    assert_eq!(h.life.free().cpu_millis, 3000, "r1's CPU is still booked");
    assert_eq!(h.held(), ["r1", "r2"], "a blocked run stays listed");
}

/// Asserts that a check that could not run blocks the slot like one that found something,
/// under its own code and with a message that says the check did not run. It catches an
/// unrun check treated as a clean host.
#[test]
fn a_check_that_could_not_run_blocks_the_slot() {
    let mut h = Harness::with(
        "check-unrun",
        Options {
            findings: vec![("r1", CheckFinding::unchecked("ps-failed", "no ps"))],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "finding r1 ps-failed");
    let found = &h.life.findings()[0];
    assert_eq!(found.message, "the check could not run: no ps");
    assert_eq!(h.life.free().cpu_millis, 3000);
}

/// Asserts that a check finding keeps at most `CHECK_DETAIL_MAX` bytes of detail and cuts
/// at a character boundary, and keeps a short detail whole. It catches a check that can
/// put the output of a program into the agent's memory and log without bound.
#[test]
fn a_check_finding_detail_is_bounded() {
    let short = CheckFinding::residue("x", "short");
    assert_eq!((short.code(), short.detail()), ("x", "short"));
    let exact = "a".repeat(CHECK_DETAIL_MAX);
    assert_eq!(CheckFinding::unchecked("x", &exact).detail(), exact);
    let long = "a".repeat(CHECK_DETAIL_MAX + 1);
    assert_eq!(CheckFinding::residue("x", &long).detail(), exact);
    // A two-byte character that straddles the limit is dropped whole.
    let straddle = format!("{}é", "a".repeat(CHECK_DETAIL_MAX - 1));
    let cut = CheckFinding::residue("x", &straddle);
    assert_eq!(cut.detail(), "a".repeat(CHECK_DETAIL_MAX - 1));
    assert_eq!(CheckFinding::residue("x", "").detail(), "");
}

/// Asserts that the cap holds for a finding built as a struct literal, which skips the
/// constructors: the detail a check hands over is cut where the lifecycle reads it, both
/// in what `detail()` returns and in the finding the run keeps. It catches a check that
/// puts the output of a program into the agent's memory and log by naming the variant.
#[test]
fn a_check_finding_built_by_hand_is_bounded_where_it_is_read() {
    let long = "z".repeat(CHECK_DETAIL_MAX * 4);
    let residue = CheckFinding::Residue {
        code: "big-residue",
        detail: long.clone(),
    };
    let unchecked = CheckFinding::Unchecked {
        code: "big-unchecked",
        detail: long,
    };
    assert_eq!(residue.detail().len(), CHECK_DETAIL_MAX);
    assert_eq!(unchecked.detail().len(), CHECK_DETAIL_MAX);
    for (finding, expected) in [
        (residue, "z".repeat(CHECK_DETAIL_MAX)),
        (
            unchecked,
            format!("the check could not run: {}", "z".repeat(CHECK_DETAIL_MAX)),
        ),
    ] {
        let mut h = Harness::with(
            "check-literal",
            Options {
                findings: vec![("r1", finding)],
                ..Options::default()
            },
        );
        h.start(job("r1"));
        h.fake.exit("r1", 0);
        h.life.step();
        assert_eq!(h.life.findings()[0].message, expected);
    }
}

/// Asserts that a container that fails to start gives a `failed_to_start` result with
/// the fixed detail (not the runtime's text), records the runtime's text as a finding,
/// removes the scratch directory it made and frees the slot. It catches a start failure
/// that leaks a slot or scratch directory, and runtime text leaking into a result.
#[test]
fn a_failed_start_reports_cleans_and_frees() {
    let mut h = Harness::new("failed-start");
    h.fake.fail("create", 1);
    let events = names(&h.start(job("r1")));
    assert_eq!(events, ["finding r1 start-failed", "result r1"]);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.detail.as_deref(), Some("runtime-failed"));
    assert_eq!(h.life.findings()[0].message, "create failed");
    let events = names(&h.life.step());
    assert_eq!(
        events,
        ["scratch removed r1", "checks passed r1", "freed r1"]
    );
    assert!(!h.scratch_of("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that a scratch directory that cannot be made (a file stands where it would
/// go and cannot be removed as a directory) is a `failed_to_start` with the
/// `scratch-failed` detail, creates no container, and is retried by the cleanup. It
/// catches a start that goes on without a scratch directory.
#[test]
fn a_scratch_directory_that_cannot_be_made_fails_the_start() {
    let mut h = Harness::new("scratch-create");
    fs::write(h.scratch_of("r1"), "in the way").unwrap();
    let events = names(&h.start(job("r1")));
    assert_eq!(events, ["finding r1 scratch-failed", "result r1"]);
    assert_eq!(
        h.life.results()[0].detail.as_deref(),
        Some("scratch-failed")
    );
    assert!(h.log.borrow().is_empty(), "no container was created");
    assert_eq!(h.life.free().cpu_millis, 3000, "booked until cleaned");
    fs::remove_file(h.scratch_of("r1")).unwrap();
    assert_eq!(names(&h.life.step()).last().unwrap(), "freed r1");
}
