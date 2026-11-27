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
const VERDICTS: [&str; 4] = ["ACCEPTED", "DUPLICATE", "STALE", "MISMATCH"];

/// Verdicts the agent does not know: one from a later release and the text an older server
/// gave for a lease it did not hold, which this release no longer reads.
const UNKNOWN_VERDICTS: [&str; 2] = ["A_VERDICT_FROM_THE_FUTURE", "UNKNOWN_LEASE"];

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
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
        fuse: false,
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: None,
        xcode: None,
        sizing: None,
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

/// A harness with the jobs `r1` (exit 7) and `j1` (exit 0) ended and stepped, `r1` started
/// by [`Harness::start`] and `j1` by a job start: both results are held and written down.
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
        "job_acks": [
            { "job_id": "r1", "ack": verdict },
            { "job_id": "j1", "ack": verdict },
        ],
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

/// Asserts that a restart lists again the results the earlier process held, of a job that
/// [`Harness::start`] made and one a job start made, byte for byte as they were serialised
/// before, with the log tail, and that the files are private (the directory `0700`, a file
/// `0600`). It catches a result lost with the process that made it, which the server then
/// never hears of.
#[test]
fn results_not_yet_acknowledged_are_resent_after_restart() {
    let mut h = Harness::new("held-resend");
    h.apply(vec![job_start("j1")]);
    h.start(job("r1"));
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
        serde_json::to_string(&super::results(&h.life)).unwrap(),
        serde_json::to_string(&h.life.held_jobs()).unwrap(),
    );
    assert!(sent.0.contains("out\\n"), "the tail is in the result");
    let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(held_dir(&h)), 0o700);
    assert_eq!(mode(held_file(&h, "r1")), 0o600);

    let events = h.restart().unwrap();
    assert!(names(&events).is_empty(), "{:?}", names(&events));
    let resent = (
        serde_json::to_string(&super::results(&h.life)).unwrap(),
        serde_json::to_string(&h.life.held_jobs()).unwrap(),
    );
    assert_eq!(resent, sent);
    assert_eq!(
        super::held(&h.life),
        [
            HeldRun {
                run_id: "j1".to_owned(),
                state: HeldState::ExitedUnacked,
            },
            HeldRun {
                run_id: "r1".to_owned(),
                state: HeldState::ExitedUnacked,
            },
        ]
    );
    assert_eq!(super::results(&h.life)[1].outcome, Outcome::Failed);
    assert_eq!(super::results(&h.life)[1].exit_code, Some(7));
    h.life.step();
    assert_eq!(
        serde_json::to_string(&super::results(&h.life)).unwrap(),
        sent.0,
        "a step changes nothing"
    );
    assert_eq!(h.fake.created().len(), 2, "nothing runs again");
}

