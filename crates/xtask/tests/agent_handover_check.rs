//! `cargo xtask agent-handover-check` ([`xtask::handover`]): the check run in this process with
//! a stand-in `js` that answers from the test's own listings (the n-th `host list` from the
//! n-th answer given, then the last; the running jobs as before until the hosts were read twice
//! and as after then, the page after token `p2` from the second page; a `job get` from the
//! jobs given, failing for any other; the ended jobs from their listing) and a `sleep` that
//! counts its calls. So each test pins what the check reads, that it calls nothing but
//! `host list`, `job list` and `job get`, its verdict for each way a roll can go, and what it
//! refuses. `run_js` is run on stand-in programs, and the verb on the built binary, for its
//! exit statuses. Nothing reaches a server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use js_testkit::dir::TestDir;
use js_testkit::spawn::install_script;
use xtask::handover::{Args, Error, check, run_js};

/// A scratch directory of the test's own.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-handover-check"),
        name,
    )
}

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

/// A job `id` on `host` that ended in `state` at `ended`, a JSON value (a string, as proto JSON
/// prints a uint64, or a number).
fn ended(id: &str, host: &str, state: &str, ended: &str) -> String {
    format!(
        r#"{{"id":"{id}","hostId":"{host}","state":"{state}","times":{{"endedAtMs":{ended}}}}}"#
    )
}

