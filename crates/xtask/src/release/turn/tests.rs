//! `release turn`: the decision of each rule, and the reading of `gh`'s answers, the refusals
//! and the waiting loop, run against a stand-in `gh` (a shell script in a scratch directory
//! that prints canned answers). No network is used and the real `gh` is never started.

use std::os::unix::fs::PermissionsExt;

use js_testkit::dir::TestDir;

use super::*;

fn v(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

/// A peer whose `publish` succeeded (its state matters only when it ended otherwise).
fn peer(number: u64, turn: Job, deployed: Job) -> Peer {
    with_publish(number, turn, Job::Succeeded, deployed)
}

/// A peer with each of its three jobs given.
fn with_publish(number: u64, turn: Job, publish: Job, deployed: Job) -> Peer {
    Peer {
        number,
        turn,
        publish,
        deployed,
    }
}

fn go() -> Decision {
    Decision::Go("no newer release, no newer run at its turn, no older run holding it".into())
}

fn superseded_by(number: u64) -> Decision {
    Decision::Superseded(format!(
        "run #{number}, newer, reached its turn: it publishes instead"
    ))
}

/// Asserts that a release newer than this run's version supersedes it, before any run is
/// looked at, and that an equal or older one does not.
#[test]
fn a_newer_published_release_supersedes_and_an_equal_one_does_not() {
    let me = v(1, 2, 3);
    assert_eq!(
        decide(5, me, Some(v(1, 2, 4)), &[]),
        Decision::Superseded("release v1.2.4 is published, newer than v1.2.3".into())
    );
    assert_eq!(
        decide(
            5,
            me,
            Some(v(2, 0, 0)),
            &[peer(4, Job::Running, Job::Absent)]
        ),
        Decision::Superseded("release v2.0.0 is published, newer than v1.2.3".into())
    );
    assert_eq!(decide(5, me, Some(v(1, 2, 3)), &[]), go());
    assert_eq!(decide(5, me, Some(v(1, 1, 9)), &[]), go());
    assert_eq!(decide(5, me, None, &[]), go());
}

/// Asserts that a newer run whose `turn` is running or succeeded supersedes this run, and one
/// whose `turn` is queued, absent, failed or cancelled does not.
#[test]
fn a_newer_run_at_its_turn_supersedes_and_one_not_there_does_not() {
    let me = v(1, 0, 0);
    for turn in [Job::Running, Job::Succeeded] {
        assert_eq!(
            decide(5, me, None, &[peer(6, turn, Job::Absent)]),
            superseded_by(6)
        );
    }
    for turn in [Job::Queued, Job::Absent, Job::Ended] {
        assert_eq!(
            decide(5, me, None, &[peer(6, turn, Job::Absent)]),
            go(),
            "{turn:?}"
        );
    }
}

/// Asserts that an older run holds the turn while its `turn` runs, or while it succeeded and
/// `deployed` is absent, queued or running, and not once `deployed` ended with any conclusion.
#[test]
fn an_older_run_holds_until_its_deployed_job_ended() {
    let me = v(1, 0, 0);
    assert_eq!(
        decide(5, me, None, &[peer(4, Job::Running, Job::Absent)]),
        Decision::Wait("run #4 has its turn".into())
    );
    assert_eq!(
        decide(5, me, None, &[peer(4, Job::Running, Job::Succeeded)]),
        Decision::Wait("run #4 has its turn".into())
    );
    for deployed in [Job::Absent, Job::Queued, Job::Running] {
        assert_eq!(
            decide(5, me, None, &[peer(4, Job::Succeeded, deployed)]),
            Decision::Wait("run #4 has published and not yet deployed".into()),
            "{deployed:?}"
        );
    }
    for deployed in [Job::Succeeded, Job::Ended] {
        assert_eq!(
            decide(5, me, None, &[peer(4, Job::Succeeded, deployed)]),
            go(),
            "{deployed:?}"
        );
    }
    // A `publish` not yet started or still running holds the turn as one that succeeded does.
    for publish in [Job::Absent, Job::Queued, Job::Running] {
        assert_eq!(
            decide(
                5,
                me,
                None,
                &[with_publish(4, Job::Succeeded, publish, Job::Queued)]
            ),
            Decision::Wait("run #4 has published and not yet deployed".into()),
            "{publish:?}"
        );
    }
    for turn in [Job::Absent, Job::Queued, Job::Ended] {
        assert_eq!(
            decide(5, me, None, &[peer(4, turn, Job::Running)]),
            go(),
            "{turn:?}"
        );
    }
}

/// Asserts the order of the rules and the mix of several runs: a newer run at its turn wins
/// over older holders, every holder is named, and this run's own number is never a peer's.
#[test]
fn several_runs_mix_by_the_order_of_the_rules() {
    let me = v(1, 0, 0);
    let peers = [
        peer(3, Job::Running, Job::Absent),
        peer(4, Job::Succeeded, Job::Running),
        peer(2, Job::Succeeded, Job::Ended),
        peer(7, Job::Queued, Job::Absent),
    ];
    assert_eq!(
        decide(5, me, None, &peers),
        Decision::Wait("run #3 has its turn; run #4 has published and not yet deployed".into())
    );
    let mut with_newer = peers.to_vec();
    with_newer.push(peer(8, Job::Running, Job::Absent));
    with_newer.push(peer(9, Job::Succeeded, Job::Absent));
    assert_eq!(decide(5, me, None, &with_newer), superseded_by(8));
}

/// Takes the turn of this test and makes the scratch directory. The lock is the one `release
/// desired`'s tests hold while they write and start their stand-ins: a script being written is a
/// file open for writing, and a program another test of this process starts meanwhile would
/// carry that descriptor into its child and fail the script's start ("text file busy").
fn setup(name: &str) -> (std::sync::MutexGuard<'static, ()>, TestDir) {
    let guard = super::super::desired::tests::serial();
    (guard, TestDir::under(std::env::temp_dir(), name))
}

/// The shell that answers one call: the canned text, or a failure of `gh` for `!fail`.
fn answer(text: &str) -> String {
    if text == "!fail" {
        "echo 'gh: HTTP 502' >&2; exit 1".to_owned()
    } else {
        format!("cat <<'EOF'\n{text}\nEOF")
    }
}

/// Writes a stand-in `gh` that appends each call's path to `gh.log`, answers the list of runs
/// with `runs`, the jobs of run `id` with `jobs[id]`, the releases with `releases`, and fails
/// (status 9) for any other call.
fn gh(dir: &TestDir, runs: &str, jobs: &[(u64, &str)], releases: &str) -> String {
    let mut body = format!(
        "echo \"$*\" >> '{}'\n[ \"$1\" = api ] || exit 9\ncase \"$2\" in\n",
        dir.join("gh.log").display()
    );
    body.push_str(&format!(
        "'repos/o/r/actions/workflows/release.yml/runs?branch=main&event=push&status=in_progress&per_page=100')\n{}\n;;\n",
        answer(runs)
    ));
    for (id, text) in jobs {
        body.push_str(&format!(
            "'repos/o/r/actions/runs/{id}/jobs?per_page=100&filter=latest')\n{}\n;;\n",
            answer(text)
        ));
    }
    body.push_str(&format!(
        "'repos/o/r/releases?per_page=100')\n{}\n;;\n*) echo \"unexpected: $*\" >&2; exit 9;;\nesac\n",
        answer(releases)
    ));
    let path = dir.join("gh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    path.display().to_string()
}

fn args(gh: String, run_number: u64, version: &str) -> TurnArgs {
    TurnArgs {
        repo: "o/r".into(),
        workflow: "release.yml".into(),
        run_number,
        version: version.into(),
        poll_seconds: 0,
        output: None,
        gh,
    }
}

fn failed(result: Result<String, Error>) -> String {
    match result {
        Err(Error::Failed(why)) => why,
        other => panic!("not Failed: {other:?}"),
    }
}

const NO_RELEASES: &str = "[]";

fn run_list(runs: &[(u64, u64)]) -> String {
    let items: Vec<String> = runs
        .iter()
        .map(|(id, n)| format!("{{\"id\":{id},\"run_number\":{n}}}"))
        .collect();
    format!(
        "{{\"total_count\":{},\"workflow_runs\":[{}]}}",
        items.len(),
        items.join(",")
    )
}

fn job_list(jobs: &[(&str, &str, Option<&str>)]) -> String {
    let items: Vec<String> = jobs
        .iter()
        .map(|(name, status, conclusion)| match conclusion {
            Some(c) => {
                format!("{{\"name\":\"{name}\",\"status\":\"{status}\",\"conclusion\":\"{c}\"}}")
            }
            None => format!("{{\"name\":\"{name}\",\"status\":\"{status}\",\"conclusion\":null}}"),
        })
        .collect();
    format!(
        "{{\"total_count\":{},\"jobs\":[{}]}}",
        items.len(),
        items.join(",")
    )
}

/// Asserts that an older run whose `turn` succeeded but whose `publish` ended other than in
/// success (skipped because its `turn` said `go=false`, or failed) does not hold the turn while
/// its `deployed` still waits for its other jobs, such as the macOS package.
#[test]
fn an_older_run_that_did_not_publish_does_not_hold_the_turn() {
    let me = v(1, 0, 0);
    for deployed in [Job::Absent, Job::Queued, Job::Running] {
        assert_eq!(
            decide(
                5,
                me,
                None,
                &[with_publish(4, Job::Succeeded, Job::Ended, deployed)]
            ),
            go(),
            "{deployed:?}"
        );
    }
    // Its `turn` still running holds it, whatever its `publish` reads.
    assert_eq!(
        decide(
            5,
            me,
            None,
            &[with_publish(4, Job::Running, Job::Ended, Job::Absent)]
        ),
        Decision::Wait("run #4 has its turn".into())
    );
}

/// Asserts that the runs and jobs are read into the states of `turn` and `deployed`: this
/// run's own number is skipped (its jobs are never asked for), other jobs are ignored, a
/// missing job is absent, and each status and conclusion maps to its state.
#[test]
fn the_jobs_of_other_runs_are_read_into_states() {
    let (_serial, dir) = setup("turn-read");
    let jobs_a = job_list(&[
        ("build", "completed", Some("failure")),
        ("turn", "completed", Some("success")),
        ("publish", "completed", Some("success")),
        ("deployed", "in_progress", None),
    ]);
    let jobs_b = job_list(&[
        ("turn", "queued", None),
        ("deployed", "completed", Some("skipped")),
    ]);
    let jobs_d = job_list(&[("build", "completed", Some("success"))]);
    let jobs_c = job_list(&[
        ("turn", "completed", Some("cancelled")),
        ("deployed", "waiting", None),
    ]);
    let gh = gh(
        &dir,
        &run_list(&[(11, 4), (12, 5), (13, 6), (14, 7), (15, 8)]),
        &[(11, &jobs_a), (13, &jobs_b), (14, &jobs_c), (15, &jobs_d)],
        NO_RELEASES,
    );
    let got = peers(&args(gh, 5, "1.0.0")).expect("read");
    assert_eq!(
        got,
        Round::Peers(vec![
            peer(4, Job::Succeeded, Job::Running),
            with_publish(6, Job::Queued, Job::Absent, Job::Ended),
            with_publish(7, Job::Ended, Job::Absent, Job::Queued),
            with_publish(8, Job::Absent, Job::Absent, Job::Absent),
        ])
    );
    let log = std::fs::read_to_string(dir.join("gh.log")).expect("log");
    assert!(!log.contains("runs/12/"), "{log}");
    let none = peers(&args(
        self::gh(&dir, &run_list(&[]), &[], NO_RELEASES),
        5,
        "1.0.0",
    ));
    assert_eq!(none, Ok(Round::Peers(vec![])));
}

/// Asserts that the newest published release is the highest `vX.Y.Z` tag, ignoring drafts,
/// pre-releases and tags that are not versions, and that none at all is no release.
#[test]
fn the_newest_release_ignores_drafts_prereleases_and_other_tags() {
    let (_serial, dir) = setup("turn-releases");
    let releases = "[{\"tag_name\":\"v1.2.3\",\"draft\":false,\"prerelease\":false},\
        {\"tag_name\":\"v1.10.0\",\"draft\":false,\"prerelease\":false},\
        {\"tag_name\":\"v9.0.0\",\"draft\":true,\"prerelease\":false},\
        {\"tag_name\":\"v9.1.0\",\"draft\":false,\"prerelease\":true},\
        {\"tag_name\":\"nightly\",\"draft\":false,\"prerelease\":false},\
        {\"draft\":false,\"prerelease\":false}]";
    let gh1 = gh(&dir, &run_list(&[]), &[], releases);
    assert_eq!(newest(&gh1, "o/r"), Ok(Some(v(1, 10, 0))));
    let gh2 = gh(
        &dir,
        &run_list(&[]),
        &[],
        "[{\"tag_name\":\"v9.0.0\",\"draft\":true,\"prerelease\":false}]",
    );
    assert_eq!(newest(&gh2, "o/r"), Ok(None));
    let gh3 = gh(&dir, &run_list(&[]), &[], NO_RELEASES);
    assert_eq!(newest(&gh3, "o/r"), Ok(None));
}

/// Asserts that answers that are not what was asked for are errors naming the call: `gh`
/// failing, non-JSON, a release list that is not a list, partial pages, and fields missing
/// or of the wrong type.
#[test]
fn answers_that_are_not_what_was_asked_for_are_refused_with_context() {
    let (_serial, dir) = setup("turn-refused");
    let good_jobs = job_list(&[("turn", "queued", None)]);
    let good_runs = run_list(&[(11, 4)]);
    let check = |runs: &str, jobs: &str, releases: &str| {
        let gh = gh(&dir, runs, &[(11, jobs)], releases);
        run_to(&args(gh, 5, "1.0.0"), &mut Vec::new())
    };

    match check(&good_runs, &good_jobs, "!fail") {
        Err(Error::Release(super::super::Error::Failed { stderr, .. })) => {
            assert_eq!(stderr, "gh: HTTP 502");
        }
        other => panic!("{other:?}"),
    }
    match check("!fail", &good_jobs, NO_RELEASES) {
        Err(Error::Release(super::super::Error::Failed { .. })) => {}
        other => panic!("{other:?}"),
    }
    match check(&good_runs, "!fail", NO_RELEASES) {
        Err(Error::Release(super::super::Error::Failed { .. })) => {}
        other => panic!("{other:?}"),
    }

    let why = failed(check(&good_runs, &good_jobs, "not json"));
    assert!(
        why.starts_with("gh api repos/o/r/releases?per_page=100 did not print JSON: "),
        "{why}"
    );
    assert_eq!(
        failed(check(&good_runs, &good_jobs, "{}")),
        "releases of o/r: the answer is not a list"
    );

    let what = "runs of release.yml of o/r";
    assert_eq!(
        failed(check("{\"total_count\":1}", &good_jobs, NO_RELEASES)),
        format!("{what}: `workflow_runs` is not a list")
    );
    assert_eq!(
        failed(check("{\"workflow_runs\":[]}", &good_jobs, NO_RELEASES)),
        format!("{what}: `total_count` is not a number")
    );
    // A full page short of the count is an error at once; one entry fewer than a full page,
    // short of the count, is read again and is an error only on the tenth round in a row.
    let runs = |listed: u64, total: u64| {
        let items: Vec<String> = (0..listed)
            .map(|n| format!("{{\"id\":{},\"run_number\":{}}}", 100 + n, n + 1))
            .collect();
        format!(
            "{{\"total_count\":{total},\"workflow_runs\":[{}]}}",
            items.join(",")
        )
    };
    assert_eq!(
        failed(check(&runs(100, 101), &good_jobs, NO_RELEASES)),
        format!("{what}: 101 entries, one page of 100 was read")
    );
    assert_eq!(
        failed(check(&runs(99, 100), &good_jobs, NO_RELEASES)),
        format!("{what}: GitHub counts 100 entries and lists 99, on 10 reads in a row")
    );
    assert_eq!(
        failed(check(
            "{\"total_count\":1,\"workflow_runs\":[{\"run_number\":4}]}",
            &good_jobs,
            NO_RELEASES
        )),
        format!("{what}: `id` is not a number")
    );
    assert_eq!(
        failed(check(
            "{\"total_count\":1,\"workflow_runs\":[{\"id\":11}]}",
            &good_jobs,
            NO_RELEASES
        )),
        format!("{what}: `run_number` is not a number")
    );

    let what = "jobs of run 11 (#4) of o/r";
    let jobs = |listed: usize, total: usize| {
        let items = vec!["{\"name\":\"turn\",\"status\":\"queued\"}"; listed];
        format!("{{\"total_count\":{total},\"jobs\":[{}]}}", items.join(","))
    };
    assert_eq!(
        failed(check(&good_runs, &jobs(100, 101), NO_RELEASES)),
        format!("{what}: 101 entries, one page of 100 was read")
    );
    assert_eq!(
        failed(check(&good_runs, &jobs(1, 3), NO_RELEASES)),
        format!("{what}: GitHub counts 3 entries and lists 1, on 10 reads in a row")
    );
    assert_eq!(
        failed(check(
            &good_runs,
            "{\"total_count\":1,\"jobs\":[{\"status\":\"queued\"}]}",
            NO_RELEASES
        )),
        format!("{what}: `name` is not a string")
    );
    assert_eq!(
        failed(check(
            &good_runs,
            "{\"total_count\":1,\"jobs\":[{\"name\":\"turn\"}]}",
            NO_RELEASES
        )),
        format!("{what}: `status` is not a string")
    );
    assert_eq!(
        failed(check(
            &good_runs,
            "{\"total_count\":1,\"jobs\":[{\"name\":\"turn\",\"status\":\"completed\",\"conclusion\":null}]}",
            NO_RELEASES
        )),
        format!("{what}: `conclusion` is not a string")
    );
    assert_eq!(
        failed(check(
            &good_runs,
            "{\"total_count\":2,\"jobs\":[{\"name\":\"turn\",\"status\":\"queued\"},{\"name\":\"deployed\"}]}",
            NO_RELEASES
        )),
        format!("{what}: `status` is not a string")
    );
}

/// Asserts that a `--version` that is not `X.Y.Z` is a usage error before `gh` is asked.
#[test]
fn a_version_that_is_not_x_y_z_is_a_usage_error() {
    let a = args("/nonexistent/gh".into(), 5, "1.2");
    assert_eq!(
        run_to(&a, &mut Vec::new()),
        Err(Error::Usage(
            "--version \"1.2\" is not <major>.<minor>.<patch>, a release tag without its v".into()
        ))
    );
}

/// Asserts that a go and a supersession print the reason and the `go=` line, return them, and
/// append `go=` and `reason=` to the output file, once each time the verb runs.
#[test]
fn a_decision_is_returned_and_appended_to_the_output_file() {
    let (_serial, dir) = setup("turn-decided");
    let out = dir.join("out");
    let mut a = args(
        gh(&dir, &run_list(&[(12, 5)]), &[], NO_RELEASES),
        5,
        "1.0.0",
    );
    a.output = Some(out.clone());
    assert_eq!(
        run_to(&a, &mut Vec::new()),
        Ok("no newer release, no newer run at its turn, no older run holding it\ngo=true".into())
    );
    a.gh = gh(
        &dir,
        &run_list(&[]),
        &[],
        "[{\"tag_name\":\"v1.0.1\",\"draft\":false,\"prerelease\":false}]",
    );
    assert_eq!(
        run_to(&a, &mut Vec::new()),
        Ok("release v1.0.1 is published, newer than v1.0.0\ngo=false".into())
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("output"),
        "go=true\nreason=no newer release, no newer run at its turn, no older run holding it\n\
         go=false\nreason=release v1.0.1 is published, newer than v1.0.0\n"
    );
    a.output = None;
    assert_eq!(
        run_to(&a, &mut Vec::new()),
        Ok("release v1.0.1 is published, newer than v1.0.0\ngo=false".into())
    );
    a.output = Some(dir.join("missing-dir").join("out"));
    match run_to(&a, &mut Vec::new()) {
        Err(Error::Release(super::super::Error::Io { .. })) => {}
        other => panic!("{other:?}"),
    }
}

/// Asserts the waiting loop: the first round an older run holds the turn (one line says so and
/// the verb reads again), the second round it has deployed and the verb goes; and that `run`
/// itself, which prints to stdout, decides the same.
#[test]
fn the_verb_waits_while_an_older_run_holds_the_turn_then_goes() {
    let (_serial, dir) = setup("turn-loop");
    let holding = job_list(&[
        ("turn", "completed", Some("success")),
        ("deployed", "in_progress", None),
    ]);
    let done = job_list(&[
        ("turn", "completed", Some("success")),
        ("deployed", "completed", Some("success")),
    ]);
    let mut body = format!(
        "echo \"$*\" >> '{log}'\ncase \"$2\" in\n\
         *workflows*) echo '{runs}';;\n\
         *releases*) echo '[]';;\n\
         *jobs*) if [ \"$(grep -c workflows '{log}')\" -ge 2 ]; then echo '{done}'; else echo '{holding}'; fi;;\n\
         esac\n",
        log = dir.join("gh.log").display(),
        runs = run_list(&[(11, 4)]),
    );
    body.insert_str(0, "#!/bin/sh\n");
    let path = dir.join("gh");
    std::fs::write(&path, body).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let mut a = args(path.display().to_string(), 5, "1.0.0");
    a.output = Some(dir.join("out"));
    let mut printed = Vec::new();
    assert_eq!(
        run_to(&a, &mut printed),
        Ok("no newer release, no newer run at its turn, no older run holding it\ngo=true".into())
    );
    assert_eq!(
        String::from_utf8(printed).expect("utf-8"),
        "waiting: run #4 has published and not yet deployed\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        "go=true\nreason=no newer release, no newer run at its turn, no older run holding it\n"
    );
    // The same stand-in, now past its second round, makes `run` decide without waiting.
    assert!(run(&a).is_ok());
}

/// Writes a stand-in `gh` whose `n`th list of runs answers `runs[n - 1]` and whose `n`th list
/// of the jobs of run 11 answers `jobs[n - 1]` (each the last of its list once past its end),
/// with no release; every call's path is appended to `gh.log`.
fn sequenced_gh(dir: &TestDir, runs: &[String], jobs: &[String]) -> String {
    for (kind, answers) in [("runs", runs), ("jobs", jobs)] {
        for (n, text) in answers.iter().enumerate() {
            std::fs::write(dir.join(format!("{kind}.{}", n + 1)), text).expect("answer");
        }
        if let Some(last) = answers.last() {
            std::fs::write(dir.join(format!("{kind}.last")), last).expect("answer");
        }
    }
    let log = dir.join("gh.log");
    let body = format!(
        "#!/bin/sh\necho \"$*\" >> '{log}'\n\
         answer() {{ f='{dir}/'$1.$(grep -c \"$2\" '{log}'); [ -f \"$f\" ] || f='{dir}/'$1.last; cat \"$f\"; }}\n\
         case \"$2\" in\n\
         'repos/o/r/actions/workflows/release.yml/runs?branch=main&event=push&status=in_progress&per_page=100') answer runs /workflows/;;\n\
         'repos/o/r/actions/runs/11/jobs?per_page=100&filter=latest') answer jobs /runs/11/jobs;;\n\
         'repos/o/r/releases?per_page=100') echo '[]';;\n\
         *) echo \"unexpected: $*\" >&2; exit 9;;\n\
         esac\n",
        log = log.display(),
        dir = dir.path().display(),
    );
    let path = dir.join("gh");
    std::fs::write(&path, body).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    path.display().to_string()
}

/// How many calls of `gh.log` name `what`.
fn calls(dir: &TestDir, what: &str) -> usize {
    std::fs::read_to_string(dir.join("gh.log"))
        .expect("gh.log")
        .lines()
        .filter(|l| l.contains(what))
        .count()
}

/// The list of runs GitHub counts as 1 and lists empty, as it answered while one run of the
/// workflow was in progress.
const RUNS_BEHIND: &str = "{\"total_count\":1,\"workflow_runs\":[]}";

/// Asserts that a list of runs, then a list of jobs, that GitHub counts longer than it lists
/// (the page not full) is not an error: each round prints one line naming the list and reads
/// again, and the verb decides once the lists are whole.
#[test]
fn a_list_behind_its_count_is_read_again_until_it_is_whole() {
    let (_serial, dir) = setup("turn-behind");
    let done = job_list(&[
        ("turn", "completed", Some("success")),
        ("deployed", "completed", Some("success")),
    ]);
    let jobs_behind = "{\"total_count\":2,\"jobs\":[{\"name\":\"turn\",\"status\":\"completed\",\"conclusion\":\"success\"}]}";
    let gh = sequenced_gh(
        &dir,
        &[RUNS_BEHIND.to_owned(), run_list(&[(11, 4)])],
        &[jobs_behind.to_owned(), done],
    );
    let mut printed = Vec::new();
    assert_eq!(
        run_to(&args(gh, 5, "1.0.0"), &mut printed),
        Ok("no newer release, no newer run at its turn, no older run holding it\ngo=true".into())
    );
    assert_eq!(
        String::from_utf8(printed).expect("utf-8"),
        "waiting: runs of release.yml of o/r: GitHub counts 1 entries and lists 0; reading again\n\
         waiting: jobs of run 11 (#4) of o/r: GitHub counts 2 entries and lists 1; reading again\n"
    );
    assert_eq!(calls(&dir, "/workflows/"), 3);
    assert_eq!(calls(&dir, "/runs/11/jobs"), 2);
    assert_eq!(calls(&dir, "/releases"), 3);
}

/// Asserts that a list GitHub counts longer than it lists on ten rounds in a row is an error
/// naming the list, after nine waiting lines and no tenth: the verb does not read forever.
#[test]
fn a_list_behind_its_count_ten_rounds_in_a_row_is_an_error() {
    let (_serial, dir) = setup("turn-behind-ten");
    let gh = sequenced_gh(&dir, &[RUNS_BEHIND.to_owned()], &[]);
    let mut printed = Vec::new();
    assert_eq!(
        failed(run_to(&args(gh, 5, "1.0.0"), &mut printed)),
        "runs of release.yml of o/r: GitHub counts 1 entries and lists 0, on 10 reads in a row"
    );
    assert_eq!(
        String::from_utf8(printed).expect("utf-8"),
        "waiting: runs of release.yml of o/r: GitHub counts 1 entries and lists 0; reading again\n"
            .repeat(9)
    );
    assert_eq!(calls(&dir, "/workflows/"), 10);
}

/// Asserts that a round whose lists are whole starts the count of rounds behind again: nine
/// rounds behind, a whole round in which an older run holds the turn, nine more behind, then a
/// whole round that goes; nineteen rounds behind in all and no error.
#[test]
fn a_whole_round_between_rounds_behind_starts_their_count_again() {
    let (_serial, dir) = setup("turn-behind-reset");
    let holding = job_list(&[
        ("turn", "completed", Some("success")),
        ("deployed", "in_progress", None),
    ]);
    let done = job_list(&[
        ("turn", "completed", Some("success")),
        ("deployed", "completed", Some("success")),
    ]);
    let whole = run_list(&[(11, 4)]);
    let mut runs = vec![RUNS_BEHIND.to_owned(); 9];
    runs.push(whole.clone());
    runs.extend(vec![RUNS_BEHIND.to_owned(); 9]);
    runs.push(whole);
    let gh = sequenced_gh(&dir, &runs, &[holding, done]);
    let mut printed = Vec::new();
    assert_eq!(
        run_to(&args(gh, 5, "1.0.0"), &mut printed),
        Ok("no newer release, no newer run at its turn, no older run holding it\ngo=true".into())
    );
    let behind =
        "waiting: runs of release.yml of o/r: GitHub counts 1 entries and lists 0; reading again\n"
            .repeat(9);
    assert_eq!(
        String::from_utf8(printed).expect("utf-8"),
        format!("{behind}waiting: run #4 has published and not yet deployed\n{behind}")
    );
    assert_eq!(calls(&dir, "/workflows/"), 20);
}
