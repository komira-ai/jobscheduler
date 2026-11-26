//! The held script `tools/held/agent-handover-check/agent_handover_check.sh`, run by `bash`
//! with stand-ins first on `PATH`: a `js` that logs each call's arguments and answers from
//! files of the test (the n-th `host list` from `hosts-<n>.json`, then `hosts-last.json`; the
//! running jobs from `running-before.json` until the hosts were read twice and from
//! `running-after.json` then, the page after token `p2` from `running-before-p2.json`; a
//! `job get` from `jobs/<id>.json`, failing when there is none; the ended jobs from
//! `ended.json`), and a `sleep` that returns at once. So each test pins what the script reads,
//! that it calls nothing but `host list`, `job list` and `job get`, its verdict for each way a
//! roll can go, and what it refuses (exit 2, with no reading of the hosts during a roll).
//! Nothing reaches a server.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::install_script;

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The stand-in `js`.
const FAKE_JS: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_DIR/js.log"
case "$*" in
*'host list'*)
  n=$(cat "$FAKE_DIR/hosts.n" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$FAKE_DIR/hosts.n"
  f="$FAKE_DIR/hosts-$n.json"; [ -f "$f" ] || f="$FAKE_DIR/hosts-last.json"
  if [ "$(cat "$f")" = FAIL ]; then exit 1; fi
  cat "$f" ;;
*'--page-token p2'*) cat "$FAKE_DIR/running-before-p2.json" ;;
*'--states running'*)
  if [ "$(cat "$FAKE_DIR/hosts.n")" -le 1 ]; then cat "$FAKE_DIR/running-before.json"; else cat "$FAKE_DIR/running-after.json"; fi ;;
*'job get --job-id '*) cat "$FAKE_DIR/jobs/${*##*--job-id }.json" 2>/dev/null || exit 1 ;;
*) cat "$FAKE_DIR/ended.json" ;;
esac
"#;

