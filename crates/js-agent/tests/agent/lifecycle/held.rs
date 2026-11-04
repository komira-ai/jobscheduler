//! The results the server has not acknowledged and the ids that must not start again
//! survive a restart of the agent: they are files in the state root, written before a
//! heartbeat can carry them, removed when an acknowledgement drops them, and read back when
//! the next process takes over.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_agent_wire::limits::MAX_HELD;
use js_agent_wire::{BookedResources, Command, HeldRun, HeldState, JobStartSpec, LogTail, Outcome};
use serde_json::{Value, json};

use super::{Harness, job, names, reply_json};

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Every verdict the agent knows.
const VERDICTS: [&str; 5] = [
    "ACCEPTED",
    "DUPLICATE",
    "STALE",
    "UNKNOWN_LEASE",
    "MISMATCH",
];

/// A valid job start for `job_id`: one CPU, 1 GiB, a minute, no GPU.
fn start(job_id: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: format!("alloc-{job_id}"),
        image: IMAGE.to_owned(),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
    }
}

fn job_start(job_id: &str) -> Command {
    Command::JobStart(Box::new(start(job_id)))
}

/// The directory of the held results.
fn held_dir(h: &Harness) -> PathBuf {
    h.dir.path("state").join("held")
}

/// The file of the held result of `id`.
fn held_file(h: &Harness, id: &str) -> PathBuf {
    held_dir(h).join(format!("{id}.json"))
}

/// The retired list's file.
fn retired_file(h: &Harness) -> PathBuf {
    h.dir.path("state").join("retired.json")
}

/// The JSON in `path`.
fn json_of(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A harness with the run `r1` (exit 7) and the job `j1` (exit 0) ended and stepped: both
/// results are held and written down.
fn ended(name: &str) -> Harness {
    let mut h = Harness::new(name);
    h.start(job("r1"));
    h.apply(vec![job_start("j1")]);
    h.fake.exit("r1", 7);
    h.fake.exit("j1", 0);
    h.life.step();
    h
}

/// A reply that gives `verdict` to the results of `r1` and `j1`.
fn ack_both(h: &mut Harness, verdict: &str) -> Vec<String> {
    let reply = reply_json(&json!({
        "result_acks": [{ "run_id": "r1", "ack": verdict }],
        "job_acks": [{ "job_id": "j1", "ack": verdict }],
    }));
    names(&h.life.apply(&reply))
}

/// The findings of `events` with `code`.
fn findings(events: &[String], code: &str) -> usize {
    let wanted = format!(" {code}");
    let found = events
        .iter()
        .filter(|e| e.starts_with("finding") && e.ends_with(&wanted));
    found.count()
}

/// Asserts that a restart lists again the results the earlier process held, a run's and a
/// job's, byte for byte as they were serialised before, with the log tail, in the same
/// listings, and that the files are private (the directory `0700`, a file `0600`). It
/// catches a result lost with the process that made it, which the server then never
/// hears of.
#[test]
fn results_not_yet_acknowledged_are_resent_after_restart() {
    let mut h = Harness::new("held-resend");
    h.start(job("r1"));
    h.apply(vec![job_start("j1")]);
    let tail = LogTail {
        stdout: "out\n".to_owned(),
        stderr: "err\n".to_owned(),
        ..LogTail::default()
    };
    h.fake.print("r1", tail.clone());
    h.fake.print("j1", tail);
    h.fake.exit("r1", 7);
    h.fake.exit("j1", 0);
    h.life.step();
    let sent = (
        serde_json::to_string(&h.life.results()).unwrap(),
        serde_json::to_string(&h.life.held_jobs()).unwrap(),
    );
    assert!(sent.0.contains("out\\n"), "the tail is in the result");
    let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(held_dir(&h)), 0o700);
    assert_eq!(mode(held_file(&h, "r1")), 0o600);

    let events = h.restart().unwrap();
    assert!(names(&events).is_empty(), "{:?}", names(&events));
    let resent = (
        serde_json::to_string(&h.life.results()).unwrap(),
        serde_json::to_string(&h.life.held_jobs()).unwrap(),
    );
    assert_eq!(resent, sent);
    assert_eq!(
        h.life.held(),
        [HeldRun {
            run_id: "r1".to_owned(),
            state: HeldState::ExitedUnacked,
        }]
    );
    assert_eq!(h.life.results()[0].outcome, Outcome::Failed);
    assert_eq!(h.life.results()[0].exit_code, Some(7));
    h.life.step();
    assert_eq!(
        serde_json::to_string(&h.life.results()).unwrap(),
        sent.0,
        "a step changes nothing"
    );
    assert_eq!(h.fake.created().len(), 2, "nothing runs again");
}

