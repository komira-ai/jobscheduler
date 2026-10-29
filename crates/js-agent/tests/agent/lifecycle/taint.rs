//! The taint: when the marker is written, what a tainted agent does, and what survives a
//! restart. The marker is a real file under the test's state root; the runtime is the
//! in-memory fake, whose log notes whether the marker existed at each of its calls.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::lifecycle::taint::{MARKER_TEMP_FILE, reason};
use js_agent::lifecycle::{CheckFinding, Event, StateRoot, code};
use js_agent_wire::{Isolation, JobSpec, Outcome, Taint};
use serde_json::json;

use super::{Harness, Options, cleanup, job, names, reply_json};

/// The labels of a container no job can stand for (its run id names a path) that is not a
/// host job's.
const ODD: [(&str, &str); 2] = [
    ("jobscheduler.dev/run-id", "../x"),
    ("jobscheduler.dev/isolation", "contained"),
];

/// A start that may change the host.
fn host_job(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.isolation = Isolation::Host;
    spec
}

/// Options for an agent that allows host jobs.
fn allowing() -> Options {
    Options {
        allow_host_jobs: true,
        ..Options::default()
    }
}

/// Makes every write of the marker fail: a directory stands where the temporary file
/// would be written.
fn block_marker(h: &Harness) {
    fs::create_dir(h.marker().with_file_name(MARKER_TEMP_FILE)).unwrap();
}

/// Lets the marker be written again.
fn unblock_marker(h: &Harness) {
    fs::remove_dir(h.marker().with_file_name(MARKER_TEMP_FILE)).unwrap();
}

fn residue_of(run_id: &str, check: &str) -> Taint {
    Taint {
        reason: reason::RESIDUE.to_owned(),
        run_id: Some(run_id.to_owned()),
        check: Some(check.to_owned()),
    }
}

/// An agent with a check that finds `stray-process` after `r1`.
fn finding_after_r1(name: &str, watch_marker: bool) -> Harness {
    Harness::with(
        name,
        Options {
            findings: vec![("r1", CheckFinding::residue("stray-process", "pid 41"))],
            watch_marker,
            ..Options::default()
        },
    )
}

fn ack(h: &mut Harness) -> Vec<Event> {
    h.life.apply(&reply_json(&json!({ "taint_ack": true })))
}

/// Asserts the order of a taint: for a host job, the marker is on disk before its
/// container is created, still there when the container is removed and the checks run, and
/// before the slot is freed; for a finding, the taint is recorded before the finding and the
/// marker is on disk when the step returns with the slot still booked. The log of the fake
/// runtime and the checks notes the marker at each call. It catches a marker written after
/// the container exists (a crash between would lose the taint), after the slot is freed, or
/// after the finding is reported.
#[test]
fn marker_written_before_report_and_before_free() {
    let mut h = Harness::with(
        "order-host",
        Options {
            watch_marker: true,
            ..allowing()
        },
    );
    assert_eq!(
        names(&h.start(host_job("r1"))),
        ["tainted r1 host-job", "started r1"]
    );
    assert!(h.marker().exists());
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert_eq!(
        *h.log.borrow(),
        [
            "create r1 [marker present]",
            "remove c1 (scratch present) [marker present]",
            "check r1 (container gone, scratch gone) [marker present]"
        ]
    );

    let mut h = finding_after_r1("order-check", true);
    h.start(job("r1"));
    assert!(!h.marker().exists(), "a contained job taints nothing");
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "tainted r1 residue",
            "finding r1 stray-process"
        ]
    );
    assert!(h.marker().exists());
    assert_eq!(
        h.life.pending_taint(),
        Some(residue_of("r1", "stray-process"))
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is still booked");
}