/// Asserts that each verdict the agent knows, given to the results of two jobs, removes
/// their files and ends the wait, so that a further restart sends nothing; that a verdict
/// it does not know (one from a later release, and `UNKNOWN_LEASE`, which an older server
/// gave) keeps the files and the results are sent again; and that a start for either job
/// after the restart is skipped as retired and creates no container. It catches a result
/// that comes back after the server took it, one that is lost on a verdict that keeps it,
/// `UNKNOWN_LEASE` read as a verdict that drops a result, and a retired id forgotten with
/// the process.
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
        assert!(super::results(&h.life).is_empty(), "{verdict}");
        assert!(h.life.held_jobs().is_empty(), "{verdict}");
        assert!(h.held().is_empty(), "{verdict}");
        assert_eq!(names(&h.start(job("r1"))), ["skipped r1 FenceRetired"]);
        assert_eq!(
            names(&h.apply(vec![job_start("j1")])),
            ["skipped j1 FenceRetired"]
        );
        assert_eq!(h.fake.created().len(), 2, "{verdict}: nothing runs again");
    }

    for (n, verdict) in UNKNOWN_VERDICTS.into_iter().enumerate() {
        let mut h = ended(&format!("held-ack-unknown-{n}"));
        ack_both(&mut h, verdict);
        assert!(held_file(&h, "r1").exists() && held_file(&h, "j1").exists());
        h.restart().unwrap();
        assert_eq!(super::results(&h.life).len(), 2, "{verdict}");
        assert_eq!(h.life.held_jobs().len(), 2, "{verdict}");
    }
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
    // Each start names the fence it was retired with: `j1` its allocation, `r9` any.
    for (id, allocation) in [("r1", "lease-r1"), ("j1", "alloc-j1"), ("r9", "lease-r9")] {
        let mut spec = job(id);
        spec.lease_id = allocation.to_owned();
        assert_eq!(
            names(&h.start(spec)),
            [format!("skipped {id} FenceRetired")]
        );
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
        json!({ "keyed": "job", "result": {
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
    assert_eq!(super::results(&h.life)[0].lease_id, "l-good");
    for gone in ["junk", "bad", "other"] {
        assert!(!dir.join(format!("{gone}.json")).exists(), "{gone}");
    }
    assert!(dir.join("good.json").exists());
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
}

/// Asserts that a result file an older release left, whose keying is `run`, is removed at
/// start with one `held-record-unreadable` finding and is listed nowhere (not among the
/// held runs, the held jobs or the results), beside a job-keyed file that is held, and that
/// the id of the removed file is not retired, so that its job can start. It catches an agent
/// that lists a result of the run keying it no longer holds, which the server would then
/// read as work of a lease it does not know.
#[test]
fn a_result_file_keyed_run_by_an_older_release_is_removed_and_listed_nowhere() {
    let mut h = Harness::new("held-older-run");
    let dir = held_dir(&h);
    fs::create_dir(&dir).unwrap();
    let record = |keyed: &str, id: &str| {
        json!({ "keyed": keyed, "result": {
            "run_id": id, "lease_id": "l", "outcome": "killed" } })
        .to_string()
    };
    fs::write(dir.join("old.json"), record("run", "old")).unwrap();
    fs::write(dir.join("new.json"), record("job", "new")).unwrap();

    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["finding - held-record-unreadable"]);
    assert!(!dir.join("old.json").exists(), "the file is removed");
    assert!(dir.join("new.json").exists());
    assert_eq!(h.held(), ["new"]);
    assert_eq!(super::results(&h.life).len(), 1);
    let jobs: Vec<String> = h.life.held_jobs().into_iter().map(|j| j.job_id).collect();
    assert_eq!(jobs, ["new"]);
    assert_eq!(names(&h.start(job("old"))), ["started old"]);
}

/// Asserts that the file this release writes for a held result says `"keyed":"job"` and
/// holds the result with the job id as `run_id` and the allocation id as `lease_id`, and
/// nothing else, written byte for byte as the previous release reads it. It catches a
/// record whose keying the previous release cannot read after a rollback, and one whose
/// ids are moved to other fields.
#[test]
fn the_record_this_release_writes_says_job_and_holds_the_run_result() {
    let mut h = Harness::new("held-record-shape");
    h.start(job("r1"));
    h.fake.exit("r1", 7);
    h.life.step();
    assert_eq!(
        json_of(&held_file(&h, "r1")),
        json!({
            "keyed": "job",
            "result": {
                "run_id": "r1",
                "lease_id": "lease-r1",
                "outcome": "failed",
                "exit_code": 7
            }
        })
    );
    h.apply(vec![job_start("j1")]);
    h.fake.exit("j1", 0);
    h.life.step();
    assert_eq!(
        json_of(&held_file(&h, "j1")),
        json!({
            "keyed": "job",
            "result": {
                "run_id": "j1",
                "lease_id": "alloc-j1",
                "outcome": "succeeded",
                "exit_code": 0
            }
        })
    );
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
    assert_eq!(names(&h.start(job("r5"))), ["skipped r5 FenceRetired"]);
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
    assert_eq!(super::results(&h.life).len(), 1, "still held");
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
    assert!(super::results(&h.life).is_empty());
    assert!(file.exists(), "it could not be removed");
    assert!(!held_file(&h, "j1").exists(), "the other one went");
    assert!(h.life.step().is_empty(), "told once");

    fs::remove_dir_all(&file).unwrap();
    assert!(h.life.step().is_empty());
    h.restart().unwrap();
    assert!(super::results(&h.life).is_empty());

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
    assert_eq!(super::results(&h.life)[0].exit_code, Some(7));
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
    assert_eq!(super::results(&h.life).len(), 1);
    assert_eq!(super::results(&h.life)[0].exit_code, Some(7));
    h.ack("r1", "ACCEPTED");
    assert!(super::results(&h.life).is_empty());
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
    let cut = super::results(&h.life);
    assert!(cut.iter().all(|r| r.log_tail.as_ref().unwrap().truncated));
    h.life.step();
    h.restart().unwrap();
    assert_eq!(super::results(&h.life), cut);
}

/// The results `x0000`, `x0001`, ... read back before their entries reach what a heartbeat
/// lists: an entry at a five-byte id weighs 44 bytes, and 558 of them fit the 24,576 the
/// entries may take.
const LISTABLE: usize = 558;
const _: () = assert!(LISTABLE < MAX_HELD);

/// Asserts that no more than the agent can list are read back: of one file more than the
/// entries a heartbeat body has room for (far fewer than the wire's count cap,
/// [`MAX_HELD`]), the last by name is removed with a finding and the rest are held. It
/// catches a directory that grows the held list past the body the server reads, or that
/// leaves a heartbeat no room for a result, so that none is ever acknowledged.
#[test]
fn more_results_than_a_heartbeat_can_list_are_not_all_read_back() {
    let mut h = Harness::new("held-cap");
    let dir = held_dir(&h);
    fs::create_dir(&dir).unwrap();
    for n in 0..=LISTABLE {
        let id = format!("x{n:04}");
        let body = json!({ "keyed": "job", "result": {
            "run_id": id, "lease_id": "l", "outcome": "killed" } });
        fs::write(dir.join(format!("{id}.json")), body.to_string()).unwrap();
    }
    let last = dir.join(format!("x{LISTABLE:04}.json"));
    let events = names(&h.restart().unwrap());
    assert_eq!(findings(&events, "held-record-unreadable"), 1, "{events:?}");
    assert_eq!(h.held().len(), LISTABLE);
    assert!(!last.exists());
    assert!(
        !super::results(&h.life).is_empty(),
        "the oldest result goes out"
    );
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
    assert!(super::results(&h.life).is_empty());
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
    assert_eq!(super::results(&h.life).len(), 2);
    assert_eq!(h.life.held_jobs().len(), 2);
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
    // `r1`'s entry (41 bytes) leaves 24,535 bytes: 557 entries of 44, one fewer than
    // [`LISTABLE`].
    for n in 0..LISTABLE {
        let id = format!("x{n:04}");
        let body = json!({ "keyed": "job", "result": {
            "run_id": id, "lease_id": "l", "outcome": "killed" } });
        fs::write(dir.join(format!("{id}.json")), body.to_string()).unwrap();
    }
    let events = names(&h.restart().unwrap());
    assert_eq!(findings(&events, "held-record-unreadable"), 1, "{events:?}");
    assert_eq!(h.held().len(), LISTABLE);
    assert!(h.held().contains(&"r1".to_owned()));
    assert!(dir.join("r1.json").exists());
    assert!(!dir.join(format!("x{:04}.json", LISTABLE - 1)).exists());
}

/// The retired list's log.
fn retired_log(h: &Harness) -> PathBuf {
    h.dir.path("state").join("retired.log")
}

/// The retired list's file as an earlier release writes it, of the run ids `ids`.
fn list_of(ids: impl Iterator<Item = String>) -> Value {
    let retired: Vec<Value> = ids.map(|id| json!({ "job_id": id })).collect();
    json!({ "retired": retired })
}

/// The run id `s<n>`, of three digits so that every line of the log is as long.
fn sid(n: usize) -> String {
    format!("s{n:03}")
}

/// Asserts the sixteenth rule on a short list: sixteen ids are written whole one by one
/// (`{"job_id":"sNNN"}` is 17 bytes, a line of the log 18, the list of sixteen 301 bytes,
/// a sixteenth of which a line fits in, and the list of fifteen 283), the seventeenth is a line of the log and leaves the
/// list as it was, and the eighteenth, which would make the log longer than a sixteenth of
/// the list, writes the list whole with all eighteen and removes the log. A restart between
/// the two reads the log's id as retired. It catches a log never used, a log that is never
/// folded back into the list, and a log a restart does not read.
#[test]
fn an_id_is_a_line_of_the_log_while_the_log_stays_within_a_sixteenth_of_the_list() {
    let mut h = Harness::new("held-retired-log");
    for n in 0..16 {
        h.stop(&sid(n));
        assert!(
            !retired_log(&h).exists(),
            "{n}: a short list is written whole"
        );
    }
    let sixteen = list_of((0..16).map(sid));
    assert_eq!(json_of(&retired_file(&h)), sixteen);
    assert_eq!(fs::read_to_string(retired_file(&h)).unwrap().len(), 301);

    h.stop(&sid(16));
    assert_eq!(
        json_of(&retired_file(&h)),
        sixteen,
        "the list is not written"
    );
    assert_eq!(
        fs::read_to_string(retired_log(&h)).unwrap(),
        "{\"job_id\":\"s016\"}\n"
    );
    let mode = fs::metadata(retired_log(&h)).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    assert!(h.restart().unwrap().is_empty());
    for n in [0, 15, 16] {
        let id = sid(n);
        assert_eq!(
            names(&h.start(job(&id))),
            [format!("skipped {id} FenceRetired")]
        );
    }

    h.stop(&sid(17));
    assert!(!retired_log(&h).exists(), "the log is folded into the list");
    assert_eq!(json_of(&retired_file(&h)), list_of((0..18).map(sid)));
    h.restart().unwrap();
    assert_eq!(names(&h.start(job("s017"))), ["skipped s017 FenceRetired"]);
    assert_eq!(h.fake.created().len(), 0, "nothing retired runs");
}

/// Asserts the rule at the cap, on a list of 4096 ids an earlier release wrote: each id
/// retired after it is one line of the log while the list file stays byte for byte as it
/// was, until the log would pass a sixteenth of the list (90,125 bytes: 312 lines of 18); the next
/// id writes the list whole, the newest 4096 with the oldest dropped, and removes the log.
/// After a restart with the log, the oldest ids the log pushed out are no longer retired and
/// the newest are. It catches the rewrite of the whole list for every id at the cap (90 KB
/// here, some 370 KB with a node's ids, written and synced at each job end) and a log that
/// does not drop the oldest ids as the list does.
#[test]
fn at_the_cap_an_id_costs_a_line_until_the_log_reaches_a_sixteenth_of_the_list() {
    let mut h = Harness::new("held-retired-cap");
    let full = list_of((0..MAX_HELD).map(|n| format!("old{n:05}")));
    fs::write(retired_file(&h), serde_json::to_vec(&full).unwrap()).unwrap();
    let written = fs::read(retired_file(&h)).unwrap();
    assert!(h.restart().unwrap().is_empty());

    assert_eq!(written.len(), 90_125);
    let lines = written.len() / 16 / 18;
    assert_eq!(lines, 312);
    for n in 0..lines {
        h.stop(&sid(n));
    }
    assert_eq!(
        fs::read(retired_file(&h)).unwrap(),
        written,
        "never rewritten"
    );
    let log = fs::read_to_string(retired_log(&h)).unwrap();
    assert_eq!(log.lines().count(), lines);
    assert_eq!(log.len(), lines * 18);

    assert!(h.restart().unwrap().is_empty());
    assert_eq!(names(&h.start(job("old00000"))), ["started old00000"]);
    let last = format!("old{:05}", MAX_HELD - 1);
    assert_eq!(
        names(&h.start(job(&last))),
        [format!("skipped {last} FenceRetired")]
    );
    let newest = sid(lines - 1);
    assert_eq!(
        names(&h.start(job(&newest))),
        [format!("skipped {newest} FenceRetired")]
    );

    h.stop(&sid(lines));
    assert!(!retired_log(&h).exists());
    let list = json_of(&retired_file(&h));
    let ids = list["retired"].as_array().unwrap();
    assert_eq!(ids.len(), MAX_HELD);
    assert_eq!(ids[MAX_HELD - 1], json!({ "job_id": sid(lines) }));
    assert_eq!(ids[0], json!({ "job_id": format!("old{:05}", lines + 1) }));
}

/// Asserts what is read of a log that a crash cut: a last line no newline ends is left out
/// without a finding and the lines before it are read; a line in the middle that is no id is
/// left out with one finding and the lines around it are read; a last line cut inside a
/// character is left out like any cut line, the lines before it read; an empty log is read
/// as no ids. After each damaged log, the next id retired writes the list whole and removes the
/// log, so that nothing is appended after a cut line. It catches an agent that refuses to
/// start over a cut log, one that loses the good lines with the bad, and one that appends to
/// a log whose last line was cut, which would glue two ids into one line.
#[test]
fn a_cut_or_damaged_log_keeps_its_good_lines_and_is_folded_into_the_list() {
    let line = |id: &str| format!("{{\"job_id\":\"{id}\"}}\n");
    for (n, (log, finding)) in [
        (format!("{}{{\"job_id\":\"s0", line("s100")), false),
        (format!("{}not an id\n{}", line("s100"), line("s101")), true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut h = Harness::new(&format!("held-retired-cut-{n}"));
        let list = list_of((0..16).map(sid));
        fs::write(retired_file(&h), serde_json::to_vec(&list).unwrap()).unwrap();
        fs::write(retired_log(&h), &log).unwrap();
        let events = names(&h.restart().unwrap());
        match finding {
            true => assert_eq!(events, ["finding - held-record-unreadable"], "{n}"),
            false => assert!(events.is_empty(), "{n}: {events:?}"),
        }
        assert_eq!(
            names(&h.start(job("s100"))),
            ["skipped s100 FenceRetired"],
            "{n}"
        );
        assert_eq!(names(&h.start(job("s0"))), ["started s0"], "{n}");

        h.stop("s200");
        assert!(!retired_log(&h).exists(), "{n}: the log is folded");
        let ids = json_of(&retired_file(&h))["retired"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(ids.len(), 16 + 1 + usize::from(finding) + 1, "{n}");
        assert_eq!(ids.last().unwrap(), &json!({ "job_id": "s200" }), "{n}");
    }

    // A write cut inside a character spoils only its own line.
    let mut h = Harness::new("held-retired-cut-char");
    let list = list_of((0..16).map(sid));
    fs::write(retired_file(&h), serde_json::to_vec(&list).unwrap()).unwrap();
    let mut log = line("s100").into_bytes();
    log.extend(b"{\"job_id\":\"\xc3");
    fs::write(retired_log(&h), log).unwrap();
    assert!(h.restart().unwrap().is_empty());
    assert_eq!(names(&h.start(job("s100"))), ["skipped s100 FenceRetired"]);
    h.stop("s200");
    assert!(!retired_log(&h).exists(), "a cut log is folded");

    let mut h = Harness::new("held-retired-empty-log");
    let list = list_of((0..16).map(sid));
    fs::write(retired_file(&h), serde_json::to_vec(&list).unwrap()).unwrap();
    fs::write(retired_log(&h), "").unwrap();
    assert!(h.restart().unwrap().is_empty());
    h.stop("s200");
    assert_eq!(
        json_of(&retired_file(&h)),
        list,
        "an empty log is appended to"
    );
    assert_eq!(fs::read_to_string(retired_log(&h)).unwrap(), line("s200"));
}

/// Asserts the failures of the log: a log that cannot be read (a directory) is a finding at
/// the restart and its ids are not there; the next id then writes the list whole and, the log
/// not being removable, is a `held-record-failed` finding until it can be, the result file
/// of a job retired meanwhile staying until then; and a log that cannot be opened to append
/// to is a finding, after which the next id writes the list whole. It catches an id lost to
/// a failed append and a result file dropped before its id is on disk.
#[test]
fn a_log_that_cannot_be_read_removed_or_appended_to_is_reported_and_tried_again() {
    let mut h = ended("held-retired-log-dir");
    fs::create_dir(retired_log(&h)).unwrap();
    let events = names(&h.restart().unwrap());
    assert!(
        events.contains(&"finding - held-record-unreadable".to_owned()),
        "{events:?}"
    );
    let events = ack_both(&mut h, "ACCEPTED");
    assert!(
        events.contains(&"finding - held-record-failed".to_owned()),
        "{events:?}"
    );
    assert!(
        held_file(&h, "r1").exists(),
        "kept until the list is whole on disk"
    );
    fs::remove_dir(retired_log(&h)).unwrap();
    assert!(h.life.step().is_empty());
    assert!(!held_file(&h, "r1").exists());
    assert_eq!(
        json_of(&retired_file(&h)),
        json!({ "retired": [
            { "job_id": "j1", "allocation_id": "alloc-j1" },
            { "job_id": "r1", "allocation_id": "lease-r1" },
        ] })
    );

    let mut h = Harness::new("held-retired-log-append");
    for n in 0..16 {
        h.stop(&sid(n));
    }
    fs::create_dir(retired_log(&h)).unwrap();
    assert_eq!(names(&h.stop(&sid(16))), ["finding - held-record-failed"]);
    fs::remove_dir(retired_log(&h)).unwrap();
    assert!(h.life.step().is_empty());
    assert!(
        !retired_log(&h).exists(),
        "written whole after a failed append"
    );
    assert_eq!(json_of(&retired_file(&h)), list_of((0..17).map(sid)));
}