/// Asserts that each verdict the agent knows, given to a run's result and a job's, removes
/// their files and ends the wait, so that a further restart sends nothing; that a verdict
/// it does not know keeps the files and the result is sent again; and that a start for the
/// run or the job after the restart is skipped as retired and creates no container. It
/// catches a result that comes back after the server took it, one that is lost on a verdict
/// that keeps it, and a retired id forgotten with the process.
#[test]
fn every_verdict_removes_the_files_and_a_further_restart_sends_nothing() {
    for (n, verdict) in VERDICTS.into_iter().enumerate() {
        let mut h = ended(&format!("held-ack-{n}"));
        h.restart().unwrap();
        assert!(held_file(&h, "r1").exists() && held_file(&h, "j1").exists());
        ack_both(&mut h, verdict);
        assert!(!held_file(&h, "r1").exists(), "{verdict}");
        assert!(!held_file(&h, "j1").exists(), "{verdict}");

        h.restart().unwrap();
        assert!(h.life.results().is_empty(), "{verdict}");
        assert!(h.life.held_jobs().is_empty(), "{verdict}");
        assert!(h.held().is_empty(), "{verdict}");
        assert_eq!(names(&h.start(job("r1"))), ["skipped r1 Retired"]);
        assert_eq!(
            names(&h.apply(vec![job_start("j1")])),
            ["skipped j1 FenceRetired"]
        );
        assert_eq!(h.fake.created().len(), 2, "{verdict}: nothing runs again");
    }

    let mut h = ended("held-ack-unknown");
    ack_both(&mut h, "A_VERDICT_FROM_THE_FUTURE");
    assert!(held_file(&h, "r1").exists() && held_file(&h, "j1").exists());
    h.restart().unwrap();
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.held_jobs().len(), 1);
}

/// Asserts the retired list's file: the ids in order, a finished run or job with its lease
/// or allocation and one retired by a stop of work the agent did not hold without, written
/// whole as the ids come; and that a start for each is skipped after a restart. It catches
/// a list that is not written, one that loses the order, and a stop that does not retire
/// across a restart.
#[test]
fn the_retired_list_is_written_with_the_allocations_and_read_back() {
    let mut h = Harness::new("held-retired");
    assert!(!retired_file(&h).exists(), "nothing retired, no file");
    h.stop("r9");
    h.apply(vec![Command::JobStop {
        job_id: "j9".to_owned(),
    }]);
    assert_eq!(
        json_of(&retired_file(&h)),
        json!({ "retired": [{ "job_id": "r9" }, { "job_id": "j9" }] })
    );
    let mut h = ended("held-retired-ended");
    ack_both(&mut h, "ACCEPTED");
    h.stop("r9");
    assert_eq!(
        json_of(&retired_file(&h)),
        json!({ "retired": [
            { "job_id": "r1", "allocation_id": "lease-r1" },
            { "job_id": "j1", "allocation_id": "alloc-j1" },
            { "job_id": "r9" },
        ] })
    );
    h.restart().unwrap();
    for id in ["r1", "j1", "r9"] {
        assert_eq!(names(&h.start(job(id))), [format!("skipped {id} Retired")]);
    }
    assert_eq!(h.fake.created().len(), 2);
    h.stop("r8");
    let list = json_of(&retired_file(&h));
    assert_eq!(list["retired"].as_array().unwrap().len(), 4, "{list}");
}