/// Asserts the fail-closed rule: when the marker cannot be written, the agent is tainted
/// in memory all the same. It refuses every start, notes the failure as a finding of the
/// job, frees no slot (not even the slot of a job whose own checks pass), reports nothing
/// until the marker is on disk, and writes the marker as soon as it can, then reports. It
/// catches an agent that treats a marker it could not write as no taint, and one that
/// reports a taint the host would forget.
#[test]
fn marker_write_failure_keeps_refusing() {
    let mut h = finding_after_r1("write-fails", false);
    block_marker(&h);
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "tainted r1 residue",
            "finding r1 taint-write-failed",
            "finding r1 stray-process"
        ]
    );
    assert!(!h.marker().exists());
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.pending_taint(),
        None,
        "nothing is reported while the marker is not on disk"
    );
    assert_eq!(
        names(&h.start(job("r3"))),
        ["refused r3 host-tainted"],
        "and no start is taken"
    );

    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r2",
            "container removed r2",
            "scratch removed r2",
            "finding r2 taint-write-failed"
        ],
        "r2 passed its own checks, and its slot is held for the marker"
    );
    assert_eq!(h.life.free().cpu_millis, 2000);

    unblock_marker(&h);
    let events = names(&h.life.step());
    assert_eq!(events, ["checks passed r2", "freed r2"]);
    assert!(h.marker().exists(), "written at the first step that could");
    assert_eq!(
        h.life.pending_taint(),
        Some(residue_of("r1", "stray-process")),
        "and reported from then on"
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "r1 stays blocked");
}

/// Asserts that a marker that cannot be written when a host job starts keeps the job from
/// starting: no container is created, the result is `failed_to_start` with the detail
/// `taint-failed`, and the agent is tainted all the same. It catches a host job that runs
/// with no durable record of what it may do to the host.
#[test]
fn a_host_job_whose_marker_cannot_be_written_is_not_started() {
    let mut h = Harness::with("host-no-marker", allowing());
    block_marker(&h);
    let events = names(&h.start(host_job("r1")));
    assert_eq!(events, ["tainted r1 host-job", "refused r1 taint-failed"]);
    assert!(h.fake.created().is_empty());
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("taint-failed"));
    assert!(h.life.is_tainted());
    assert!(!h.marker().exists());
}

/// Asserts that a restart finds the taint: the marker is read when the agent starts, the
/// new lifecycle is tainted, reports the same taint again, and refuses starts, for a taint
/// from a check and for one from a host job. It catches a taint kept only in memory.
#[test]
fn taint_survives_restart() {
    let mut h = finding_after_r1("survives", false);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    let before = h.life.pending_taint();
    assert!(before.is_some());
    ack(&mut h);
    assert_eq!(h.life.pending_taint(), None);
    h.restart().unwrap();
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.pending_taint(),
        before,
        "a new process reports it again: the server's word is not on this host"
    );
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);

    let mut host = Harness::with("survives-host", allowing());
    host.start(host_job("h1"));
    host.restart().unwrap();
    assert_eq!(
        host.life.pending_taint().map(|taint| taint.reason),
        Some(reason::HOST_JOB.to_owned())
    );
    assert_eq!(names(&host.start(job("h2"))), ["refused h2 host-tainted"]);
}

