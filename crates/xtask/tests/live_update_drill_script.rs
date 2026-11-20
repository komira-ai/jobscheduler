//! The held script `tools/held/live-update-drill/live_update_drill.sh`, run by `bash` with
//! stand-ins first on `PATH`: a `js` that answers the host and job listings from files of the
//! test (the hosts after the update once the stand-in `docker` has run), a `docker` that
//! writes the arguments it was given and exits with the status it is told, a `sleep` that
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
    /// The arguments the stand-in `docker` got, or `None` when it was not run.
    docker: Option<String>,
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
        for (file, text) in [
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
             *'host list'*) if [ -e \"$FAKE_DIR/docker.log\" ]; then cat \"$FAKE_DIR/hosts-after.json\"; else cat \"$FAKE_DIR/hosts.json\"; fi ;;\n\
             *'--states running'*) cat \"$FAKE_DIR/running.json\" ;;\n\
             *) cat \"$FAKE_DIR/ended.json\" ;;\n\
             esac\n",
            "755",
        );
        install_script(
            &dir.join("bin/docker"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$FAKE_DIR/docker.log\"\nexit \"${FAKE_STATUS:-0}\"\n",
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
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            docker: fs::read_to_string(self.dir.join("docker.log")).ok(),
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

    fn run(&self, more: &[&str]) -> Ran {
        self.run_with(&self.args(more), 0)
    }

    /// A file the script wrote to `out/`.
    fn out(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join("out").join(name)).unwrap()
    }
}

/// The update the script makes.
const UPDATE: &str = "service update --force --detach=false js-server_server\n";

/// Asserts that a dry run reads the hosts and the running jobs through the job service with
/// the flags given, says what it found and the update it would make, and never runs `docker`.
/// It catches a drill that updates the service without `--update`, and one that counts a
/// tainted host as taking jobs.
#[test]
fn a_dry_run_reads_and_changes_nothing() {
    let rig = Rig::quiet("dry");
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.docker, None);
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(
        lines[1..],
        [
            "jobs running: 2",
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
        ran.docker.as_deref(),
        Some("service update --force --detach=false jobs_server\n")
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
    assert_eq!(ran.docker.as_deref(), Some(UPDATE));
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

/// Asserts what the drill refuses, each with status 2 and `docker` never run: an update with no
/// job running (the drill needs a build under way), one with no host taking jobs, a missing
/// `--server`, `--token-file` or `--out-dir`, an out-dir that does not exist, a service name
/// that is not one, a settle count or interval that is not a positive count, and an unknown
/// argument. It catches a drill that updates a farm with nothing to lose, and one that passes
/// an operator's typo to `docker`.
#[test]
fn what_the_drill_refuses() {
    let idle = Rig::new("idle", (HOSTS, HOSTS), NONE, NONE);
    let ran = idle.run(&["--update"]);
    assert_eq!(
        (ran.status, ran.docker.clone(), ran.stderr.trim()),
        (
            2,
            None,
            "live_update_drill.sh: no job is running: start the build first"
        ),
    );
    let empty = r#"{"hosts":[]}"#;
    let down = Rig::new("down", (empty, empty), RUNNING, NONE);
    let ran = down.run(&["--update"]);
    assert_eq!((ran.status, ran.docker.clone()), (2, None), "{ran:?}");
    assert!(ran.stderr.contains("no host takes jobs"), "{}", ran.stderr);

    let rig = Rig::quiet("refused");
    let out = rig.dir.join("out").display().to_string();
    // Each with the flags every run needs (`true`) or alone.
    let refusals: [(bool, Vec<&str>, &str); 8] = [
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
            (2, None),
            "{said}: {ran:?}"
        );
        let first = ran.stderr.lines().next().unwrap_or_default();
        assert_eq!(first, format!("live_update_drill.sh: {said}"));
    }
}
