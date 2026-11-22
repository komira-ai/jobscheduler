//! The held script `tools/held/live-update-drill/live_update_drill.sh`, run by `bash` with
//! stand-ins first on `PATH`: a `js` that answers the host and job listings from files of the
//! test (the hosts after the update once the stand-in `docker` has run an update), a `docker`
//! that logs each call's arguments, answers `service inspect` and `service ps` from files of
//! the test (those of before the update until it ran an update, those of after it then) and
//! exits with the status it is told for the update, a `sleep` that
//! returns at once and, for one test, a `wc` that pads its count as macOS's does. So each test
//! pins what the script would run, that a dry run is the default and changes nothing, what the
//! verdict of an update is, and what it refuses (exit 2, with `docker` never run). Nothing
//! reaches a server or a Swarm.

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

/// Two hosts that take jobs and a third that is tainted.
const HOSTS: &str = r#"{"hosts":[{"id":"h2","state":"HOST_STATE_LIVE"},{"id":"h1","state":"HOST_STATE_LIVE"},{"id":"h3","state":"HOST_STATE_LIVE","taintReason":"residue"}]}"#;

/// Two running jobs.
const RUNNING: &str = r#"{"jobs":[{"id":"job-1","state":"JOB_STATE_RUNNING"},{"id":"job-2","state":"JOB_STATE_RUNNING"}]}"#;

/// A service spec as `docker service inspect` prints it, with the fields the drill reads: the
/// replicas, the update's order, parallelism and failure action, the stop grace period in
/// nanoseconds and, when given, the state of the last update.
fn spec(replicas: u32, order: &str, parallelism: u32, action: &str, grace: u64) -> String {
    format!(
        r#"{{"Spec":{{"Mode":{{"Replicated":{{"Replicas":{replicas}}}}},"UpdateConfig":{{"Order":"{order}","Parallelism":{parallelism},"FailureAction":"{action}"}},"TaskTemplate":{{"ContainerSpec":{{"StopGracePeriod":{grace}}}}}}}}}"#
    )
}

/// The spec the drill needs: two replicas, start-first, one at a time, rollback, 40 s grace.
fn good_spec() -> String {
    spec(2, "start-first", 1, "rollback", 40_000_000_000)
}

/// The good spec after an update whose state is `state`.
fn after_spec(state: &str) -> String {
    let mut json = good_spec();
    json.pop();
    format!(r#"{json},"UpdateStatus":{{"State":"{state}"}}}}"#)
}

/// Two tasks, both running, as `docker service ps` prints them.
const TASKS_BEFORE: &str = "old1 Running 3 hours ago\nold2 Running 3 hours ago\n";

/// Two new tasks.
const TASKS_AFTER: &str = "new1 Running 5 seconds ago\nnew2 Running 2 seconds ago\n";

/// No job.
const NONE: &str = r#"{"jobs":[]}"#;

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
    /// Each call of the stand-in `docker`, its arguments joined by spaces.
    docker: Vec<String>,
    /// Each call of the stand-in `js`, its arguments joined by spaces.
    js: Vec<String>,
}