/// Asserts that files in the held directory that cannot be used are removed and each is a
/// finding, and the agent starts and holds the good one beside them: text that is not a
/// record, a result whose ids do not validate, a record in a file named for another run, and
/// a directory (which cannot be removed and is reported). It catches an agent that refuses
/// to start over a bad file, one that holds a result nobody could send, and a bad file kept
/// to fail every start.
#[test]
fn unusable_result_files_are_dropped_with_findings_and_the_agent_starts() {
    let mut h = Harness::new("held-unusable");
    let dir = held_dir(&h);
    fs::create_dir(&dir).unwrap();
    let good = |id: &str, lease: &str| {
        json!({ "keyed": "run", "result": {
            "run_id": id, "lease_id": lease, "outcome": "killed" } })
        .to_string()
    };
    fs::write(dir.join("junk.json"), "not json").unwrap();
    fs::write(dir.join("bad.json"), good("bad", "has space")).unwrap();
    fs::write(dir.join("other.json"), good("someone", "l")).unwrap();
    fs::create_dir(dir.join("dir.json")).unwrap();
    fs::write(dir.join("good.json"), good("good", "l-good")).unwrap();

    let events = names(&h.restart().unwrap());
    assert_eq!(findings(&events, "held-record-unreadable"), 4, "{events:?}");
    assert_eq!(h.held(), ["good"]);
    assert_eq!(h.life.results()[0].lease_id, "l-good");
    for gone in ["junk", "bad", "other"] {
        assert!(!dir.join(format!("{gone}.json")).exists(), "{gone}");
    }
    assert!(dir.join("good.json").exists());
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
}

/// Asserts that a held directory that cannot be listed (a file stands where it should be)
/// is one finding and the agent starts all the same. It catches a start that is refused
/// over a state root it cannot read results from.
#[test]
fn a_held_directory_that_cannot_be_listed_is_a_finding_and_the_agent_starts() {
    let mut h = Harness::new("held-unlisted");
    fs::write(held_dir(&h), "not a directory").unwrap();
    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["finding - held-record-unreadable"]);
    assert!(h.held().is_empty());
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
}

/// Asserts that a retired list that is not a list (text, or a directory) starts the agent
/// with none retired and one finding, that a start of any id then runs, and that the next
/// id retired replaces the file. It catches a start refused over a bad list, and a bad list kept for good.
#[test]
fn an_unreadable_retired_list_is_a_finding_and_starts_empty() {
    let mut h = Harness::new("held-retired-bad");
    fs::create_dir(retired_file(&h)).unwrap();
    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["finding - held-record-unreadable"], "a directory");
    fs::remove_dir(retired_file(&h)).unwrap();
    fs::write(retired_file(&h), "not json").unwrap();
    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["finding - held-record-unreadable"]);
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    h.stop("r5");
    assert_eq!(
        json_of(&retired_file(&h)),
        json!({ "retired": [{ "job_id": "r5" }] })
    );
    assert_eq!(names(&h.restart().unwrap()), ["adopted r1"]);
    assert_eq!(names(&h.start(job("r5"))), ["skipped r5 Retired"]);
}

/// Asserts that a result that cannot be written is still held in memory and sent, is one
/// finding naming the run however many calls fail again, is written by the first call that
/// can, and that a failure after that is reported again. It catches a result dropped with
/// its file, a finding at every call, and a write that is never tried again.
#[test]
fn a_result_that_cannot_be_written_stays_held_and_is_written_later() {
    let mut h = Harness::new("held-write-fails");
    fs::write(held_dir(&h), "in the way").unwrap();
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(findings(&events, "held-record-failed"), 1, "{events:?}");
    assert!(events.contains(&"finding r1 held-record-failed".to_owned()));
    assert_eq!(h.life.results().len(), 1, "still held");
    let events = names(&h.life.step());
    assert_eq!(findings(&events, "held-record-failed"), 0, "{events:?}");

    fs::remove_file(held_dir(&h)).unwrap();
    assert!(h.life.step().is_empty());
    assert!(held_file(&h, "r1").exists());

    fs::create_dir_all(held_dir(&h).join("r2.json.tmp")).unwrap();
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"finding r2 held-record-failed".to_owned()),
        "{events:?}"
    );
    assert!(!held_file(&h, "r2").exists());
    fs::remove_dir(held_dir(&h).join("r2.json.tmp")).unwrap();
    h.life.step();
    assert!(held_file(&h, "r2").exists());
}

