//! The held script `tools/held/submit-rollout/submit_rollout.sh`, run by `bash` with stand-ins
//! first on `PATH`: a `gh` that logs each call's arguments and answers the newest release, the
//! comparison of the commit with its tag, the tag's commit, the release run and that run's deploy
//! jobs from files of the test, and a `docker` that logs each call and answers the running
//! containers of the service and each one's version label from files of the test. So each test
//! pins what the script reads, that a dry run changes nothing, that `--apply` re-runs the failed
//! jobs of the newest release's run only when its control-plane deploy failed, and what it
//! refuses. Nothing reaches GitHub or a Swarm.

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

/// The commit the rollout is for.
const COMMIT: &str = "4ec9b675f461ff4d5a94b72e99d09f50718b0aca";

/// The commit the newest release's tag names.
const TAG_SHA: &str = "1111111111111111111111111111111111111111";

/// The id of the running js-server container.
const CONTAINER: &str = "c0ffee";

/// The deploy jobs of a release run whose control-plane deploy failed, as the script's `gh run
/// view` prints them: name, status and conclusion, separated by tabs.
const DEPLOY_FAILED: &str =
    "cp-deploy-digest\tcompleted\tsuccess\ncp-deploy / deploy js-server\tcompleted\tfailure\n";

/// A scratch directory of its own for each test (outside the checkout), holding the stand-ins
/// in `bin/`, their answers and the script's `out/`.
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
    /// Each call of the stand-in `gh`, its arguments joined by spaces.
    gh: Vec<String>,
    /// Each call of the stand-in `docker`, its arguments joined by spaces.
    docker: Vec<String>,
}