impl Rig {
    /// A rig whose `js` answers `hosts` for the hosts before the update and `after` once it
    /// ran, `running` for the running jobs and `ended` for the jobs that ended badly.
    fn new(name: &str, (hosts, after): (&str, &str), running: &str, ended: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("live-update-drill-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("out")).unwrap();
        let (svc_before, svc_after) = (good_spec(), after_spec("completed"));
        for (file, text) in [
            ("service-before.json", svc_before.as_str()),
            ("tasks-before.txt", TASKS_BEFORE),
            ("service-after.json", svc_after.as_str()),
            ("tasks-after.txt", TASKS_AFTER),
            ("hosts.json", hosts),
            ("hosts-after.json", after),
            ("running.json", running),
            ("ended.json", ended),
        ] {
            fs::write(dir.join(file), text).unwrap();
        }
        install_script(
            &dir.join("bin/js"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$FAKE_DIR/js.log\"\n\
             case \"$*\" in\n\
             *'host list'*) if grep -q 'service update' \"$FAKE_DIR/docker.log\" 2>/dev/null; then cat \"$FAKE_DIR/hosts-after.json\"; else cat \"$FAKE_DIR/hosts.json\"; fi ;;\n\
             *'--states running'*) cat \"$FAKE_DIR/running.json\" ;;\n\
             *) cat \"$FAKE_DIR/ended.json\" ;;\n\
             esac\n",
            "755",
        );
        install_script(
            &dir.join("bin/docker"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$FAKE_DIR/docker.log\"\n\
             if grep -q 'service update' \"$FAKE_DIR/docker.log\"; then phase=after; else phase=before; fi\n\
             case \"$*\" in\n\
             'service inspect'*) cat \"$FAKE_DIR/service-$phase.json\" ;;\n\
             'service ps'*) cat \"$FAKE_DIR/tasks-$phase.txt\" ;;\n\
             'service update'*) exit \"${FAKE_STATUS:-0}\" ;;\n\
             esac\n",
            "755",
        );
        install_script(&dir.join("bin/sleep"), "#!/bin/sh\nexit 0\n", "755");
        Rig {
            dir,
            _scratch: scratch,
        }
    }

    /// A rig whose hosts are the same before and after, with two jobs running and none ended.
    fn quiet(name: &str) -> Rig {
        Rig::new(name, (HOSTS, HOSTS), RUNNING, NONE)
    }

    /// The flags every run needs, then `more`.
    fn args<'a>(&'a self, more: &[&'a str]) -> Vec<String> {
        let out = self.dir.join("out").display().to_string();
        let base = [
            "--server",
            "https://jobs.example:7300",
            "--token-file",
            "token",
            "--out-dir",
            out.as_str(),
        ];
        base.iter().chain(more).map(|s| (*s).to_owned()).collect()
    }

    /// Runs the script with `args`; the stand-in `docker` exits `status`.
    fn run_with(&self, args: &[String], status: i32) -> Ran {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join("tools/held/live-update-drill/live_update_drill.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_DIR", &self.dir)
            .env("FAKE_STATUS", status.to_string())
            .output()
            .unwrap();
        let js = fs::read_to_string(self.dir.join("js.log")).unwrap_or_default();
        let docker = fs::read_to_string(self.dir.join("docker.log")).unwrap_or_default();
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            docker: docker.lines().map(str::to_owned).collect(),
            js: js.lines().map(str::to_owned).collect(),
        }
    }

    /// Puts a `wc` first on `PATH` that counts the lines of its input and pads the count to
    /// eight columns, as `wc -l` on macOS does.
    fn pad_wc(&self) {
        install_script(
            &self.dir.join("bin/wc"),
            "#!/bin/sh\nn=0\nwhile IFS= read -r _; do n=$((n + 1)); done\nprintf '%8d\\n' \"$n\"\n",
            "755",
        );
    }

    /// Replaces an answer of the stand-in `docker` (`service-before.json`, `tasks-after.txt`...).
    fn set(&self, file: &str, text: &str) {
        fs::write(self.dir.join(file), text).unwrap();
    }

    fn run(&self, more: &[&str]) -> Ran {
        self.run_with(&self.args(more), 0)
    }

    /// A file the script wrote to `out/`.
    fn out(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join("out").join(name)).unwrap()
    }
}

/// The update the script makes.
const UPDATE: &str = "service update --force --detach=false js-server_server";

/// The reading of the service.
const INSPECT: &str = "service inspect --format {{json .}} js-server_server";

/// The reading of the running tasks.
const PS: &str =
    "service ps --filter desired-state=running --format {{.ID}} {{.CurrentState}} js-server_server";

/// Whether the stand-in `docker` was told to update the service.
fn updated(ran: &Ran) -> bool {
    ran.docker
        .iter()
        .any(|call| call.starts_with("service update"))
}

/// Asserts that a dry run reads the hosts and the running jobs through the job service with
/// the flags given, reads the service and its tasks, says what it found and the update it
/// would make, and runs no `docker` but those two readings.
/// It catches a drill that updates the service without `--update`, and one that counts a
/// tainted host as taking jobs.
#[test]
fn a_dry_run_reads_and_changes_nothing() {
    let rig = Rig::quiet("dry");
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.docker, [INSPECT, PS]);
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(
        lines[1..],
        [
            "jobs running: 2",
            "service replicas: 2 (ok)",
            "service update-order: start-first (ok)",
            "service update-parallelism: 1 (ok)",
            "service update-failure-action: rollback (ok)",
            "service stop-grace-period-ns: 40000000000 (ok)",
            "tasks running: 2 (want 2, all Running)",
            "update: docker service update --force --detach=false js-server_server",
            "dry run: nothing was changed; run again with --update while a build runs",
        ]
    );
    assert!(
        lines[0].starts_with("hosts taking jobs: 2 (in "),
        "{}",
        ran.stdout
    );
    assert_eq!(rig.out("hosts-before.txt"), "h1\nh2\n");
    assert_eq!(rig.out("service-before.json"), good_spec());
    assert_eq!(rig.out("tasks-before.txt"), TASKS_BEFORE);
    let flags = "--server https://jobs.example:7300 --token-file token --json";
    assert_eq!(ran.js[0], format!("{flags} host list"));
    assert_eq!(
        ran.js[1],
        format!("{flags} job list --states running --page-size 500")
    );
}