/// A host listing entry: `id` with agent `version`, live, and the JSON fields in `more`.
fn host(id: &str, version: &str, more: &str) -> String {
    format!(r#"{{"id":"{id}","state":"HOST_STATE_LIVE","agentVersion":"{version}"{more}}}"#)
}

/// A host listing of `hosts`.
fn hosts(hosts: &[String]) -> String {
    format!(r#"{{"hosts":[{}]}}"#, hosts.join(","))
}

/// A running job `id` on `host` in `attempt` (0 leaves the field out, as proto JSON does).
fn running(id: &str, host: &str, attempt: u32) -> String {
    let attempt = if attempt == 0 {
        String::new()
    } else {
        format!(r#","attempt":{attempt}"#)
    };
    format!(r#"{{"id":"{id}","hostId":"{host}","state":"JOB_STATE_RUNNING"{attempt}}}"#)
}

/// A job `id` on `host` that ended in `state` at `ended_ms`.
fn ended(id: &str, host: &str, state: &str, ended_ms: u64) -> String {
    format!(
        r#"{{"id":"{id}","hostId":"{host}","state":"{state}","times":{{"endedAtMs":"{ended_ms}"}}}}"#
    )
}

/// A job listing of `jobs`, with `next` as its next page token when given.
fn jobs(jobs: &[String], next: Option<&str>) -> String {
    let next = next
        .map(|t| format!(r#","nextPageToken":"{t}""#))
        .unwrap_or_default();
    format!(r#"{{"jobs":[{}]{next}}}"#, jobs.join(","))
}

/// A time after any check of these tests began: a job that ended then ended during it.
const LATER: u64 = 99_999_999_999_999;

/// A scratch directory of its own for each test (outside the checkout), holding the stand-ins
/// in `bin/`, the answers of the stand-in `js` and the script's `out/`.
struct Rig {
    dir: PathBuf,
    _scratch: js_testkit::dir::TestDir,
}

/// What one run of the script did.
#[derive(Debug)]
struct Ran {
    status: i32,
    stdout: String,
    stderr: String,
    /// Each call of the stand-in `js`, its arguments joined by spaces.
    js: Vec<String>,
}

impl Ran {
    /// The stdout lines after the first `skip`.
    fn lines(&self, skip: usize) -> Vec<&str> {
        self.stdout.lines().skip(skip).collect()
    }

    /// How many times the hosts were read.
    fn host_reads(&self) -> usize {
        self.js.iter().filter(|c| c.ends_with("host list")).count()
    }
}

impl Rig {
    /// A rig whose `js` answers `before` for the first host listing, `later` for the ones after
    /// it, no job running before or after and none ended.
    fn new(name: &str, before: &str, later: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("agent-handover-check-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::create_dir_all(dir.join("jobs")).unwrap();
        install_script(&dir.join("bin/js"), FAKE_JS, "755");
        install_script(&dir.join("bin/sleep"), "#!/bin/sh\nexit 0\n", "755");
        let rig = Rig {
            dir,
            _scratch: scratch,
        };
        let none = jobs(&[], None);
        rig.set("hosts-1.json", before);
        rig.set("hosts-last.json", later);
        rig.set("running-before.json", &none);
        rig.set("running-after.json", &none);
        rig.set("ended.json", &none);
        rig
    }

    /// Replaces an answer of the stand-in `js` (`hosts-2.json`, `jobs/j1.json`...).
    fn set(&self, file: &str, text: &str) {
        fs::write(self.dir.join(file), text).unwrap();
    }

    /// Runs the script with exactly `args`.
    fn run_args(&self, args: &[String]) -> Ran {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join("tools/held/agent-handover-check/agent_handover_check.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_DIR", &self.dir)
            .output()
            .unwrap();
        let js = fs::read_to_string(self.dir.join("js.log")).unwrap_or_default();
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            js: js.lines().map(str::to_owned).collect(),
        }
    }

    /// The flags every run needs, a `--host` for each of `hosts`, then `more`.
    fn args(&self, hosts: &[&str], more: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = [
            "--server",
            "https://jobs.example:7300",
            "--token-file",
            "token",
            "--out-dir",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        args.push(self.dir.join("out").display().to_string());
        for h in hosts {
            args.push("--host".to_owned());
            args.push((*h).to_owned());
        }
        args.extend(more.iter().map(|s| (*s).to_owned()));
        args
    }

    /// Runs the script watching `hosts`, with `more` flags.
    fn run(&self, hosts: &[&str], more: &[&str]) -> Ran {
        self.run_args(&self.args(hosts, more))
    }

    /// A file the script wrote to `out/`.
    fn out(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join("out").join(name)).unwrap()
    }
}

/// The flags each call of `js` carries.
const FLAGS: &str = "--server https://jobs.example:7300 --token-file token --json";

/// Two hosts on 0.1.316.
fn old_pair() -> String {
    hosts(&[host("n1", "0.1.316", ""), host("n2", "0.1.316", "")])
}

/// Two hosts on 0.1.418.
fn new_pair() -> String {
    hosts(&[host("n1", "0.1.418", ""), host("n2", "0.1.418", "")])
}

/// Asserts that a roll which hands every job over passes: the jobs that ran before each host
/// moved (over two pages of the listing) run on it still in the same attempt, or ended
/// succeeded, and every host moved, one reading after the other. A `--ca-cert` reaches every
/// call, every call is a reading (`host list`, `job list`, `job get`), a job of a host not
/// watched is not asked about, and a job that ended lost before the check began, or on a host
/// not watched, does not count. It catches a check that passes a roll it did not watch to its
/// end, follows only the first page, or counts a job that ended before the check began.
#[test]
fn a_roll_that_hands_every_job_over_passes() {
    let rig = Rig::new("pass", &old_pair(), &new_pair());
    rig.set(
        "hosts-2.json",
        &hosts(&[host("n1", "0.1.418", ""), host("n2", "0.1.316", "")]),
    );
    rig.set(
        "running-before.json",
        &jobs(
            &[running("j1", "n1", 1), running("j9", "elsewhere", 1)],
            Some("p2"),
        ),
    );
    rig.set(
        "running-before-p2.json",
        &jobs(&[running("j2", "n2", 0), running("j3", "n2", 2)], None),
    );
    rig.set(
        "running-after.json",
        &jobs(
            &[
                running("j1", "n1", 1),
                running("j2", "n2", 1),
                running("j5", "n2", 1),
            ],
            None,
        ),
    );
    rig.set(
        "jobs/j3.json",
        &format!(
            r#"{{"job":{}}}"#,
            ended("j3", "n2", "JOB_STATE_SUCCEEDED", LATER)
        ),
    );
    rig.set(
        "ended.json",
        &jobs(
            &[
                ended("old", "n1", "JOB_STATE_LOST", 5),
                ended("j9", "elsewhere", "JOB_STATE_LOST", LATER),
            ],
            None,
        ),
    );
    let ran = rig.run(&["n1", "n2"], &["--ca-cert", "ca.pem"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    let readings = format!(
        "readings of the hosts: 2 (in {})",
        rig.dir.join("out/timeline.tsv").display()
    );
    assert_eq!(
        ran.lines(0),
        [
            "n1: agent 0.1.316; running before: j1",
            "n2: agent 0.1.316; running before: j2 j3",
            readings.as_str(),
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: moved 0.1.316 -> 0.1.418",
            "n2: running before: 2; kept: 1; ended succeeded or cancelled: 1; running after: j2 j5",
            "PASS: every host moved and handed its running jobs over: none cordoned, tainted, lost, failed or started again",
        ],
        "{ran:?}"
    );
    let flags = format!("{FLAGS} --ca-cert ca.pem");
    assert_eq!(
        ran.js,
        [
            format!("{flags} host list"),
            format!("{flags} job list --states running --page-size 500"),
            format!("{flags} job list --states running --page-size 500 --page-token p2"),
            format!("{flags} host list"),
            format!("{flags} host list"),
            format!("{flags} job list --states running --page-size 500"),
            format!("{flags} job get --job-id j3"),
            format!(
                "{flags} job list --states lost --states failed --states failed-to-start --page-size 500"
            ),
        ]
    );
    assert_eq!(
        rig.out("outcomes.tsv"),
        "n1\tj1\tkept\nn2\tj2\tkept\nn2\tj3\tended\n"
    );
    assert_eq!(
        rig.out("timeline.tsv"),
        "reading\thost\tversion\tcordon\tmark\tstate\ttaint\n\
         1\tn1\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n\
         1\tn2\t0.1.316\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n\
         2\tn1\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n\
         2\tn2\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n"
    );
    assert_eq!(rig.out("jobs-ended-badly.jsonl"), "");
}

/// Asserts that a host seen cordoned while it moved fails the check and is named as having
/// drained, though it moved and kept its job; a mark it showed is reported and does not fail
/// it. It catches a check that reads the hosts only before and after the roll, and so passes an
/// update that drained.
#[test]
fn a_host_that_drains_fails_and_is_named() {
    let rig = Rig::new("drain", &old_pair(), &new_pair());
    rig.set(
        "hosts-2.json",
        &hosts(&[
            host("n1", "0.1.418", ""),
            host(
                "n2",
                "0.1.316",
                r#","cordonExpiresAtMs":"9","mark":"HOST_MARK_RESERVED""#,
            ),
        ]),
    );
    rig.set(
        "running-before.json",
        &jobs(&[running("j1", "n1", 1), running("j2", "n2", 1)], None),
    );
    rig.set(
        "running-after.json",
        &jobs(&[running("j1", "n1", 1), running("j2", "n2", 1)], None),
    );
    let ran = rig.run(&["n1", "n2"], &[]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.lines(3),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: moved 0.1.316 -> 0.1.418",
            "n2: FAIL: cordoned at reading 1: it drained instead of handing over",
            "n2: marks seen: HOST_MARK_RESERVED",
            "n2: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j2",
        ],
        "{ran:?}"
    );
}

/// Asserts that each way a job can fail to survive is named against its host: a job running
/// again in a later attempt (`started-again`), one on another host (`started-again`), one that
/// ended lost, failed or failed to start, one the service cannot answer for (`unreadable`), one
/// that is placed again in the same attempt (`not-running:JOB_STATE_PLACED`); and that a job
/// which ended lost during the check on a watched host, though it was not running before, is
/// listed. It catches a check that counts a job by id alone, so passes one started again, or
/// one that passes a roll it could not read a job of.
#[test]
fn a_job_that_did_not_survive_fails_the_check() {
    let rig = Rig::new(
        "jobs",
        &hosts(&[host("n1", "0.1.316", "")]),
        &hosts(&[host("n1", "0.1.418", "")]),
    );
    let ids = ["a", "b", "c", "d", "e", "f", "g"];
    let before: Vec<String> = ids.iter().map(|id| running(id, "n1", 1)).collect();
    rig.set("running-before.json", &jobs(&before, None));
    rig.set("running-after.json", &jobs(&[running("a", "n1", 2)], None));
    let job = |id: &str, host: &str, state: &str, attempt: u32| {
        format!(
            r#"{{"job":{{"id":"{id}","hostId":"{host}","state":"{state}","attempt":{attempt}}}}}"#
        )
    };
    rig.set("jobs/a.json", &job("a", "n1", "JOB_STATE_RUNNING", 2));
    rig.set("jobs/b.json", &job("b", "n2", "JOB_STATE_RUNNING", 1));
    rig.set("jobs/c.json", &job("c", "n1", "JOB_STATE_LOST", 1));
    rig.set("jobs/d.json", &job("d", "n1", "JOB_STATE_FAILED", 1));
    rig.set(
        "jobs/e.json",
        &job("e", "n1", "JOB_STATE_FAILED_TO_START", 1),
    );
    rig.set("jobs/g.json", &job("g", "n1", "JOB_STATE_PLACED", 1));
    rig.set(
        "ended.json",
        &jobs(
            &[ended("z", "n1", "JOB_STATE_FAILED_TO_START", LATER)],
            None,
        ),
    );
    let ran = rig.run(&["n1"], &[]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.lines(2),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 7; kept: 0; ended succeeded or cancelled: 0; running after: a",
            "n1: FAIL: job a started-again",
            "n1: FAIL: job b started-again",
            "n1: FAIL: job c lost",
            "n1: FAIL: job d failed",
            "n1: FAIL: job e failed-to-start",
            "n1: FAIL: job f unreadable",
            "n1: FAIL: job g not-running:JOB_STATE_PLACED",
            "FAIL: jobs of these hosts that ended lost, failed or failed to start since the check began:",
            r#"  {"id":"z","host":"n1","state":"JOB_STATE_FAILED_TO_START"}"#,
        ],
        "{ran:?}"
    );
}

/// Asserts that a host that never moves fails the check once the readings it is given are spent:
/// exactly `--samples` readings after the first, a reading the service did not answer recorded
/// as unreadable and the watch going on, and the host named with what it was last listed as
/// (here `absent`: it dropped out of the listing, which also reads as not live). It catches a
/// check that waits forever, stops at the first unreadable reading, or passes a roll that did
/// not reach a host.
#[test]
fn a_host_that_does_not_move_fails_after_the_samples() {
    let rig = Rig::new("stuck", &old_pair(), &old_pair());
    rig.set("hosts-2.json", &old_pair());
    rig.set("hosts-3.json", "FAIL");
    rig.set("hosts-4.json", &hosts(&[host("n1", "0.1.418", "")]));
    rig.set(
        "running-before.json",
        &jobs(&[running("j1", "n1", 1), running("j2", "n2", 1)], None),
    );
    rig.set(
        "running-after.json",
        &jobs(&[running("j1", "n1", 1), running("j2", "n2", 1)], None),
    );
    let ran = rig.run(&["n1", "n2"], &["--samples", "3", "--interval", "5"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.host_reads(), 4, "{ran:?}");
    assert_eq!(
        ran.lines(3),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: FAIL: did not move from 0.1.316 (last listed: absent)",
            "n2: not heard as live in 1 reading(s)",
            "n2: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j2",
        ],
        "{ran:?}"
    );
    let timeline = rig.out("timeline.tsv");
    let rows: Vec<&str> = timeline.lines().skip(1).collect();
    assert_eq!(
        rows,
        [
            "1\tn1\t0.1.316\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-",
            "1\tn2\t0.1.316\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-",
            "2\t*\tunreadable\t-\t-\t-\t-",
            "3\tn1\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-",
            "3\tn2\tabsent",
        ]
    );
}

/// Asserts that a host seen tainted while it moved fails the check, with the reading and the
/// reason. It catches a check that passes an agent that came up on a host it found unclean.
#[test]
fn a_host_seen_tainted_fails() {
    let rig = Rig::new(
        "taint",
        &hosts(&[host("n1", "0.1.316", "")]),
        &hosts(&[host("n1", "0.1.418", r#","taintReason":"residue left""#)]),
    );
    rig.set(
        "running-before.json",
        &jobs(&[running("j1", "n1", 1)], None),
    );
    rig.set("running-after.json", &jobs(&[running("j1", "n1", 1)], None));
    let ran = rig.run(&["n1"], &[]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.lines(2),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: FAIL: tainted at reading 1 (residue left)",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
        ],
        "{ran:?}"
    );
}

/// Asserts that a roll in which every host moved and lost nothing, but one host ran no job
/// before, ends inconclusive with status 3, not PASS. It catches a check that calls a hand-over
/// shown on a host that had nothing to hand over.
#[test]
fn a_host_with_no_job_before_is_inconclusive() {
    let rig = Rig::new("idle", &old_pair(), &new_pair());
    rig.set(
        "running-before.json",
        &jobs(&[running("j1", "n1", 1)], None),
    );
    rig.set("running-after.json", &jobs(&[running("j1", "n1", 1)], None));
    let ran = rig.run(&["n1", "n2"], &[]);
    assert_eq!(ran.status, 3, "{ran:?}");
    let readings = format!(
        "readings of the hosts: 1 (in {})",
        rig.dir.join("out/timeline.tsv").display()
    );
    assert_eq!(
        ran.lines(0),
        [
            "n1: agent 0.1.316; running before: j1",
            "n2: agent 0.1.316; running before: none",
            readings.as_str(),
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: moved 0.1.316 -> 0.1.418",
            "n2: running before: 0; kept: 0; ended succeeded or cancelled: 0; running after: none",
            "INCONCLUSIVE: every host moved and lost nothing, but a host ran no job before, so its hand-over of running jobs was not shown",
        ],
        "{ran:?}"
    );
}

/// Asserts each refusal: exit 2, the reason on stderr, and no reading of the hosts during a
/// roll (at most the first `host list` and running jobs). The bad flags are refused before
/// `js` runs; a host the service does not list, one with no agent version (a server that lost
/// it cannot tell whether it moved), one cordoned already, and a first reading that fails are
/// refused after the first reading. It catches a check that watches a roll it cannot judge.
#[test]
fn what_cannot_be_judged_is_refused() {
    let listed = hosts(&[
        host("n1", "0.1.316", ""),
        host("n2", "", ""),
        host("n3", "0.1.316", r#","cordonExpiresAtMs":"9""#),
    ]);
    let listed = listed.as_str();
    let say = "agent_handover_check.sh: ";
    let cases: [(&str, Vec<&str>, &str, String); 10] = [
        ("none", vec![], listed, "--host is required, once per host".to_owned()),
        ("twice", vec!["n1", "n1"], listed, "a --host is given twice".to_owned()),
        ("badid", vec!["n 1"], listed, "--host n 1 is not a host id".to_owned()),
        (
            "samples",
            vec!["n1", "--samples", "0"],
            listed,
            "--samples is a count from 1".to_owned(),
        ),
        (
            "interval",
            vec!["n1", "--interval", "x"],
            listed,
            "--interval is a whole number of seconds from 1".to_owned(),
        ),
        (
            "value",
            vec!["n1", "--samples"],
            listed,
            "--samples needs a value".to_owned(),
        ),
        (
            "unknown",
            vec!["n4"],
            listed,
            "host n4 is not listed by the job service".to_owned(),
        ),
        (
            "noversion",
            vec!["n1", "n2"],
            listed,
            "host n2 lists no agent version, so whether it moves cannot be told; read the hosts again once its agent has said hello".to_owned(),
        ),
        (
            "cordoned",
            vec!["n3"],
            listed,
            "host n3 is cordoned already, so a drain could not be told from it; start the check once the cordon is gone".to_owned(),
        ),
        ("unread", vec!["n1"], "FAIL", "the hosts cannot be read".to_owned()),
    ];
    for (name, args, first, said) in cases {
        let rig = Rig::new(&format!("refuse-{name}"), first, listed);
        // `--host` before each host id, the flags (`--samples`...) as they are.
        let mut named = Vec::new();
        let mut more = Vec::new();
        let mut flag = false;
        for a in args {
            if flag || a.starts_with("--") {
                flag = a.starts_with("--");
                more.push(a);
            } else {
                named.push(a);
            }
        }
        let ran = rig.run(&named, &more);
        assert_eq!(ran.status, 2, "{name}: {ran:?}");
        assert_eq!(
            ran.stderr.lines().next().unwrap(),
            format!("{say}{said}"),
            "{name}: {ran:?}"
        );
        assert!(ran.host_reads() <= 1, "{name}: {ran:?}");
        assert!(
            ran.js.iter().all(|c| !c.contains("job get")),
            "{name}: {ran:?}"
        );
    }
}