/// Asserts that a marker that is there and cannot be used still taints the host, with the
/// reason `marker-unreadable`: text that is not a taint, a taint the wire would refuse, one
/// longer than a marker can be, and a marker that is a directory. It catches a damaged
/// marker read as the absence of one.
#[test]
fn a_marker_that_cannot_be_read_is_a_taint() {
    let h = Harness::new("unreadable-marker");
    let marker = h.marker();
    let valid = serde_json::to_string(&residue_of("r1", "new-mount")).unwrap();
    let empty_reason = json!({ "reason": "" }).to_string();
    let too_long = format!(r#"{{"reason":"{}"}}"#, "x".repeat(5000));
    let state = StateRoot::open(marker.parent().unwrap()).unwrap();
    assert_eq!(state.read(), None, "no marker, no taint");
    for (text, expected) in [
        (valid.as_str(), residue_of("r1", "new-mount").reason),
        ("not json", reason::MARKER_UNREADABLE.to_owned()),
        ("", reason::MARKER_UNREADABLE.to_owned()),
        (empty_reason.as_str(), reason::MARKER_UNREADABLE.to_owned()),
        (too_long.as_str(), reason::MARKER_UNREADABLE.to_owned()),
    ] {
        fs::write(&marker, text).unwrap();
        assert_eq!(state.read().map(|t| t.reason), Some(expected), "{text:.20}");
    }
    fs::write(&marker, [0xff, 0xfe]).unwrap();
    assert_eq!(
        state.read().map(|t| t.reason),
        Some(reason::MARKER_UNREADABLE.to_owned()),
        "not text"
    );
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    assert_eq!(
        state.read().map(|t| t.reason),
        Some(reason::MARKER_UNREADABLE.to_owned()),
        "a directory"
    );
}

/// Asserts that the server's acknowledgement stops the report and nothing else: the marker
/// stays, the agent stays tainted and keeps refusing starts, and an acknowledgement that
/// arrives while the host is not tainted acknowledges nothing, so a taint that comes later
/// is still reported. It catches an acknowledgement that clears the taint, and one that is
/// remembered before there is anything to acknowledge.
#[test]
fn taint_ack_keeps_marker() {
    let mut h = finding_after_r1("ack", false);
    ack(&mut h);
    assert_eq!(h.life.pending_taint(), None);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(
        h.life.pending_taint().is_some(),
        "an earlier ack acknowledged nothing"
    );
    h.life.apply(&reply_json(&json!({ "taint_ack": false })));
    assert!(
        h.life.pending_taint().is_some(),
        "taint_ack false is not one"
    );
    ack(&mut h);
    assert_eq!(h.life.pending_taint(), None, "the report stops");
    assert!(h.life.is_tainted());
    assert!(h.marker().exists(), "the marker stays");
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
    let kept = fs::read_to_string(h.marker()).unwrap();
    assert!(kept.contains("stray-process"), "{kept}");
}

/// Asserts that a tainted agent refuses a start with the code `host-tainted` (a held,
/// terminal `failed_to_start` result) and lets the job it runs finish and report. It
/// catches a taint that kills running work, and one that lets a new job in.
#[test]
fn a_tainted_agent_refuses_starts_and_lets_running_jobs_finish() {
    let mut h = finding_after_r1("refuses", false);
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(names(&h.start(job("r3"))), ["refused r3 host-tainted"]);
    let refused = h
        .life
        .results()
        .into_iter()
        .find(|result| result.run_id == "r3")
        .unwrap();
    assert_eq!(refused.outcome, Outcome::FailedToStart);
    assert_eq!(refused.detail.as_deref(), Some("host-tainted"));
    h.fake.exit("r2", 7);
    let events = names(&h.life.step());
    assert_eq!(events[0], "result r2");
    assert_eq!(events.last().unwrap(), "freed r2");
    assert!(
        h.life
            .results()
            .iter()
            .any(|r| r.run_id == "r2" && r.outcome == Outcome::Failed),
        "the running job finished and reported"
    );
    assert!(
        h.start(job("r1")).is_empty(),
        "a start for a run held is still no news"
    );
}

/// Asserts that a host job taints the host whatever happens to it: it exits 0, fails,
/// runs out of memory, is stopped, cancelled or timed out, or fails to start. For each, the
/// marker is on disk the moment the start is applied, names the run, and is still there
/// when the slot is freed. It catches a host job whose taint depends on how it ended.
#[test]
fn host_job_taints_on_every_outcome() {
    type Ending = fn(&mut Harness);
    let endings: [(&str, Ending); 7] = [
        ("exit0", |h| h.fake.exit("r1", 0)),
        ("exit3", |h| h.fake.exit("r1", 3)),
        ("oom", |h| h.fake.exit_oom("r1")),
        ("stopped", |h| {
            h.stop("r1");
        }),
        ("cancelled", |h| {
            h.cancel("r1");
        }),
        ("timed-out", |h| h.clock.advance(60_000)),
        ("never-started", |_| {}),
    ];
    for (name, end) in endings {
        let mut h = Harness::with(&format!("host-{name}"), allowing());
        if name == "never-started" {
            h.fake.fail("create", 1);
        }
        let events = names(&h.start(host_job("r1")));
        assert_eq!(events[0], "tainted r1 host-job", "{name}");
        assert!(h.marker().exists(), "{name}: before anything else");
        end(&mut h);
        h.life.step();
        h.life.step();
        assert_eq!(h.life.free().cpu_millis, 4000, "{name}: the slot is free");
        assert!(h.marker().exists(), "{name}");
        let taint = h.life.pending_taint().unwrap();
        assert_eq!(taint.run_id.as_deref(), Some("r1"), "{name}");
        assert_eq!(taint.check, None, "{name}");
    }
}

/// Asserts that a start the runtime reports as showing that the agent cannot see the host
/// taints the host as unchecked, with the run and the code `host-not-visible`, before
/// anything else of the failure (taint, then the finding, then the result); that the marker
/// is on disk and the taint is reported; that the result is `failed_to_start` with the
/// detail `runtime-failed`; that every later start is refused as `host-tainted`; and that a
/// start that fails for any other reason taints nothing. It catches an agent that cannot
/// see its jobs and goes on accepting them, and a plain failed create that taints a host.
#[test]
fn a_start_that_shows_the_host_is_not_seen_taints_it_as_unchecked() {
    let mut h = Harness::with(
        "blind-start",
        Options {
            watch_marker: true,
            ..Options::default()
        },
    );
    h.fake.fail("create", 1);
    assert_eq!(
        names(&h.start(job("plain"))),
        ["finding plain start-failed", "result plain"]
    );
    assert!(!h.marker().exists(), "a failed create taints nothing");
    assert!(!h.life.is_tainted());

    h.fake.blind_next_create();
    assert_eq!(
        names(&h.start(job("r1"))),
        [
            "tainted r1 unchecked",
            "finding r1 start-failed",
            "result r1"
        ]
    );
    assert!(h.marker().exists());
    assert_eq!(
        h.life.pending_taint(),
        Some(Taint {
            reason: reason::UNCHECKED.to_owned(),
            run_id: Some("r1".to_owned()),
            check: Some(code::HOST_NOT_VISIBLE.to_owned()),
        })
    );
    let result = &h.life.results()[1];
    assert_eq!(
        (
            result.run_id.as_str(),
            result.outcome,
            result.detail.as_deref()
        ),
        ("r1", Outcome::FailedToStart, Some("runtime-failed"))
    );
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
}

/// Asserts that a host job's container is marked so that a restarted agent knows it ran
/// one: an agent that is found with a marker lost (a new state root) and a host job still
/// running taints the host when it adopts the container. It catches a host job that
/// outlives the agent and its marker and leaves the host looking clean.
#[test]
fn an_adopted_host_job_taints_the_host() {
    let mut h = Harness::with("adopt-host", allowing());
    h.start(job("r2"));
    h.start(host_job("r1"));
    assert_eq!(
        h.fake.created()[0].labels["jobscheduler.dev/isolation"],
        "contained"
    );
    assert_eq!(
        h.fake.created()[1].labels["jobscheduler.dev/isolation"],
        "host"
    );
    fs::remove_file(h.marker()).unwrap();
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        ["adopted r2", "tainted r1 host-job", "adopted r1"],
        "only the host job taints"
    );
    assert!(h.marker().exists());
}