/// Asserts that the counts the script prints are bare numbers when `wc -l` pads its count
/// with spaces, as it does on macOS: a dry run prints `hosts taking jobs: 2` and
/// `jobs running: 2` exactly. It catches a drill that prints `wc`'s output as it came.
#[test]
fn a_count_that_wc_pads_is_printed_bare() {
    let rig = Rig::quiet("padded");
    rig.pad_wc();
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    let hosts = format!(
        "hosts taking jobs: 2 (in {})",
        rig.dir.join("out/hosts-before.txt").display()
    );
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines[..2], [hosts.as_str(), "jobs running: 2"], "{ran:?}");
}

/// Asserts that an update that loses nothing passes: `docker` is told to update the service
/// with `--force` and to wait for it, the hosts are read again, the jobs that ended badly are
/// read, and the verdict is PASS with status 0. A `--ca-cert` and a `--service` reach the
/// calls. It catches a drill that does not wait for the update, or that reads the wrong
/// service.
#[test]
fn an_update_that_loses_nothing_passes() {
    let rig = Rig::quiet("pass");
    let ran = rig.run_with(
        &rig.args(&[
            "--update",
            "--ca-cert",
            "ca.pem",
            "--service",
            "jobs_server",
        ]),
        0,
    );
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        ran.docker[2],
        "service update --force --detach=false jobs_server"
    );
    let last = ran.stdout.lines().last().unwrap();
    assert_eq!(
        last,
        "PASS: every host takes jobs again and no job ended lost, failed or failed to start"
    );
    let ended = ran.js.last().unwrap();
    assert!(ended.ends_with(
        "--ca-cert ca.pem job list --states lost --states failed --states failed-to-start --page-size 500"
    ), "{ended}");
    assert_eq!(rig.out("jobs-ended-badly.txt"), "");
}