/// Asserts that a file that cannot be removed when its result is dropped is a finding naming
/// the run and is tried again, that a file that is gone already is not a failure, and that
/// the result is gone from the lifecycle either way. It catches a file left to bring back a
/// result the server took, and a removal that fails for a file that is not there.
#[test]
fn a_file_that_cannot_be_removed_is_reported_and_tried_again() {
    let mut h = ended("held-remove-fails");
    let file = held_file(&h, "r1");
    fs::remove_file(&file).unwrap();
    fs::create_dir_all(file.join("in-the-way")).unwrap();
    let events = ack_both(&mut h, "ACCEPTED");
    assert!(
        events.contains(&"finding r1 held-record-failed".to_owned()),
        "{events:?}"
    );
    assert!(h.life.results().is_empty());
    assert!(file.exists(), "it could not be removed");
    assert!(!held_file(&h, "j1").exists(), "the other one went");
    assert!(h.life.step().is_empty(), "told once");

    fs::remove_dir_all(&file).unwrap();
    assert!(h.life.step().is_empty());
    h.restart().unwrap();
    assert!(h.life.results().is_empty());

    let mut h = ended("held-remove-gone");
    fs::remove_file(held_file(&h, "r1")).unwrap();
    let events = ack_both(&mut h, "ACCEPTED");
    assert!(events.is_empty(), "{events:?}");
}

/// Asserts that a retired list that cannot be written is a finding without a run and is
/// written by the next call that can. It catches a list that is lost with one failed write.
#[test]
fn a_retired_list_that_cannot_be_written_is_reported_and_tried_again() {
    let mut h = Harness::new("held-retired-fails");
    fs::create_dir(h.dir.path("state").join("retired.json.tmp")).unwrap();
    let events = h.stop("r9");
    assert_eq!(names(&events), ["finding - held-record-failed"]);
    assert!(!retired_file(&h).exists());
    fs::remove_dir(h.dir.path("state").join("retired.json.tmp")).unwrap();
    assert!(h.life.step().is_empty());
    assert_eq!(
        json_of(&retired_file(&h)),
        json!({ "retired": [{ "job_id": "r9" }] })
    );
}

/// Asserts that a taken-over job whose result was written down before the restart keeps
/// that result and makes no second one when its container is seen to have exited (no
/// `result` event, and the exit it reported stays whatever the container says now), and
/// then goes through its cleanup as usual. It catches a restart that reports a job's end
/// twice, or the second reading in place of the first.
#[test]
fn an_adopted_job_with_a_stored_result_does_not_make_a_second_one() {
    let mut h = Harness::new("held-adopted");
    h.start(job("r1"));
    h.fake.exit("r1", 7);
    h.fake.fail("remove", u32::MAX);
    h.life.step();
    assert_eq!(h.life.results()[0].exit_code, Some(7));
    assert!(held_file(&h, "r1").exists());

    assert_eq!(names(&h.restart().unwrap()).last().unwrap(), "adopted r1");
    h.fake.exit("r1", 9);
    h.fake.fail("remove", 0);
    let events = names(&h.life.step());
    assert!(
        !events.iter().any(|e| e.starts_with("result")),
        "{events:?}"
    );
    assert!(events.contains(&"container removed r1".to_owned()));
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.results()[0].exit_code, Some(7));
    h.ack("r1", "ACCEPTED");
    assert!(h.life.results().is_empty());
    assert!(!held_file(&h, "r1").exists());
}

/// Asserts that results whose log tails were cut to fit a heartbeat after they were first
/// written are written again, so that a restart lists the cut results and not the long ones.
/// It catches a stored result that goes stale when its tail is cut.
#[test]
fn a_result_is_written_again_when_its_log_tail_is_cut() {
    let mut h = Harness::new("held-cut");
    let long = LogTail {
        stdout: "x".repeat(2000),
        ..LogTail::default()
    };
    for run in ["r1", "r2"] {
        h.start(job(run));
        h.fake.print(run, long.clone());
        h.fake.exit(run, 0);
    }
    h.life.step();
    h.life.fit_log_tails(1500, &mut Vec::new());
    let cut = h.life.results();
    assert!(cut.iter().all(|r| r.log_tail.as_ref().unwrap().truncated));
    h.life.step();
    h.restart().unwrap();
    assert_eq!(h.life.results(), cut);
}