/// `job get`'s answer for `job`.
fn got(job: &str) -> String {
    format!(r#"{{"job":{job}}}"#)
}

/// A job listing of `jobs`, with `next` as its next page token when given.
fn jobs(jobs: &[String], next: Option<&str>) -> String {
    let next = next
        .map(|t| format!(r#","nextPageToken":"{t}""#))
        .unwrap_or_default();
    format!(r#"{{"jobs":[{}]{next}}}"#, jobs.join(","))
}

/// The time the check began in these tests.
const NOW: u64 = 1_000_000;

/// The answer standing for a call that fails.
const FAIL: &str = "FAIL";

/// The stand-in `js`.
#[derive(Default)]
struct Fake {
    /// The n-th `host list` answers the n-th of these, then the last.
    hosts: Vec<String>,
    running_before: String,
    running_before_p2: String,
    running_after: String,
    ended: String,
    /// `job get` by id.
    jobs: BTreeMap<String, String>,
    /// Each call, its arguments joined by spaces.
    log: Vec<String>,
    host_reads: usize,
}

impl Fake {
    /// A stand-in whose first host listing is `before` and the ones after it `later`, with no
    /// job running before or after and none ended.
    fn new(before: &str, later: &str) -> Fake {
        let none = jobs(&[], None);
        Fake {
            hosts: vec![before.to_owned(), later.to_owned()],
            running_before: none.clone(),
            running_after: none.clone(),
            ended: none,
            ..Fake::default()
        }
    }

    /// Sets the answer of the n-th host listing (from 1), keeping the last as the one after.
    fn reading(&mut self, n: usize, answer: &str) {
        let last = self.hosts.pop().unwrap_or_default();
        while self.hosts.len() < n - 1 {
            self.hosts.push(last.clone());
        }
        self.hosts.insert(n - 1, answer.to_owned());
        self.hosts.push(last);
    }

    fn answer(&mut self, args: &[String]) -> Result<String, String> {
        let call = args.join(" ");
        self.log.push(call.clone());
        let text = if call.ends_with("host list") {
            self.host_reads += 1;
            let i = (self.host_reads - 1).min(self.hosts.len() - 1);
            self.hosts[i].clone()
        } else if call.contains("--page-token p2") {
            self.running_before_p2.clone()
        } else if call.contains("--states running ") {
            if self.host_reads <= 1 {
                self.running_before.clone()
            } else {
                self.running_after.clone()
            }
        } else if let Some(id) = call.split("--job-id ").nth(1) {
            self.jobs
                .get(id)
                .cloned()
                .unwrap_or_else(|| FAIL.to_owned())
        } else {
            self.ended.clone()
        };
        if text == FAIL {
            Err("refused".to_owned())
        } else {
            Ok(text)
        }
    }
}

/// What one run of the check did.
#[derive(Debug)]
struct Ran {
    result: Result<String, Error>,
    stdout: String,
    js: Vec<String>,
    sleeps: Vec<Duration>,
}

impl Ran {
    /// The stdout lines after the first `skip`.
    fn lines(&self, skip: usize) -> Vec<&str> {
        self.stdout.lines().skip(skip).collect()
    }
}

/// The flags of a run watching `hosts` with its out-dir `dir`.
fn args(dir: &Path, hosts: &[&str]) -> Args {
    Args {
        server: "https://jobs.example:7300".to_owned(),
        token_file: PathBuf::from("token"),
        ca_cert: None,
        out_dir: dir.to_path_buf(),
        hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
        samples: 360,
        interval_secs: 20,
        js: PathBuf::from("js"),
    }
}

/// Runs the check with `args` against `fake`.
fn run(args: &Args, fake: &mut Fake) -> Ran {
    let mut sleeps = Vec::new();
    let mut out = Vec::new();
    let result = {
        let mut js = |a: &[String]| fake.answer(a);
        let mut sleep = |d: Duration| sleeps.push(d);
        check(args, &mut js, &mut sleep, NOW, &mut out)
    };
    Ran {
        result,
        stdout: String::from_utf8(out).unwrap(),
        js: fake.log.clone(),
        sleeps,
    }
}

/// A file the check wrote to `dir`.
fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
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
/// succeeded or cancelled, and every host moved, one reading after the other with one
/// `--interval-secs` sleep between. A `--ca-cert` reaches every call, every call is a reading,
/// a job of a host not watched is not asked about, and a job that ended lost before the check
/// began, or on a host not watched, does not count. It catches a check that passes a roll it
/// did not watch to its end, follows only the first page, or counts a job that ended before it
/// began.
#[test]
fn a_roll_that_hands_every_job_over_passes() {
    let dir = scratch("pass");
    let mut fake = Fake::new(&old_pair(), &new_pair());
    fake.reading(
        2,
        &hosts(&[host("n1", "0.1.418", ""), host("n2", "0.1.316", "")]),
    );
    fake.running_before = jobs(
        &[running("j1", "n1", 1), running("j9", "elsewhere", 1)],
        Some("p2"),
    );
    fake.running_before_p2 = jobs(
        &[
            running("j2", "n2", 0),
            running("j3", "n2", 2),
            running("j4", "n2", 1),
        ],
        None,
    );
    fake.running_after = jobs(
        &[
            running("j1", "n1", 1),
            running("j2", "n2", 1),
            running("j5", "n2", 1),
        ],
        None,
    );
    let j3 = ended("j3", "n2", "JOB_STATE_SUCCEEDED", r#""2000000""#);
    fake.jobs.insert("j3".to_owned(), got(&j3));
    let j4 = ended("j4", "n2", "JOB_STATE_CANCELLED", "2000000");
    fake.jobs.insert("j4".to_owned(), got(&j4));
    fake.ended = jobs(
        &[
            ended("old", "n1", "JOB_STATE_LOST", r#""5""#),
            ended("j9", "elsewhere", "JOB_STATE_LOST", r#""2000000""#),
        ],
        None,
    );
    let mut a = args(dir.path(), &["n1", "n2"]);
    a.ca_cert = Some(PathBuf::from("ca.pem"));
    let ran = run(&a, &mut fake);
    assert_eq!(
        ran.result,
        Ok("PASS: every host moved and handed its running jobs over: none cordoned, tainted, lost, failed or started again".to_owned())
    );
    let readings = format!(
        "readings of the hosts: 2 (in {})",
        dir.path().join("timeline.tsv").display()
    );
    assert_eq!(
        ran.lines(0),
        [
            "n1: agent 0.1.316; running before: j1",
            "n2: agent 0.1.316; running before: j2 j3 j4",
            readings.as_str(),
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: moved 0.1.316 -> 0.1.418",
            "n2: running before: 3; kept: 1; ended succeeded or cancelled: 2; running after: j2 j5",
        ]
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
            format!("{flags} job get --job-id j4"),
            format!(
                "{flags} job list --states lost --states failed --states failed-to-start --page-size 500"
            ),
        ]
    );
    assert_eq!(ran.sleeps, [Duration::from_secs(20)]);
    assert_eq!(read(dir.path(), "hosts-before.json"), old_pair());
    assert_eq!(
        read(dir.path(), "outcomes.tsv"),
        "n1\tj1\tkept\nn2\tj2\tkept\nn2\tj3\tended\nn2\tj4\tended\n"
    );
    assert_eq!(
        read(dir.path(), "timeline.tsv"),
        "reading\thost\tversion\tcordon\tmark\tstate\ttaint\n\
         1\tn1\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n\
         1\tn2\t0.1.316\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n\
         2\tn1\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n\
         2\tn2\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-\n"
    );
    assert_eq!(read(dir.path(), "jobs-ended-badly.jsonl"), "");
}

/// Asserts that a host seen cordoned while it moved fails the check and is named as having
/// drained, though it moved and kept its job; a mark it showed is reported and does not fail
/// it. It catches a check that reads the hosts only before and after the roll, and so passes
/// an update that drained.
#[test]
fn a_host_that_drains_fails_and_is_named() {
    let dir = scratch("drain");
    let mut fake = Fake::new(&old_pair(), &new_pair());
    fake.reading(
        2,
        &hosts(&[
            host("n1", "0.1.418", ""),
            host(
                "n2",
                "0.1.316",
                r#","cordonExpiresAtMs":"9","mark":"HOST_MARK_RESERVED""#,
            ),
        ]),
    );
    let both = jobs(&[running("j1", "n1", 1), running("j2", "n2", 1)], None);
    fake.running_before = both.clone();
    fake.running_after = both;
    let ran = run(&args(dir.path(), &["n1", "n2"]), &mut fake);
    assert_eq!(ran.result, Err(Error::Failed(1)));
    assert_eq!(
        ran.result.as_ref().unwrap_err().to_string(),
        "FAIL: 1 finding(s), each named above"
    );
    assert_eq!(
        ran.lines(3),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: moved 0.1.316 -> 0.1.418",
            "n2: FAIL: cordoned at reading 1: it drained instead of handing over",
            "n2: marks seen: HOST_MARK_RESERVED",
            "n2: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j2",
        ]
    );
    assert!(
        read(dir.path(), "timeline.tsv")
            .contains("1\tn2\t0.1.316\tcordoned\tHOST_MARK_RESERVED\tHOST_STATE_LIVE\t-\n")
    );
}

/// Asserts that each way a job can fail to survive is named against its host: running again in
/// a later attempt, or on another host though it is still listed running under its id
/// (`started-again`); ended lost, failed or failed to start; one the service cannot answer for,
/// or answers with no job or with text that is not JSON (`unreadable`); one placed again in the
/// same attempt (`not-running:JOB_STATE_PLACED`) or in a state the answer leaves out
/// (`not-running:-`); and that a job which ended lost during the check on a watched host,
/// though it was not running before, is listed, its end time given as a number or a string. It
/// catches a check that counts a job by id alone, so passes one started again, or passes a roll
/// it could not read a job of.
#[test]
fn a_job_that_did_not_survive_fails_the_check() {
    let dir = scratch("jobs");
    let mut fake = Fake::new(
        &hosts(&[host("n1", "0.1.316", "")]),
        &hosts(&[host("n1", "0.1.418", "")]),
    );
    let ids = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "k"];
    let before: Vec<String> = ids.iter().map(|id| running(id, "n1", 1)).collect();
    fake.running_before = jobs(&before, None);
    fake.running_after = jobs(&[running("a", "n1", 2), running("b", "n2", 1)], None);
    let job = |id: &str, host: &str, state: &str, attempt: u32| {
        got(&format!(
            r#"{{"id":"{id}","hostId":"{host}","state":"{state}","attempt":{attempt}}}"#
        ))
    };
    for (id, answer) in [
        ("a", job("a", "n1", "JOB_STATE_RUNNING", 2)),
        ("b", job("b", "n2", "JOB_STATE_RUNNING", 1)),
        ("c", job("c", "n1", "JOB_STATE_LOST", 1)),
        ("d", job("d", "n1", "JOB_STATE_FAILED", 1)),
        ("e", job("e", "n1", "JOB_STATE_FAILED_TO_START", 1)),
        ("g", job("g", "n1", "JOB_STATE_PLACED", 1)),
        (
            "h",
            r#"{"job":{"id":"h","hostId":"n1","attempt":1}}"#.to_owned(),
        ),
        ("i", "{}".to_owned()),
        ("k", "not json".to_owned()),
    ] {
        fake.jobs.insert(id.to_owned(), answer);
    }
    fake.ended = jobs(
        &[
            ended("z", "n1", "JOB_STATE_FAILED_TO_START", "2000000"),
            ended("y", "n1", "JOB_STATE_LOST", r#""2000000""#),
            ended("x", "n1", "JOB_STATE_LOST", "null"),
        ],
        None,
    );
    let ran = run(&args(dir.path(), &["n1"]), &mut fake);
    assert_eq!(ran.result, Err(Error::Failed(11)));
    assert_eq!(
        ran.lines(2),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 10; kept: 0; ended succeeded or cancelled: 0; running after: a",
            "n1: FAIL: job a started-again",
            "n1: FAIL: job b started-again",
            "n1: FAIL: job c lost",
            "n1: FAIL: job d failed",
            "n1: FAIL: job e failed-to-start",
            "n1: FAIL: job f unreadable",
            "n1: FAIL: job g not-running:JOB_STATE_PLACED",
            "n1: FAIL: job h not-running:-",
            "n1: FAIL: job i unreadable",
            "n1: FAIL: job k unreadable",
            "FAIL: jobs of these hosts that ended lost, failed or failed to start since the check began:",
            r#"  {"host":"n1","id":"z","state":"JOB_STATE_FAILED_TO_START"}"#,
            r#"  {"host":"n1","id":"y","state":"JOB_STATE_LOST"}"#,
        ]
    );
    assert_eq!(
        read(dir.path(), "jobs-ended-badly.jsonl"),
        "{\"host\":\"n1\",\"id\":\"z\",\"state\":\"JOB_STATE_FAILED_TO_START\"}\n\
         {\"host\":\"n1\",\"id\":\"y\",\"state\":\"JOB_STATE_LOST\"}\n"
    );
}

/// Asserts that a host that never moves fails the check once the readings it is given are
/// spent: exactly `--samples` readings after the first and one sleep fewer, a reading the
/// service did not answer (or answered with text that is not JSON) recorded as unreadable and
/// the watch going on, a reading in which the host is not live or not listed counted, and the
/// host named with what it was last listed as (here `absent`). A host listed with no version
/// mid-roll has not moved yet. It catches a check that waits forever, stops at the first
/// unreadable reading, or passes a roll that did not reach a host.
#[test]
fn a_host_that_does_not_move_fails_after_the_samples() {
    let dir = scratch("stuck");
    let mut fake = Fake::new(&old_pair(), &old_pair());
    fake.reading(
        2,
        &hosts(&[
            host("n1", "", ""),
            r#"{"id":"n2","state":"HOST_STATE_LOST","agentVersion":"0.1.316"}"#.to_owned(),
        ]),
    );
    fake.reading(3, FAIL);
    fake.reading(4, "not json");
    fake.reading(5, &hosts(&[host("n1", "0.1.418", "")]));
    let both = jobs(&[running("j1", "n1", 1), running("j2", "n2", 1)], None);
    fake.running_before = both.clone();
    fake.running_after = both;
    let mut a = args(dir.path(), &["n1", "n2"]);
    a.samples = 4;
    a.interval_secs = 5;
    let ran = run(&a, &mut fake);
    assert_eq!(ran.result, Err(Error::Failed(1)));
    assert_eq!(ran.sleeps, [Duration::from_secs(5); 3]);
    let host_reads = ran.js.iter().filter(|c| c.ends_with("host list")).count();
    assert_eq!(host_reads, 5);
    assert_eq!(
        ran.lines(3),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
            "n2: FAIL: did not move from 0.1.316 (last listed: absent)",
            "n2: not heard as live in 2 reading(s)",
            "n2: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j2",
        ]
    );
    let timeline = read(dir.path(), "timeline.tsv");
    let rows: Vec<&str> = timeline.lines().skip(1).collect();
    assert_eq!(
        rows,
        [
            "1\tn1\t-\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-",
            "1\tn2\t0.1.316\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LOST\t-",
            "2\t*\tunreadable\t-\t-\t-\t-",
            "3\t*\tunreadable\t-\t-\t-\t-",
            "4\tn1\t0.1.418\t-\tHOST_MARK_UNSPECIFIED\tHOST_STATE_LIVE\t-",
            "4\tn2\tabsent",
        ]
    );
}

/// Asserts that a host never read during the roll fails as `unread`: one reading, unanswered,
/// and no sleep after the last reading. It catches a check that calls a host it never read
/// moved.
#[test]
fn a_host_never_read_during_the_roll_fails() {
    let dir = scratch("unread");
    let mut fake = Fake::new(&hosts(&[host("n1", "0.1.316", "")]), FAIL);
    let mut a = args(dir.path(), &["n1"]);
    a.samples = 1;
    let ran = run(&a, &mut fake);
    assert_eq!(ran.result, Err(Error::Failed(1)));
    assert!(ran.sleeps.is_empty());
    assert_eq!(
        ran.lines(2)[0],
        "n1: FAIL: did not move from 0.1.316 (last listed: unread)"
    );
}

/// Asserts that a host seen tainted while it moved fails the check, with the first reading it
/// was tainted in and that reading's reason, not a later one. It catches a check that passes an
/// agent that came up on a host it found unclean.
#[test]
fn a_host_seen_tainted_fails() {
    let dir = scratch("taint");
    let mut fake = Fake::new(
        &hosts(&[host("n1", "0.1.316", "")]),
        &hosts(&[host("n1", "0.1.418", r#","taintReason":"later""#)]),
    );
    fake.reading(
        2,
        &hosts(&[host("n1", "0.1.316", r#","taintReason":"residue left""#)]),
    );
    let one = jobs(&[running("j1", "n1", 1)], None);
    fake.running_before = one.clone();
    fake.running_after = one;
    let ran = run(&args(dir.path(), &["n1"]), &mut fake);
    assert_eq!(ran.result, Err(Error::Failed(1)));
    assert_eq!(
        ran.lines(2),
        [
            "n1: moved 0.1.316 -> 0.1.418",
            "n1: FAIL: tainted at reading 1 (residue left)",
            "n1: running before: 1; kept: 1; ended succeeded or cancelled: 0; running after: j1",
        ]
    );
}

/// Asserts that a roll in which every host moved and lost nothing, but one host ran no job
/// before, is inconclusive (not PASS), naming that host. It catches a check that calls a
/// hand-over shown on a host that had nothing to hand over.
#[test]
fn a_host_with_no_job_before_is_inconclusive() {
    let dir = scratch("idle");
    let mut fake = Fake::new(&old_pair(), &new_pair());
    let one = jobs(&[running("j1", "n1", 1)], None);
    fake.running_before = one.clone();
    fake.running_after = one;
    let ran = run(&args(dir.path(), &["n1", "n2"]), &mut fake);
    assert_eq!(ran.result, Err(Error::Inconclusive("n2".to_owned())));
    assert_eq!(
        ran.result.as_ref().unwrap_err().to_string(),
        "INCONCLUSIVE: every host moved and lost nothing, but n2 ran no job before, so its hand-over of running jobs was not shown"
    );
    let readings = format!(
        "readings of the hosts: 1 (in {})",
        dir.path().join("timeline.tsv").display()
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
        ]
    );
}

/// Asserts each refusal before the watch: the error and its message, and no reading of the
/// hosts during a roll (at most the first `host list` and the running jobs). A bad host id
/// (empty, too long, or with a character a host id has not), a host given twice and an out-dir
/// that is not a directory are usage errors before `js` runs; a first reading of the hosts or
/// of the running jobs that fails (unanswered, or not JSON), a host the service does not list,
/// one with no agent version, and one cordoned already are refused after it. It catches a
/// check that watches a roll it cannot judge.
#[test]
fn what_cannot_be_judged_is_refused() {
    let dir = scratch("refuse");
    let listed = hosts(&[
        host("n1", "0.1.316", ""),
        host("n2", "", ""),
        host("n3", "0.1.316", r#","cordonExpiresAtMs":"9""#),
    ]);
    let listed = listed.as_str();
    let long = "h".repeat(254);
    let none = jobs(&[], None);
    let none = none.as_str();
    let cases: Vec<(Vec<&str>, &str, &str, &str)> = vec![
        (vec![""], listed, none, "--host  is not a host id"),
        (vec![long.as_str()], listed, none, ""),
        (vec!["n 1"], listed, none, "--host n 1 is not a host id"),
        (vec!["n1", "n1"], listed, none, "--host n1 is given twice"),
        (vec!["n1"], FAIL, none, "the hosts cannot be read: refused"),
        (
            vec!["n1"],
            "[",
            none,
            "the hosts cannot be read: EOF while parsing a list at line 1 column 1",
        ),
        (
            vec!["n1"],
            listed,
            FAIL,
            "the jobs running cannot be read: refused",
        ),
        (
            vec!["n1"],
            listed,
            "{",
            "the jobs running cannot be read: EOF while parsing an object at line 1 column 1",
        ),
        (
            vec!["n4"],
            listed,
            none,
            "host n4 is not listed by the job service",
        ),
        (
            vec!["n1", "n2"],
            listed,
            none,
            "host n2 lists no agent version, so whether it moves cannot be told; read the hosts again once its agent has said hello",
        ),
        (
            vec!["n3"],
            listed,
            none,
            "host n3 is cordoned already, so a drain could not be told from it; start the check once the cordon is gone",
        ),
    ];
    for (named, first, running_before, said) in cases {
        let mut fake = Fake::new(first, first);
        fake.running_before = running_before.to_owned();
        let ran = run(&args(dir.path(), &named), &mut fake);
        let said = if said.is_empty() {
            format!("--host {long} is not a host id")
        } else {
            said.to_owned()
        };
        assert_eq!(
            ran.result.as_ref().unwrap_err().to_string(),
            said,
            "{named:?}"
        );
        assert!(fake.host_reads <= 1, "{named:?}: {ran:?}");
        assert!(ran.js.iter().all(|c| !c.contains("job get")), "{ran:?}");
    }
    let usage = run(
        &args(dir.path(), &["n1", "n1"]),
        &mut Fake::new(listed, listed),
    );
    assert!(matches!(usage.result, Err(Error::Usage(_))));
    assert!(usage.js.is_empty());

    let file = dir.path().join("a-file");
    std::fs::write(&file, "").unwrap();
    let ran = run(&args(&file, &["n1"]), &mut Fake::new(listed, listed));
    assert_eq!(
        ran.result,
        Err(Error::Usage(format!(
            "--out-dir {} is not a directory",
            file.display()
        )))
    );
    assert!(ran.js.is_empty());
}

/// Asserts that a reading after the roll that fails ends the check with the reason, and no
/// verdict: the running jobs after, and the jobs that ended badly. It catches a check that
/// passes a roll whose outcome it could not read.
#[test]
fn a_reading_after_the_roll_that_fails_ends_the_check() {
    let dir = scratch("after");
    let mut fake = Fake::new(&old_pair(), &new_pair());
    fake.running_after = FAIL.to_owned();
    let ran = run(&args(dir.path(), &["n1", "n2"]), &mut fake);
    assert_eq!(
        ran.result.unwrap_err().to_string(),
        "the jobs running cannot be read: refused"
    );

    let mut fake = Fake::new(&old_pair(), &new_pair());
    fake.ended = FAIL.to_owned();
    let ran = run(&args(dir.path(), &["n1", "n2"]), &mut fake);
    assert_eq!(
        ran.result.unwrap_err().to_string(),
        "the jobs lost, failed, failed-to-start cannot be read: refused"
    );
}

/// Asserts that a file of the out-dir that cannot be written, and a stdout that cannot be
/// written, end the check naming what could not be written. It catches a check that passes
/// with its evidence lost.
#[test]
fn what_cannot_be_written_ends_the_check() {
    let dir = scratch("write");
    std::fs::create_dir(dir.path().join("hosts-before.json")).unwrap();
    let ran = run(
        &args(dir.path(), &["n1"]),
        &mut Fake::new(&old_pair(), &new_pair()),
    );
    match ran.result {
        Err(Error::Write { path, .. }) => assert_eq!(path, dir.path().join("hosts-before.json")),
        other => panic!("{other:?}"),
    }

    /// A stdout that refuses every write.
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let dir = scratch("stdout");
    let mut fake = Fake::new(&old_pair(), &new_pair());
    let mut js = |a: &[String]| fake.answer(a);
    let result = check(
        &args(dir.path(), &["n1"]),
        &mut js,
        &mut |_| {},
        NOW,
        &mut Closed,
    );
    assert_eq!(
        result,
        Err(Error::Write {
            path: PathBuf::from("stdout"),
            why: "closed".to_owned()
        })
    );
}

/// Asserts that `run_js` gives a program's stdout when it exits 0 (its stdin empty), its exit
/// status and stderr when it does not, and why it could not be run when it is not there. It
/// catches a check that reads a failed `js`'s output as an answer.
#[test]
fn run_js_gives_stdout_or_why_not() {
    let dir = scratch("run-js");
    let ok = dir.path().join("ok");
    install_script(
        &ok,
        "#!/bin/sh\nread -r line || printf '%s|' \"$*\"\n",
        "755",
    );
    let args = vec!["host".to_owned(), "list".to_owned()];
    assert_eq!(run_js(&ok, &args), Ok("host list|".to_owned()));

    let bad = dir.path().join("bad");
    install_script(&bad, "#!/bin/sh\necho ' no token ' >&2\nexit 3\n", "755");
    assert_eq!(
        run_js(&bad, &args),
        Err(format!(
            "{} ended with exit status: 3: no token",
            bad.display()
        ))
    );

    let missing = dir.path().join("missing");
    let why = run_js(&missing, &args).unwrap_err();
    assert!(
        why.starts_with(&format!("{} cannot be run: ", missing.display())),
        "{why}"
    );
}

/// The built `cargo xtask` run with `args`: its status, stdout and stderr.
fn verb(args: &[String]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("agent-handover-check")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// Asserts the verb's exit statuses: 0 with the PASS line on stdout when the roll handed over,
/// 2 with the reason on stderr for a usage error, and 1 with the verdict on stderr when it did
/// not pass, through a stand-in `js` program given with `--js`. It catches a verb whose usage
/// errors exit as failures, or whose failure exits 0.
#[test]
fn the_verb_exits_by_its_verdict() {
    let dir = scratch("verb");
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let js = dir.path().join("js");
    let flag = dir.path().join("listed");
    // The first `host list` answers before, every later one after; no job runs or ended.
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n*'host list'*) if [ -f '{flag}' ]; then echo '{new}'; else touch '{flag}'; echo '{old}'; fi ;;\n*) echo '{none}' ;;\nesac\n",
        flag = flag.display(),
        new = hosts(&[host("n1", "0.1.418", "")]),
        old = hosts(&[host("n1", "0.1.316", "")]),
        none = jobs(&[], None),
    );
    install_script(&js, &script, "755");
    let base = |host: &str| -> Vec<String> {
        vec![
            "--server".to_owned(),
            "https://jobs.example:7300".to_owned(),
            "--token-file".to_owned(),
            "token".to_owned(),
            "--out-dir".to_owned(),
            out.display().to_string(),
            "--js".to_owned(),
            js.display().to_string(),
            "--host".to_owned(),
            host.to_owned(),
        ]
    };

    let (status, stdout, stderr) = verb(&base("n1"));
    assert_eq!(status, 1, "{stdout}{stderr}");
    assert_eq!(
        stderr,
        "xtask: INCONCLUSIVE: every host moved and lost nothing, but n1 ran no job before, so its hand-over of running jobs was not shown\n"
    );

    let mut twice = base("n1");
    twice.extend(["--host".to_owned(), "n1".to_owned()]);
    let (status, _, stderr) = verb(&twice);
    assert_eq!(status, 2);
    assert_eq!(stderr, "xtask: --host n1 is given twice\n");

    std::fs::remove_file(&flag).unwrap();
    let running = jobs(&[running("j1", "n1", 1)], None);
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n*'host list'*) if [ -f '{flag}' ]; then echo '{new}'; else touch '{flag}'; echo '{old}'; fi ;;\n*'--states running'*) echo '{running}' ;;\n*) echo '{none}' ;;\nesac\n",
        flag = flag.display(),
        new = hosts(&[host("n1", "0.1.418", "")]),
        old = hosts(&[host("n1", "0.1.316", "")]),
        none = jobs(&[], None),
    );
    install_script(&js, &script, "755");
    let (status, stdout, stderr) = verb(&base("n1"));
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert!(
        stdout.ends_with("PASS: every host moved and handed its running jobs over: none cordoned, tainted, lost, failed or started again\n"),
        "{stdout}"
    );
}