/// Asserts that an adopted host job whose marker cannot be written is a finding of its own
/// (the host is tainted in memory), and so is a container that cannot be removed and
/// whose taint cannot be written. It catches an adoption that drops the failure.
#[test]
fn a_marker_that_cannot_be_written_at_adoption_is_a_finding() {
    let mut h = Harness::with("adopt-fails", allowing());
    h.start(host_job("r1"));
    fs::remove_file(h.marker()).unwrap();
    block_marker(&h);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted r1 host-job",
            "finding r1 taint-write-failed",
            "adopted r1"
        ]
    );
    assert!(h.life.is_tainted());

    let mut h = Harness::new("evict-fails");
    block_marker(&h);
    let odd = h.fake.leak(&ODD, 0);
    h.fake.fail("remove", 2);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted - cleanup-failed",
            "finding - taint-write-failed",
            "finding - unadoptable-container-kept"
        ]
    );
    assert!(h.fake.existing().contains(&odd));
    h.fake.fail("remove", 0);
    let events = names(&h.life.step());
    assert_eq!(events, ["finding - unadoptable-container"]);
}

/// Asserts that a cleanup that fails taints the host and keeps the slot: a container that
/// cannot be removed, and a scratch directory that cannot be removed, each record the
/// taint before the finding; the cleanup finishes when it can, the slot is freed then, and
/// the host stays tainted. It catches a cleanup failure that is only a finding, and a taint
/// that goes away with the failure.
#[test]
fn cleanup_failure_taints_and_keeps_slot() {
    let mut h = Harness::new("cleanup-container");
    h.start(job("r1"));
    h.fake.fail("remove", 1);
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "tainted r1 cleanup-failed",
            "finding r1 container-remove-failed"
        ]
    );
    let taint = h.life.pending_taint().unwrap();
    assert_eq!(taint.check.as_deref(), Some(code::CONTAINER_REMOVE_FAILED));
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is kept");
    assert_eq!(names(&h.life.step()), cleanup("r1"));
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert!(h.life.is_tainted(), "and the host stays tainted");
    assert!(h.marker().exists());

    let mut h = Harness::new("cleanup-scratch");
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
            "tainted r1 cleanup-failed",
            "finding r1 scratch-failed"
        ]
    );
    assert_eq!(
        h.life.pending_taint().unwrap().check.as_deref(),
        Some(code::SCRATCH_FAILED)
    );
    assert_eq!(h.life.free().cpu_millis, 3000);
}