/// Asserts that no more than the agent can list are read back: of one file more than a
/// heartbeat holds, the last by name is removed with a finding and the rest are held. It
/// catches a directory that grows the held list past the wire's bound.
#[test]
fn more_results_than_a_heartbeat_can_list_are_not_all_read_back() {
    let mut h = Harness::new("held-cap");
    let dir = held_dir(&h);
    fs::create_dir(&dir).unwrap();
    for n in 0..=MAX_HELD {
        let id = format!("x{n:04}");
        let body = json!({ "keyed": "run", "result": {
            "run_id": id, "lease_id": "l", "outcome": "killed" } });
        fs::write(dir.join(format!("{id}.json")), body.to_string()).unwrap();
    }
    let last = dir.join(format!("x{MAX_HELD:04}.json"));
    let events = names(&h.restart().unwrap());
    assert_eq!(findings(&events, "held-record-unreadable"), 1, "{events:?}");
    assert_eq!(h.held().len(), MAX_HELD);
    assert!(!last.exists());
}

/// Asserts that a result stored for a run that is not a run the agent holds is held by a
/// job keyed as the file says, listed in the listing of its keying alone. It catches a
/// restored job listed under the wrong keying.
#[test]
fn a_restored_result_is_listed_under_its_own_keying() {
    let mut h = ended("held-keying");
    h.restart().unwrap();
    assert_eq!(h.held(), ["r1"]);
    let jobs = h.life.held_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_id, "j1");
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].run_id, "r1");
}

/// Asserts that a result file whose id is retired in the same call is removed only after
/// the retired list is written: with the list unwritable, both files stay, the failure is
/// one finding with no run, and the call that can write the list then removes them. It
/// catches a crash window in which the id is neither held nor retired, so that a replayed
/// start would run it again.
#[test]
fn a_result_file_stays_until_the_retired_list_is_written() {
    let mut h = ended("held-retire-first");
    let temp = h.dir.path("state").join("retired.json.tmp");
    fs::create_dir(&temp).unwrap();
    let events = ack_both(&mut h, "ACCEPTED");
    assert!(
        events.contains(&"finding - held-record-failed".to_owned()),
        "{events:?}"
    );
    assert_eq!(findings(&events, "held-record-failed"), 1, "{events:?}");
    assert!(h.life.results().is_empty());
    assert!(held_file(&h, "r1").exists() && held_file(&h, "j1").exists());
    assert!(!retired_file(&h).exists());
    assert!(h.life.step().is_empty(), "told once");
    assert!(held_file(&h, "r1").exists());

    fs::remove_dir(&temp).unwrap();
    assert!(h.life.step().is_empty());
    assert!(!held_file(&h, "r1").exists() && !held_file(&h, "j1").exists());
    assert_eq!(
        json_of(&retired_file(&h))["retired"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// Asserts that the unfinished write of a dead process (`<id>.json.tmp`) is removed at
/// start without a finding, beside a good result that is held. It catches a leftover that
/// is reported as an unreadable result at every start.
#[test]
fn an_unfinished_write_is_removed_without_a_finding() {
    let mut h = ended("held-tmp");
    let leftover = held_dir(&h).join("r1.json.tmp");
    fs::write(&leftover, "{\"keyed\": ").unwrap();
    let events = names(&h.restart().unwrap());
    assert_eq!(findings(&events, "held-record-unreadable"), 0, "{events:?}");
    assert!(!leftover.exists());
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.held_jobs().len(), 1);
}

/// Asserts that a file for a job the agent has taken over does not count against the room
/// for held results: with the taken-over job's file and one more file than the rest of the
/// room, exactly the one too many is removed and the taken-over job keeps its result. It
/// catches a cap that counts a job twice and so drops a result it had room for.
#[test]
fn a_taken_over_jobs_file_does_not_use_up_room() {
    let mut h = Harness::new("held-cap-adopted");
    h.start(job("r1"));
    h.fake.exit("r1", 7);
    h.fake.fail("remove", u32::MAX);
    h.life.step();
    let dir = held_dir(&h);
    for n in 0..MAX_HELD {
        let id = format!("x{n:04}");
        let body = json!({ "keyed": "run", "result": {
            "run_id": id, "lease_id": "l", "outcome": "killed" } });
        fs::write(dir.join(format!("{id}.json")), body.to_string()).unwrap();
    }
    let events = names(&h.restart().unwrap());
    assert_eq!(findings(&events, "held-record-unreadable"), 1, "{events:?}");
    assert_eq!(h.held().len(), MAX_HELD);
    assert!(h.held().contains(&"r1".to_owned()));
    assert!(dir.join("r1.json").exists());
    assert!(!dir.join(format!("x{:04}.json", MAX_HELD - 1)).exists());
}