impl Rig {
    /// A rig whose newest release is `v0.1.420`, contains the commit (`ahead`) and was made by
    /// run 4242, whose control-plane deploy failed; one js-server container runs 0.1.419.
    fn new(name: &str) -> Rig {
        let scratch = js_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("submit-rollout-{name}"),
        );
        let dir = scratch.path().to_path_buf();
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("out")).unwrap();
        install_script(
            &dir.join("bin/gh"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$FAKE_DIR/gh.log\"\n\
             case \"$*\" in\n\
             'release view'*) cat \"$FAKE_DIR/tag.txt\" ;;\n\
             'api repos/'*'/compare/'*) cat \"$FAKE_DIR/compare.txt\" ;;\n\
             'api repos/'*'/commits/'*) cat \"$FAKE_DIR/tag-sha.txt\" ;;\n\
             'run list'*) c=''; next=0; for a in \"$@\"; do [ \"$next\" = 1 ] && c=$a; next=0; [ \"$a\" = --commit ] && next=1; done; cat \"$FAKE_DIR/runs-$c.txt\" 2>/dev/null; exit 0 ;;\n\
             'run view'*) cat \"$FAKE_DIR/status-$3.txt\" \"$FAKE_DIR/jobs-$3.tsv\" ;;\n\
             'run rerun'*) exit \"${FAKE_RERUN:-0}\" ;;\n\
             *) exit 9 ;;\n\
             esac\n",
            "755",
        );
        install_script(
            &dir.join("bin/docker"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$FAKE_DIR/docker.log\"\n\
             case \"$1\" in\n\
             ps) cat \"$FAKE_DIR/containers.txt\" ;;\n\
             inspect) for a in \"$@\"; do last=$a; done; cat \"$FAKE_DIR/version-$last.txt\" ;;\n\
             *) exit 9 ;;\n\
             esac\n",
            "755",
        );
        let rig = Rig {
            dir,
            _scratch: scratch,
        };
        rig.set("tag.txt", "v0.1.420\n");
        rig.set("compare.txt", "ahead\n");
        rig.set("tag-sha.txt", &format!("{TAG_SHA}\n"));
        rig.set(&format!("runs-{TAG_SHA}.txt"), "4242\n");
        rig.set("status-4242.txt", "completed\n");
        rig.set("jobs-4242.tsv", DEPLOY_FAILED);
        rig.set("containers.txt", &format!("{CONTAINER}\n"));
        rig.set(&format!("version-{CONTAINER}.txt"), "0.1.419\n");
        rig
    }

    /// Replaces an answer of a stand-in (`tag.txt`, `jobs-4242.tsv`...).
    fn set(&self, file: &str, text: &str) {
        fs::write(self.dir.join(file), text).unwrap();
    }

    /// The flags every run needs, then `more`.
    fn args<'a>(&'a self, more: &[&'a str]) -> Vec<String> {
        let out = self.dir.join("out").display().to_string();
        let base = ["--commit", COMMIT, "--out-dir", out.as_str()];
        base.iter().chain(more).map(|s| (*s).to_owned()).collect()
    }

    /// Runs the script with exactly `args`.
    fn run_with(&self, args: &[String]) -> Ran {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(root().join("tools/held/submit-rollout/submit_rollout.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_DIR", &self.dir)
            .output()
            .unwrap();
        let log = |name: &str| -> Vec<String> {
            fs::read_to_string(self.dir.join(name))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        Ran {
            status: out.status.code().unwrap(),
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            gh: log("gh.log"),
            docker: log("docker.log"),
        }
    }

    /// Runs the script with the flags every run needs, then `more`.
    fn run(&self, more: &[&str]) -> Ran {
        self.run_with(&self.args(more))
    }

    /// A file the script wrote to `out/`.
    fn out(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join("out").join(name)).unwrap()
    }
}

/// The reading of the newest release.
const RELEASE: &str = "release view --repo komira-ai/jobscheduler --json tagName --jq .tagName";

/// The reading of the running containers of the service.
const PS: &str = "ps --no-trunc --filter name=js-server_server --format {{.ID}}";

/// The re-run `--apply` makes.
const RERUN: &str = "run rerun 4242 --repo komira-ai/jobscheduler --failed";

/// Whether the stand-in `gh` was told to re-run anything.
fn reran(ran: &Ran) -> bool {
    ran.gh.iter().any(|call| call.starts_with("run rerun"))
}

/// Asserts that a dry run against a failed control-plane deploy reads, in order, the newest
/// release, whether it contains the commit, the running container and its version, the tag's
/// commit, the release run and its deploy jobs; prints the re-run it would make; keeps its
/// readings in `--out-dir`; and re-runs nothing. It catches a script that acts without
/// `--apply`, or reads another release's run.
#[test]
fn a_dry_run_names_the_re_run_and_changes_nothing() {
    let rig = Rig::new("dry");
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
    assert_eq!(
        ran.gh[..4],
        [
            RELEASE.to_owned(),
            format!("api repos/komira-ai/jobscheduler/compare/{COMMIT}...v0.1.420 --jq .status"),
            "api repos/komira-ai/jobscheduler/commits/v0.1.420 --jq .sha".to_owned(),
            format!(
                "run list --repo komira-ai/jobscheduler --workflow release.yml --commit {TAG_SHA} \
                 --event push --json databaseId --jq .[0].databaseId // empty"
            ),
        ]
    );
    assert!(
        ran.gh[4]
            .starts_with("run view 4242 --repo komira-ai/jobscheduler --json status,jobs --jq "),
        "{ran:?}"
    );
    assert_eq!(ran.gh.len(), 5, "{ran:?}");
    assert_eq!(
        ran.docker,
        [
            PS.to_owned(),
            format!(
                "inspect --format {{{{index .Config.Labels \"org.opencontainers.image.version\"}}}} {CONTAINER}"
            ),
        ]
    );
    assert_eq!(
        ran.stdout.lines().collect::<Vec<_>>(),
        [
            format!("newest release: v0.1.420 (contains {COMMIT})").as_str(),
            "running js-server_server: 0.1.419",
            "release run: 4242 (completed)",
            "job cp-deploy-digest: completed success",
            "job cp-deploy / deploy js-server: completed failure",
            "rollout: gh run rerun 4242 --repo komira-ai/jobscheduler --failed   (cp-deploy / deploy js-server failed; it re-runs every failed job of the run)",
            "dry run: nothing was changed; run again with --apply to re-run the failed jobs",
        ]
    );
    assert_eq!(rig.out("release.txt"), format!("v0.1.420 {COMMIT} ahead\n"));
    assert_eq!(rig.out("running.txt"), "0.1.419\n");
    assert_eq!(rig.out("run.tsv"), format!("completed\n{DEPLOY_FAILED}"));
}

/// Asserts that `--apply` against a failed control-plane deploy re-runs the failed jobs of that
/// run once, with `--repo` reaching every call, and says so. It catches a script that re-runs
/// every job, or starts a deploy of its own.
#[test]
fn apply_re_runs_the_failed_jobs_of_the_newest_release() {
    let rig = Rig::new("apply");
    let ran = rig.run(&["--apply", "--repo", "komira-ai/jobscheduler"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.gh.last().unwrap(), RERUN);
    assert_eq!(
        ran.gh.iter().filter(|c| c.starts_with("run rerun")).count(),
        1
    );
    assert_eq!(
        ran.stdout.lines().last().unwrap(),
        "APPLIED: the failed jobs of run 4242 run again; run this again once they end to confirm"
    );
}

/// Asserts that a re-run `gh` refuses ends the script with `gh`'s status and no APPLIED line.
/// It catches a script that reports a re-run that did not start.
#[test]
fn a_re_run_that_gh_refuses_fails() {
    let rig = Rig::new("rerun-refused");
    let path = format!(
        "{}:{}",
        rig.dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(root().join("tools/held/submit-rollout/submit_rollout.sh"))
        .args(rig.args(&["--apply"]))
        .env("PATH", path)
        .env("FAKE_DIR", &rig.dir)
        .env("FAKE_RERUN", "4")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert!(!String::from_utf8(out.stdout).unwrap().contains("APPLIED"));
}

/// Asserts that when every running container carries the release's version the script says it
/// is rolled out, exits 0 and reads no run, with or without `--apply`. It catches a script that
/// re-runs a deploy that already converged.
#[test]
fn a_release_that_runs_is_rolled_out() {
    for more in [&[][..], &["--apply"][..]] {
        let rig = Rig::new("runs");
        rig.set(&format!("version-{CONTAINER}.txt"), "0.1.420\n");
        let ran = rig.run(more);
        assert_eq!(ran.status, 0, "{ran:?}");
        assert_eq!(ran.gh.len(), 2, "{ran:?}");
        assert_eq!(
            ran.stdout.lines().last().unwrap(),
            format!("ROLLED OUT: js-server_server runs 0.1.420, which contains {COMMIT}")
        );
    }
}

/// Asserts that a release whose tag equals the commit (`identical`) counts as containing it.
#[test]
fn a_release_at_the_commit_contains_it() {
    let rig = Rig::new("identical");
    rig.set("compare.txt", "identical\n");
    rig.set(&format!("version-{CONTAINER}.txt"), "0.1.420\n");
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(ran.stdout.contains("ROLLED OUT"), "{ran:?}");
}

/// Asserts that when one of two running containers is still on the older version the rollout
/// is not done: the script goes on to the release run. It catches a script that looks at the
/// first container only.
#[test]
fn one_container_behind_is_not_rolled_out() {
    let rig = Rig::new("half");
    rig.set("containers.txt", "c1\nc2\n");
    rig.set("version-c1.txt", "0.1.420\n");
    rig.set("version-c2.txt", "0.1.419\n");
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(
        ran.stdout
            .contains("running js-server_server: 0.1.420 0.1.419"),
        "{ran:?}"
    );
    assert!(ran.stdout.contains("rollout: gh run rerun 4242"), "{ran:?}");
}

/// Asserts that when the newest release does not contain the commit (it is `behind` or
/// `diverged`) and the commit has no release run of its own, the script says a later release
/// will carry it and exits 3, having read no container and re-run nothing. It catches a script
/// that rolls out a release without the change.
#[test]
fn no_release_with_the_commit_and_no_run_of_its_own_is_not_yet() {
    for status in ["behind", "diverged"] {
        let rig = Rig::new(&format!("not-yet-{status}"));
        rig.set("compare.txt", &format!("{status}\n"));
        let ran = rig.run(&["--apply"]);
        assert_eq!(ran.status, 3, "{ran:?}");
        assert!(ran.docker.is_empty(), "{ran:?}");
        assert!(!reran(&ran), "{ran:?}");
        assert_eq!(
            ran.gh[2],
            format!(
                "run list --repo komira-ai/jobscheduler --workflow release.yml --commit {COMMIT} \
                 --event push --json databaseId --jq .[0].databaseId // empty"
            )
        );
        assert_eq!(ran.gh.len(), 3, "{ran:?}");
        assert_eq!(
            ran.stderr,
            format!(
                "submit_rollout.sh: no release contains {COMMIT} yet, and it has no release run of its own: the release of a later commit will carry it\n"
            )
        );
    }
}

/// The jobs of a release run of the commit that failed in its tests, before it published.
const OWN_FAILED: &str = "plan\tcompleted\tsuccess\nunit-tests (1)\tcompleted\tfailure\n\
                          unit-tests (2)\tcompleted\tsuccess\npublish\tcompleted\tskipped\n\
                          cp-deploy / deploy js-server\tcompleted\tskipped\n";

impl Rig {
    /// A rig whose newest release does not contain the commit, and whose commit has its own
    /// release run 777 in `status` with `jobs`.
    fn own_run(name: &str, status: &str, jobs: &str) -> Rig {
        let rig = Rig::new(name);
        rig.set("compare.txt", "behind\n");
        rig.set(&format!("runs-{COMMIT}.txt"), "777\n");
        rig.set("status-777.txt", &format!("{status}\n"));
        rig.set("jobs-777.tsv", jobs);
        rig
    }
}

/// Asserts that when the commit's own release run ended having failed before it published, a
/// dry run names the re-run of that run's failed jobs and the failed job, reads no container and
/// re-runs nothing; and `--apply` re-runs that run's failed jobs once. It catches a script that
/// waits forever for a release a failed run will never make, or re-runs without `--apply`.
#[test]
fn the_commits_failed_release_run_is_re_run() {
    let rig = Rig::own_run("own-failed", "completed", OWN_FAILED);
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
    assert!(ran.docker.is_empty(), "{ran:?}");
    assert_eq!(
        ran.stdout.lines().collect::<Vec<_>>(),
        [
            format!("newest release: v0.1.420 (does not contain {COMMIT}: behind)").as_str(),
            format!("release run of {COMMIT}: 777 (completed)").as_str(),
            "rollout: gh run rerun 777 --repo komira-ai/jobscheduler --failed   (its release run failed before publishing: unit-tests (1); it re-runs every failed job of the run)",
            "dry run: nothing was changed; run again with --apply to re-run the failed jobs",
        ]
    );
    assert_eq!(
        rig.out("commit-run.tsv"),
        format!("completed\n{OWN_FAILED}")
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(
        ran.gh.last().unwrap(),
        "run rerun 777 --repo komira-ai/jobscheduler --failed"
    );
    assert_eq!(
        ran.gh.iter().filter(|c| c.starts_with("run rerun")).count(),
        1
    );
}

/// Asserts the states of the commit's own release run the script waits for or refuses, none
/// re-run even with `--apply`: still going with a failed job (exit 3: GitHub re-runs only an ended
/// run), still going with none failed (exit 3), ended with none failed (exit 1: the run decided
/// not to release), and published although no release contains the commit (exit 1).
#[test]
fn the_commits_release_run_states_it_does_not_re_run() {
    let cases = [
        (
            "in_progress",
            OWN_FAILED.to_owned(),
            3,
            "run 777 is still in_progress: its failed jobs can be re-run once it ends; run this again then".to_owned(),
        ),
        (
            "in_progress",
            "plan\tcompleted\tsuccess\nunit-tests (1)\tin_progress\t\n".to_owned(),
            3,
            format!("no release contains {COMMIT} yet: its release run 777 is still in_progress; wait for it, then run this again"),
        ),
        (
            "completed",
            "plan\tcompleted\tsuccess\npublish\tcompleted\tskipped\n".to_owned(),
            1,
            format!("no release contains {COMMIT}, yet its release run 777 ended with no failed job: read that run's plan before anything else"),
        ),
        (
            "completed",
            "publish\tcompleted\tsuccess\ncp-deploy / deploy js-server\tcompleted\tfailure\n".to_owned(),
            1,
            format!("release run 777 published, yet the newest release does not contain {COMMIT}: read the releases before anything else"),
        ),
    ];
    for (status, jobs, exit, why) in cases {
        let rig = Rig::own_run("own-state", status, &jobs);
        let ran = rig.run(&["--apply"]);
        assert_eq!(ran.status, exit, "{jobs:?} {ran:?}");
        assert!(!reran(&ran), "{jobs:?} {ran:?}");
        assert_eq!(
            ran.stderr,
            format!("submit_rollout.sh: {why}\n"),
            "{jobs:?}"
        );
    }
}

/// Asserts that a deploy job that has not completed is waited for: exit 3 and no re-run, even
/// with `--apply`, whichever job is pending. It catches a script that re-runs a run still going.
#[test]
fn a_deploy_still_running_is_waited_for() {
    for jobs in [
        "cp-deploy-digest\tcompleted\tsuccess\ncp-deploy / deploy js-server\tin_progress\t\n",
        "cp-deploy-digest\tcompleted\tfailure\ncp-deploy / deploy js-server\tqueued\t\n",
        "cp-deploy-digest\tin_progress\t\ncp-deploy / deploy js-server\tcompleted\tfailure\n",
        "",
    ] {
        let rig = Rig::new("pending");
        rig.set("jobs-4242.tsv", jobs);
        let ran = rig.run(&["--apply"]);
        assert_eq!(ran.status, 3, "{jobs:?} {ran:?}");
        assert!(!reran(&ran), "{ran:?}");
        assert_eq!(
            ran.stderr,
            "submit_rollout.sh: the control-plane deploy of run 4242 has not ended: wait for it, then run this again\n"
        );
    }
}

/// Asserts that a deploy the run reports converged while the service still runs the older
/// version is refused (exit 1, no re-run): something else moved the service. It catches a
/// script that re-runs a deploy that passed.
#[test]
fn a_deploy_that_passed_but_did_not_land_is_refused() {
    let rig = Rig::new("passed");
    rig.set(
        "jobs-4242.tsv",
        "cp-deploy-digest\tcompleted\tsuccess\ncp-deploy / deploy js-server\tcompleted\tsuccess\n",
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
    assert_eq!(
        ran.stderr,
        "submit_rollout.sh: run 4242 reports the deploy converged, yet js-server_server runs 0.1.419, not 0.1.420: read the service (docker service ps js-server_server) before anything else\n"
    );
}

/// Asserts that a skipped control-plane deploy (the deploy is off) is refused with exit 1 and
/// no re-run: re-running a skipped job deploys nothing.
#[test]
fn a_skipped_deploy_is_refused() {
    let rig = Rig::new("skipped");
    rig.set(
        "jobs-4242.tsv",
        "cp-deploy-digest\tcompleted\tskipped\ncp-deploy / deploy js-server\tcompleted\tskipped\n",
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
    assert_eq!(
        ran.stderr,
        "submit_rollout.sh: run 4242 skipped the control-plane deploy and no job of it failed (the deploy is off: docs/release.md): nothing here deploys\n"
    );
}

/// Asserts that a deploy skipped because an earlier job of the run failed (here `publish`) is
/// re-run with `--apply`, and the failed job is named. It catches a script that calls such a
/// deploy "off" and offers no recovery.
#[test]
fn a_deploy_skipped_after_a_failed_job_is_re_run() {
    let rig = Rig::new("upstream");
    rig.set(
        "jobs-4242.tsv",
        "plan\tcompleted\tsuccess\npublish\tcompleted\tfailure\n\
         cp-deploy-digest\tcompleted\tskipped\ncp-deploy / deploy js-server\tcompleted\tskipped\n",
    );
    let ran = rig.run(&[]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert!(
        ran.stdout.contains(
            "rollout: gh run rerun 4242 --repo komira-ai/jobscheduler --failed   (the deploy, skipped after publish failed; it re-runs every failed job of the run)\n"
        ),
        "{ran:?}"
    );
    assert!(!reran(&ran), "{ran:?}");
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.gh.last().unwrap(), RERUN);
}

/// Asserts that the deploy's state does not depend on the order its jobs are listed in: a failed
/// job listed after a skipped or an unknown one still makes the deploy failed (and re-run), and a
/// job not ended still wins over a failed one listed after it. It catches a script that keeps
/// the first state it reads.
#[test]
fn the_deploy_state_does_not_depend_on_job_order() {
    for jobs in [
        "cp-deploy / deploy js-server\tcompleted\tskipped\ncp-deploy-digest\tcompleted\tfailure\n",
        "cp-deploy / deploy js-server\tcompleted\tneutral\ncp-deploy-digest\tcompleted\tstartup_failure\n",
    ] {
        let rig = Rig::new("order");
        rig.set("jobs-4242.tsv", jobs);
        let ran = rig.run(&["--apply"]);
        assert_eq!(ran.status, 0, "{jobs:?} {ran:?}");
        assert_eq!(ran.gh.last().unwrap(), RERUN, "{jobs:?}");
        assert!(
            ran.stdout.contains("(cp-deploy-digest failed;"),
            "{jobs:?} {ran:?}"
        );
    }
    let rig = Rig::new("order-pending");
    rig.set(
        "jobs-4242.tsv",
        "cp-deploy / deploy js-server\tcompleted\tfailure\ncp-deploy-digest\tin_progress\t\n",
    );
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 3, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
}

/// Asserts that a failed deploy in a run that has not ended (other jobs still run) is waited for:
/// exit 3 and no re-run, even with `--apply`, because GitHub re-runs only a finished run. It
/// catches a script that tries the re-run and fails on GitHub's refusal.
#[test]
fn a_run_still_going_is_waited_for() {
    let rig = Rig::new("run-going");
    rig.set("status-4242.txt", "in_progress\n");
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 3, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
    assert!(
        ran.stdout.contains("release run: 4242 (in_progress)\n"),
        "{ran:?}"
    );
    assert_eq!(
        ran.stderr,
        "submit_rollout.sh: run 4242 is still in_progress: its failed jobs can be re-run once it ends; run this again then\n"
    );
}

/// Asserts that a deploy ended `cancelled` or `timed_out` is re-run like a failure, and that a
/// failed job counts even when it is listed after a passed one.
#[test]
fn a_cancelled_or_timed_out_deploy_is_re_run() {
    for conclusion in ["cancelled", "timed_out"] {
        let rig = Rig::new(&format!("ended-{conclusion}"));
        rig.set(
            "jobs-4242.tsv",
            &format!(
                "cp-deploy-digest\tcompleted\tsuccess\ncp-deploy / deploy js-server\tcompleted\t{conclusion}\n"
            ),
        );
        let ran = rig.run(&["--apply"]);
        assert_eq!(ran.status, 0, "{ran:?}");
        assert_eq!(ran.gh.last().unwrap(), RERUN);
    }
}

/// Asserts that a conclusion the script does not know (`neutral`, `action_required`) is refused
/// with exit 1 and no re-run, naming the conclusion.
#[test]
fn an_unknown_conclusion_is_refused() {
    for conclusion in ["neutral", "action_required"] {
        let rig = Rig::new(&format!("unknown-{conclusion}"));
        rig.set(
            "jobs-4242.tsv",
            &format!("cp-deploy / deploy js-server\tcompleted\t{conclusion}\n"),
        );
        let ran = rig.run(&["--apply"]);
        assert_eq!(ran.status, 1, "{ran:?}");
        assert!(!reran(&ran), "{ran:?}");
        assert_eq!(
            ran.stderr,
            format!(
                "submit_rollout.sh: run 4242's deploy ended {conclusion}: not a state this script acts on\n"
            )
        );
    }
}

/// Asserts the states the script will not act on, each exit 1 with no re-run: a newest tag
/// that is not `vX.Y.Z`, no running container of the service on this node, and no release run
/// of the tag's commit.
#[test]
fn states_it_will_not_act_on() {
    let rig = Rig::new("bad-tag");
    rig.set("tag.txt", "nightly\n");
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.stderr,
        "submit_rollout.sh: the newest release's tag is not vX.Y.Z: nightly\n"
    );

    let rig = Rig::new("no-container");
    rig.set("containers.txt", "");
    let ran = rig.run(&["--apply", "--service", "jobs_server"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert_eq!(
        ran.docker,
        ["ps --no-trunc --filter name=jobs_server --format {{.ID}}"]
    );
    assert_eq!(
        ran.stderr,
        "submit_rollout.sh: no running container of jobs_server on this node: run the script on the node that runs it\n"
    );

    let rig = Rig::new("no-run");
    rig.set(&format!("runs-{TAG_SHA}.txt"), "");
    let ran = rig.run(&["--apply"]);
    assert_eq!(ran.status, 1, "{ran:?}");
    assert!(!reran(&ran), "{ran:?}");
    assert_eq!(
        ran.stderr,
        format!(
            "submit_rollout.sh: no release run of {TAG_SHA}: look at the Actions page before anything else\n"
        )
    );
}

/// Asserts that bad input is refused with exit 2 and the usage, before `gh` or `docker` runs: a
/// missing or short commit, a missing or absent out directory, a repository that is not
/// OWNER/NAME, an empty service, a flag without its value and an unknown argument.
#[test]
fn bad_input_is_refused_before_anything_runs() {
    let rig = Rig::new("usage");
    let out = rig.dir.join("out").display().to_string();
    let missing = rig.dir.join("nope").display().to_string();
    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec!["--out-dir", &out], "--commit is required".to_owned()),
        (vec!["--commit", COMMIT], "--out-dir is required".to_owned()),
        (
            vec!["--commit", "4ec9b675f", "--out-dir", &out],
            "--commit must be a full 40-hex SHA: 4ec9b675f".to_owned(),
        ),
        (
            vec!["--commit", COMMIT, "--out-dir", &missing],
            format!("--out-dir is not a directory: {missing}"),
        ),
        (
            vec![
                "--commit",
                COMMIT,
                "--out-dir",
                &out,
                "--repo",
                "jobscheduler",
            ],
            "--repo must be OWNER/NAME: jobscheduler".to_owned(),
        ),
        (
            vec!["--commit", COMMIT, "--out-dir", &out, "--service", ""],
            "--service must not be empty".to_owned(),
        ),
        (
            vec!["--commit", COMMIT, "--out-dir"],
            "--out-dir needs a value".to_owned(),
        ),
        (
            vec!["--commit", COMMIT, "--out-dir", &out, "--force"],
            "unknown argument: --force".to_owned(),
        ),
    ];
    for (args, why) in cases {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let ran = rig.run_with(&args);
        assert_eq!(ran.status, 2, "{args:?} {ran:?}");
        assert!(
            ran.stderr.starts_with(&format!(
                "submit_rollout.sh: {why}\nusage: submit_rollout.sh "
            )),
            "{args:?} {ran:?}"
        );
        assert!(
            ran.gh.is_empty() && ran.docker.is_empty(),
            "{args:?} {ran:?}"
        );
    }
    let ran = rig.run_with(&["--help".to_owned()]);
    assert_eq!(ran.status, 2, "{ran:?}");
    assert!(
        ran.stderr.starts_with("usage: submit_rollout.sh "),
        "{ran:?}"
    );
}