/// Asserts that a container that is left on the host at adoption (it cannot be removed)
/// taints the host and that the later retries add no second taint. It catches an orphan
/// container that is only a finding.
#[test]
fn an_orphan_that_stays_taints_the_host() {
    let mut h = Harness::new("orphan-taint");
    h.fake.leak(&ODD, 0);
    h.fake.fail("remove", 1);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted - cleanup-failed",
            "finding - unadoptable-container-kept"
        ]
    );
    assert_eq!(
        h.life.pending_taint().unwrap().check.as_deref(),
        Some(code::UNADOPTABLE_KEPT)
    );
    assert_eq!(names(&h.life.step()), ["finding - unadoptable-container"]);
}

/// Asserts the state root's own rules: only a directory that the agent's user owns and
/// others cannot enter is accepted, naming the flag; the marker is written whole, replaces
/// an older one, is private to its owner and leaves no temporary file; and a second cause
/// does not change the first taint. It catches a marker kept where other users can write
/// it, and one that is overwritten by a later cause.
#[test]
fn the_state_root_is_private_and_the_first_taint_stays() {
    let h = Harness::new("state-root");
    let root = h.marker().parent().unwrap().to_path_buf();
    let open = root.with_file_name("open-state");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    let file = root.with_file_name("file-state");
    fs::write(&file, "x").unwrap();
    for (path, text) in [
        (
            open.as_path(),
            "--state-root: group and others must have no permission",
        ),
        (file.as_path(), "--state-root: not a directory"),
        (
            Path::new("/nonexistent/state"),
            "--state-root: cannot read the directory",
        ),
    ] {
        assert!(
            StateRoot::open(path).unwrap_err().starts_with(text),
            "{path:?}"
        );
    }
    let state = StateRoot::open(&root).unwrap();
    let first = residue_of("r1", "new-mount");
    state.write(&first).unwrap();
    state
        .write(&residue_of("r2", "kernel-module-changed"))
        .unwrap();
    assert_eq!(state.read().map(|t| t.run_id), Some(Some("r2".to_owned())));
    let listed: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(listed, ["taint.json"], "no temporary file is left");
    assert_eq!(
        fs::metadata(state.marker_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(state.path(), fs::canonicalize(&root).unwrap());

    let mut tainted = finding_after_r1("first-stays", false);
    tainted.start(job("r1"));
    tainted.start(job("r2"));
    tainted.fake.exit("r1", 0);
    tainted.life.step();
    tainted.fake.fail("remove", 1);
    tainted.fake.exit("r2", 0);
    tainted.life.step();
    let taint = tainted.life.pending_taint().unwrap();
    assert_eq!(
        (taint.reason.as_str(), taint.run_id.as_deref()),
        ("residue", Some("r1")),
        "the second cause changed nothing"
    );
}

/// The labels of a container this agent started for `run_id` that books one CPU and 1 GiB,
/// with the isolation `isolation` (`None`: no isolation label at all).
fn container(run_id: &str, isolation: Option<&str>) -> Vec<(&'static str, String)> {
    let mut labels = vec![
        ("jobscheduler.dev/run-id", run_id.to_owned()),
        ("jobscheduler.dev/lease-id", "lease-old".to_owned()),
        ("jobscheduler.dev/cpu-millis", "1000".to_owned()),
        ("jobscheduler.dev/memory-bytes", "1073741824".to_owned()),
        ("jobscheduler.dev/gpus", String::new()),
        ("jobscheduler.dev/wall-ms", "60000".to_owned()),
    ];
    labels.extend(isolation.map(|text| ("jobscheduler.dev/isolation", text.to_owned())));
    labels
}

/// `labels` with the value of `key` replaced.
fn with_label(
    mut labels: Vec<(&'static str, String)>,
    key: &str,
    value: &str,
) -> Vec<(&'static str, String)> {
    labels.iter_mut().for_each(|(k, v)| {
        if *k == key {
            *v = value.to_owned();
        }
    });
    labels
}

/// Leaks `labels` into the fake runtime.
fn leak(h: &Harness, labels: &[(&'static str, String)]) -> String {
    let borrowed: Vec<(&str, &str)> = labels.iter().map(|(k, v)| (*k, v.as_str())).collect();
    h.fake.leak(&borrowed, 0)
}

/// Asserts that a container the agent cannot account for taints the host when it is, or may
/// be, a host job's: one that carries the host isolation label, one with no isolation label
/// and one with a label the agent never writes, whichever way its booking is broken (an
/// unreadable number, more than the machine has, a run id that names a path, a run id held
/// already), is removed with the taint of a host job (naming the run when its id is one);
/// one labelled `contained` taints nothing. It catches a host job's container that is
/// removed without a trace because its booking failed, and an unlabelled container read as
/// a contained one.
#[test]
fn a_container_the_agent_cannot_adopt_taints_the_host_unless_it_is_contained() {
    /// (name, isolation label, label to break, its value, whether the host is tainted, the
    /// run the taint names)
    type Case<'a> = (&'a str, Option<&'a str>, &'a str, &'a str, bool, &'a str);
    let cases: [Case; 8] = [
        (
            "host",
            Some("host"),
            "jobscheduler.dev/cpu-millis",
            "lots",
            true,
            "h1",
        ),
        (
            "host-big",
            Some("host"),
            "jobscheduler.dev/cpu-millis",
            "99999999",
            true,
            "h1",
        ),
        (
            "host-path",
            Some("host"),
            "jobscheduler.dev/run-id",
            "../x",
            true,
            "-",
        ),
        (
            "unlabelled",
            None,
            "jobscheduler.dev/cpu-millis",
            "lots",
            true,
            "h1",
        ),
        (
            "unknown",
            Some("weird"),
            "jobscheduler.dev/cpu-millis",
            "lots",
            true,
            "h1",
        ),
        (
            "empty",
            Some(""),
            "jobscheduler.dev/cpu-millis",
            "lots",
            true,
            "h1",
        ),
        (
            "contained",
            Some("contained"),
            "jobscheduler.dev/cpu-millis",
            "lots",
            false,
            "h1",
        ),
        (
            "contained-path",
            Some("contained"),
            "jobscheduler.dev/run-id",
            "../x",
            false,
            "-",
        ),
    ];
    for (name, isolation, key, value, taints, run) in cases {
        let mut h = Harness::new(&format!("evict-{name}"));
        let broken = with_label(container("h1", isolation), key, value);
        let id = leak(&h, &broken);
        let events = names(&h.restart().unwrap());
        match taints {
            true => {
                assert_eq!(
                    events,
                    [
                        format!("tainted {run} host-job"),
                        "finding - unadoptable-container".to_owned()
                    ],
                    "{name}"
                );
                assert!(
                    h.marker().exists(),
                    "{name}: on disk before the container goes"
                );
                assert!(h.life.is_tainted(), "{name}");
            }
            false => {
                assert_eq!(events, ["finding - unadoptable-container"], "{name}");
                assert!(!h.marker().exists(), "{name}");
                assert!(!h.life.is_tainted(), "{name}");
            }
        }
        assert!(
            !h.fake.existing().contains(&id),
            "{name}: removed all the same"
        );
    }

    let mut twice = Harness::new("evict-host-twice");
    leak(&twice, &container("h1", Some("host")));
    leak(&twice, &container("h1", Some("host")));
    let events = names(&twice.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted h1 host-job",
            "adopted h1",
            "finding - unadoptable-container"
        ],
        "the first taint stays, and the repeated run is removed"
    );
}

/// Asserts that a container found after a restart with no isolation label, or one the agent
/// never writes, is adopted as a host job (the host is tainted, naming the run), and one
/// labelled `contained` is not. It catches a container of unknown isolation that is taken
/// for a contained one.
#[test]
fn an_adopted_container_of_unknown_isolation_is_a_host_job() {
    for (isolation, taints) in [
        (None, true),
        (Some("weird"), true),
        (Some("host"), true),
        (Some("contained"), false),
    ] {
        let mut h = Harness::new("adopt-unknown-isolation");
        leak(&h, &container("r9", isolation));
        let events = names(&h.restart().unwrap());
        let expected: &[&str] = match taints {
            true => &["tainted r9 host-job", "adopted r9"],
            false => &["adopted r9"],
        };
        assert_eq!(events, expected, "{isolation:?}");
    }
}

/// Asserts that a taint is reported only once the marker is on disk and that an
/// acknowledgement before that acknowledges nothing: with the marker blocked the host is
/// tainted and refuses starts but reports nothing; an `ack` that arrives then does not
/// count, so the report goes out when the marker is written, and stops at the next ack. It
/// catches a taint reported to the server that a crash would make the host forget, and an
/// acknowledgement of a report that was never made.
#[test]
fn a_taint_is_reported_only_once_it_is_on_disk() {
    let mut h = finding_after_r1("report-after-disk", false);
    block_marker(&h);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
    ack(&mut h);
    unblock_marker(&h);
    h.life.step();
    assert_eq!(
        h.life.pending_taint(),
        Some(residue_of("r1", "stray-process")),
        "the early acknowledgement did not stop the report"
    );
    ack(&mut h);
    assert_eq!(h.life.pending_taint(), None);
}

/// Asserts that `step` writes a marker that could not be written when the host was
/// tainted, with no job involved: a container that cannot be removed taints the host while
/// the marker is blocked; once it can be written the next step writes it and the taint is
/// reported. It catches a step that leaves the taint in memory for good, which a restart
/// would lose.
#[test]
fn step_writes_a_marker_that_could_not_be_written() {
    let mut h = Harness::new("step-persists");
    block_marker(&h);
    leak(
        &h,
        &with_label(
            container("odd", Some("contained")),
            "jobscheduler.dev/run-id",
            "../x",
        ),
    );
    h.fake.fail("remove", 1);
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        [
            "tainted - cleanup-failed",
            "finding - taint-write-failed",
            "finding - unadoptable-container-kept"
        ]
    );
    assert!(!h.marker().exists());
    assert_eq!(h.life.pending_taint(), None);
    unblock_marker(&h);
    h.life.step();
    assert!(h.marker().exists(), "written by the step alone");
    assert_eq!(
        h.life.pending_taint().map(|taint| taint.reason),
        Some(reason::CLEANUP_FAILED.to_owned())
    );
}

/// Asserts that a check that could not run taints the host as `unchecked` and not as
/// `residue`, naming the check, and says in the finding that the check could not run. It
/// catches a check that did not run reported as one that found something.
#[test]
fn a_check_that_could_not_run_taints_as_unchecked() {
    let mut h = Harness::with(
        "unchecked",
        Options {
            findings: vec![(
                "r1",
                CheckFinding::unchecked("mounts-unreadable", "no mountinfo"),
            )],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "tainted r1 unchecked",
            "finding r1 mounts-unreadable"
        ]
    );
    let taint = h.life.pending_taint().unwrap();
    assert_eq!(
        (taint.reason.as_str(), taint.check.as_deref()),
        (reason::UNCHECKED, Some("mounts-unreadable"))
    );
    assert_eq!(
        h.life.findings()[0].message,
        "the check could not run: no mountinfo"
    );
}

/// Asserts that a host job that is refused for any reason other than the taint taints
/// nothing: one that names a mount root that does not exist, one larger than the machine
/// and one that does not fit beside a running job leave no marker and the agent clean. It
/// catches a host job that taints the host on a start that creates nothing.
#[test]
fn a_host_job_that_is_refused_for_another_reason_taints_nothing() {
    let mut h = Harness::with("host-refused", allowing());
    let mut bad_mount = host_job("m1");
    bad_mount.mounts = vec![js_agent_wire::Mount {
        root_name: "nowhere".to_owned(),
        rel: "x".to_owned(),
        ro: true,
    }];
    let mut large = host_job("l1");
    large.limits.cpu_millis = 99_000_000;
    let mut big = job("b1");
    big.limits.cpu_millis = 3500;
    let mut crowded = host_job("c1");
    crowded.limits.cpu_millis = 1000;
    for (spec, code) in [
        (bad_mount, "invalid-mount"),
        (large, "limits-exceed-shape"),
        (big, "started"),
        (crowded, "no-capacity"),
    ] {
        let run = spec.run_id.clone();
        let expected = match code {
            "started" => format!("started {run}"),
            _ => format!("refused {run} {code}"),
        };
        assert_eq!(names(&h.start(spec)), [expected]);
        assert!(!h.marker().exists(), "{run}");
        assert!(!h.life.is_tainted(), "{run}");
    }
    assert_eq!(h.fake.created().len(), 1, "only b1 started");
}

/// Asserts that a start on a host that a check tainted is refused before the checks are
/// told the host is idle: the first job starts on an idle host (one idle call), its check
/// finds something, and the next start is refused `host-tainted` with no second call. An
/// idle call there would take a new baseline over what the job left. It catches the idle
/// call moved above the refusal, or the taint guard dropped from it.
#[test]
fn a_tainted_host_refuses_a_start_before_the_checks_hear_it_is_idle() {
    let mut h = Harness::with(
        "idle-after-taint",
        Options {
            findings: vec![("r1", CheckFinding::residue("new-mount", "/mnt/x"))],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    assert_eq!(h.fake.idles(), 1, "r1 started on an idle host");
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
    assert_eq!(h.fake.idles(), 1, "no idle call on a tainted host");
}

/// Asserts that a container the agent found at start and could not remove keeps the
/// checks from being told the host is idle: the host is tainted, and a start is refused
/// `host-tainted` with no idle call. It catches an idle test that looks at the jobs alone
/// and forgets a container no job stands for.
#[test]
fn a_container_that_could_not_be_removed_keeps_the_host_from_being_idle() {
    let mut h = Harness::new("idle-stuck");
    h.fake.leak(&ODD, 0);
    h.fake.fail("remove", 1);
    h.restart().unwrap();
    assert!(h.life.is_tainted());
    assert_eq!(names(&h.start(job("r1"))), ["refused r1 host-tainted"]);
    assert_eq!(h.fake.idles(), 0);
}