/// Asserts that a host that takes jobs before the update and not after it fails the drill once
/// the readings it is given are spent: exactly `--settle-samples` readings after the update,
/// status 1, and the host named. It catches a drill that reads the hosts once and passes on a
/// host that is still on its way, or never gives up on one that does not come back.
#[test]
fn a_host_that_does_not_take_jobs_again_fails() {
    let after = r#"{"hosts":[{"id":"h2","state":"HOST_STATE_LIVE"},{"id":"h1","state":"HOST_STATE_LOST"}]}"#;
    let rig = Rig::new("missing", (HOSTS, after), RUNNING, NONE);
    let ran = rig.run(&["--update", "--settle-samples", "3"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(ran.docker[2], UPDATE);
    let tail: Vec<&str> = ran.stdout.lines().rev().take(2).collect();
    assert_eq!(
        tail,
        [
            "  h1",
            "FAIL: hosts that took jobs before the update and do not now:"
        ]
    );
    let hosts = ran.js.iter().filter(|call| call.ends_with("host list"));
    // The readings while the update runs are as many as its files: none or more, as the
    // stand-in `docker` ends before or after the first.
    let during = fs::read_dir(rig.dir.join("out")).unwrap();
    let during = during.filter(|entry| {
        let name = entry.as_ref().unwrap().file_name();
        name.to_string_lossy().starts_with("hosts-during-")
    });
    let during = during.count();
    assert_eq!(
        hosts.count(),
        1 + during + 3,
        "one before, those during, three after"
    );
}

/// Asserts that a job that ended lost, failed or failed to start after the update began fails
/// the drill with status 1 and is named by id and state, while one that ended before it is
/// not. It catches a drill that counts the jobs of an earlier run, and one that passes on a job
/// the update lost.
#[test]
fn a_job_that_ended_badly_during_the_update_fails() {
    let ended = r#"{"jobs":[{"id":"job-0","state":"JOB_STATE_FAILED","times":{"endedAtMs":"1000"}},{"id":"job-9","state":"JOB_STATE_LOST","times":{"endedAtMs":"99999999999999"}}]}"#;
    let rig = Rig::new("lost", (HOSTS, HOSTS), RUNNING, ended);
    let ran = rig.run(&["--update"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    let tail: Vec<&str> = ran.stdout.lines().rev().take(2).collect();
    assert_eq!(
        tail,
        [
            r#"  {"id":"job-9","state":"JOB_STATE_LOST"}"#,
            "FAIL: jobs that ended lost, failed or failed to start during the update:",
        ]
    );
}

/// Asserts that an update that fails (the service rolled back) fails the drill with status 1
/// and names the update's log, before anything else is judged. It catches a drill that
/// reports PASS for an update that never converged.
#[test]
fn an_update_that_fails_fails_the_drill() {
    let rig = Rig::quiet("rollback");
    let ran = rig.run_with(&rig.args(&["--update"]), 1);
    assert_eq!(ran.status, 1, "{ran:?}");
    let last = ran.stdout.lines().last().unwrap();
    assert!(
        last.starts_with("FAIL: the update ended with status 1 (see "),
        "{last}"
    );
}

/// Asserts what the drill refuses, each with status 2 and the service never updated (a bad
/// argument runs no `docker` at all): an update with no
/// job running (the drill needs a build under way), one with no host taking jobs, a missing
/// `--server`, `--token-file` or `--out-dir`, an out-dir that does not exist, a service name
/// that is not one, a replica count, settle count or interval that is not a positive count, and an unknown
/// argument. It catches a drill that updates a farm with nothing to lose, and one that passes
/// an operator's typo to `docker`.
#[test]
fn what_the_drill_refuses() {
    let idle = Rig::new("idle", (HOSTS, HOSTS), NONE, NONE);
    let ran = idle.run(&["--update"]);
    assert_eq!(
        (ran.status, updated(&ran), ran.stderr.trim()),
        (
            2,
            false,
            "live_update_drill.sh: no job is running: start the build first"
        ),
    );
    let empty = r#"{"hosts":[]}"#;
    let down = Rig::new("down", (empty, empty), RUNNING, NONE);
    let ran = down.run(&["--update"]);
    assert_eq!((ran.status, updated(&ran)), (2, false), "{ran:?}");
    assert!(ran.stderr.contains("no host takes jobs"), "{}", ran.stderr);

    let rig = Rig::quiet("refused");
    let out = rig.dir.join("out").display().to_string();
    // Each with the flags every run needs (`true`) or alone.
    let refusals: [(bool, Vec<&str>, &str); 9] = [
        (
            false,
            vec!["--token-file", "t", "--out-dir", &out],
            "--server is required",
        ),
        (
            false,
            vec!["--server", "s", "--out-dir", &out],
            "--token-file is required",
        ),
        (
            false,
            vec!["--server", "s", "--token-file", "t"],
            "--out-dir is required",
        ),
        (
            false,
            vec![
                "--server",
                "s",
                "--token-file",
                "t",
                "--out-dir",
                "no-such-dir",
            ],
            "--out-dir no-such-dir is not a directory",
        ),
        (
            true,
            vec!["--service", "a b"],
            "--service is a Swarm service name",
        ),
        (
            true,
            vec!["--replicas", "0"],
            "--replicas is a count from 1",
        ),
        (
            true,
            vec!["--settle-samples", "0"],
            "--settle-samples is a count from 1",
        ),
        (
            true,
            vec!["--interval", "x"],
            "--interval is a whole number of seconds from 1",
        ),
        (true, vec!["--force"], "unknown argument: --force"),
    ];
    for (based, args, said) in refusals {
        let args: Vec<String> = match based {
            true => rig.args(&args),
            false => args.iter().map(|s| (*s).to_owned()).collect(),
        };
        let ran = rig.run_with(&args, 0);
        assert_eq!(
            (ran.status, ran.docker.clone()),
            (2, Vec::<String>::new()),
            "{said}: {ran:?}"
        );
        let first = ran.stderr.lines().next().unwrap_or_default();
        assert_eq!(first, format!("live_update_drill.sh: {said}"));
    }
}

/// Asserts that a spec that is not the one the drill needs is reported by the dry run and
/// refused by `--update` (status 2, the service never updated), one case per field: the
/// replicas, the update's order, its parallelism, its failure action and a stop grace period
/// shorter than 40 s. The dry run names the field with `FAIL` and exits 0.
#[test]
fn a_spec_that_is_not_two_start_first_replicas_is_refused() {
    let good = 40_000_000_000;
    let cases = [
        (
            "replicas",
            spec(1, "start-first", 1, "rollback", good),
            "service replicas: 1 (want 2): FAIL",
        ),
        (
            "order",
            spec(2, "stop-first", 1, "rollback", good),
            "service update-order: stop-first (want start-first): FAIL",
        ),
        (
            "parallelism",
            spec(2, "start-first", 2, "rollback", good),
            "service update-parallelism: 2 (want 1): FAIL",
        ),
        (
            "action",
            spec(2, "start-first", 1, "pause", good),
            "service update-failure-action: pause (want rollback): FAIL",
        ),
        (
            "grace",
            spec(2, "start-first", 1, "rollback", 39_999_999_999),
            "service stop-grace-period-ns: 39999999999 (want at least 40000000000): FAIL",
        ),
        (
            "none",
            r#"{"Spec":{}}"#.to_owned(),
            "service replicas: none (want 2): FAIL",
        ),
    ];
    for (name, json, finding) in cases {
        let rig = Rig::quiet(&format!("spec-{name}"));
        rig.set("service-before.json", &json);
        let dry = rig.run(&[]);
        assert_eq!(dry.status, 0, "{name}: {dry:?}");
        assert!(
            dry.stdout.lines().any(|line| line == finding),
            "{name}: {}",
            dry.stdout
        );
        assert!(!updated(&dry), "{name}");
        let ran = rig.run(&["--update"]);
        assert_eq!(ran.status, 2, "{name}: {ran:?}");
        assert!(!updated(&ran), "{name}: {ran:?}");
        assert_eq!(
            ran.stderr.trim(),
            "live_update_drill.sh: the service is not as the drill needs it (see above)",
            "{name}"
        );
        assert!(
            ran.stdout.lines().any(|line| line == finding),
            "{name}: {}",
            ran.stdout
        );
    }
}

/// Asserts that a service that cannot be read is reported and refused: `docker service
/// inspect` printing something that is not JSON fails every finding, and `--update` exits 2
/// without updating.
#[test]
fn a_spec_that_cannot_be_read_is_refused() {
    let rig = Rig::quiet("spec-garbled");
    rig.set("service-before.json", "not json");
    let ran = rig.run(&["--update"]);
    assert_eq!(ran.status, 2, "{ran:?}");
    assert!(!updated(&ran), "{ran:?}");
    assert!(
        ran.stdout.contains("service replicas: none (want 2): FAIL"),
        "{}",
        ran.stdout
    );
}

/// Asserts that a count of running tasks other than `--replicas`, or a task that is not
/// Running yet, is reported by the dry run and refused by `--update` (status 2, the service
/// never updated); and that `--replicas 3` with three Running tasks and a spec of three
/// replicas is accepted.
#[test]
fn a_task_count_other_than_the_replicas_is_refused() {
    let cases = [
        (
            "one",
            "old1 Running 3 hours ago\n",
            "tasks running: 1 (want 2, all Running)",
        ),
        (
            "three",
            "a Running 1 hour ago\nb Running 1 hour ago\nc Running 1 hour ago\n",
            "tasks running: 3 (want 2, all Running)",
        ),
        (
            "starting",
            "old1 Running 3 hours ago\nold2 Starting 1 second ago\n",
            "tasks running: 2 (want 2, all Running)",
        ),
    ];
    for (name, tasks, line) in cases {
        let rig = Rig::quiet(&format!("tasks-{name}"));
        rig.set("tasks-before.txt", tasks);
        let dry = rig.run(&[]);
        assert_eq!(dry.status, 0, "{name}: {dry:?}");
        assert!(
            dry.stdout.lines().any(|l| l == line),
            "{name}: {}",
            dry.stdout
        );
        let ran = rig.run(&["--update"]);
        assert_eq!(ran.status, 2, "{name}: {ran:?}");
        assert!(!updated(&ran), "{name}: {ran:?}");
        assert_eq!(
            ran.stderr.trim(),
            "live_update_drill.sh: the running tasks are not 2, all Running",
            "{name}"
        );
    }
    let rig = Rig::quiet("tasks-three");
    rig.set(
        "service-before.json",
        &spec(3, "start-first", 1, "rollback", 40_000_000_000),
    );
    rig.set(
        "tasks-before.txt",
        "a Running 1 hour ago\nb Running 1 hour ago\nc Running 1 hour ago\n",
    );
    rig.set(
        "tasks-after.txt",
        "d Running 1 second ago\ne Running 1 second ago\nf Running 1 second ago\n",
    );
    let ran = rig.run(&["--update", "--replicas", "3"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(updated(&ran));
}

/// Asserts that an update that leaves an old task running fails the drill with status 1, the
/// message `FAIL: the update did not replace every task` and the kept task named; and that
/// the same happens when fewer running tasks than the replicas are left.
#[test]
fn an_update_that_leaves_an_old_task_running_fails() {
    let rig = Rig::quiet("kept");
    rig.set(
        "tasks-after.txt",
        "old2 Running 3 hours ago\nnew1 Running 2 seconds ago\n",
    );
    let ran = rig.run(&["--update"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert!(
        lines.contains(&"FAIL: the update did not replace every task"),
        "{}",
        ran.stdout
    );
    assert!(
        lines.contains(&"  old2 Running 3 hours ago"),
        "{}",
        ran.stdout
    );
    assert!(!ran.stdout.contains("PASS"), "{}", ran.stdout);
    assert_eq!(
        rig.out("tasks-after.txt"),
        "old2 Running 3 hours ago\nnew1 Running 2 seconds ago\n"
    );

    let few = Rig::quiet("few");
    few.set("tasks-after.txt", "new1 Running 2 seconds ago\n");
    let ran = few.run(&["--update"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert!(
        ran.stdout
            .contains("FAIL: the update did not replace every task"),
        "{}",
        ran.stdout
    );
}

/// Asserts that a service update that ended in any state but `completed` fails the drill with
/// status 1 and names the state, even when every task is new: `rollback_completed` is the
/// rollback of a failed start, and a missing state is not a completed update either.
#[test]
fn an_update_status_other_than_completed_fails() {
    for state in ["rollback_completed", "paused", "updating"] {
        let rig = Rig::quiet(&format!("state-{state}"));
        rig.set("service-after.json", &after_spec(state));
        let ran = rig.run(&["--update"]);
        assert_eq!(ran.status, 1, "{state}: {ran:?}");
        let want = format!(
            "FAIL: the service update ended {state}, not completed (see {})",
            rig.dir.join("out/service-after.json").display()
        );
        assert!(
            ran.stdout.lines().any(|l| l == want),
            "{state}: {}",
            ran.stdout
        );
        assert!(!ran.stdout.contains("PASS"), "{state}: {}", ran.stdout);
    }
    let rig = Rig::quiet("state-none");
    rig.set("service-after.json", &good_spec());
    let ran = rig.run(&["--update"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert!(
        ran.stdout.contains("ended none, not completed"),
        "{}",
        ran.stdout
    );
}

/// Asserts the happy path of two tasks rolled one at a time: status 0, the PASS line, and
/// `docker` saw exactly the inspect and the task listing before, the update, and the task
/// listing and the inspect after, in that order. Every reading is kept in the out-dir.
#[test]
fn two_tasks_replaced_and_a_completed_update_pass_with_the_readings_in_order() {
    let rig = Rig::quiet("order");
    let ran = rig.run(&["--update"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.docker, [INSPECT, PS, UPDATE, PS, INSPECT]);
    assert_eq!(
        ran.stdout.lines().last().unwrap(),
        "PASS: every host takes jobs again and no job ended lost, failed or failed to start"
    );
    assert_eq!(rig.out("tasks-before.txt"), TASKS_BEFORE);
    assert_eq!(rig.out("tasks-after.txt"), TASKS_AFTER);
    assert_eq!(rig.out("service-before.json"), good_spec());
    assert_eq!(rig.out("service-after.json"), after_spec("completed"));
}
