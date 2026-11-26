//! The release's run of the two-run isolation test (`.github/workflows/js-agent-native-isolation.yml`,
//! job `isolation`), read as it is in the repository: a job on a hosted Mac that runs
//! `cargo xtask mac-root-tests` for the `native_isolation` target alone, from a work directory
//! outside every directory the test searches for what a run left (`FIND_ROOTS` of
//! `crates/js-agent/tests/native_isolation/main.rs`), and whose failure is not passed over; and
//! its call in release.yml, `beta-macos-isolation`: on a push to main alone, counted by
//! `release-red` and `release-green`, and not needed by `promote-macos`, since on a hosted
//! runner the test reaches no verdict yet (the workflow's header says why).

use std::path::Path;

const WORKFLOW: &str = ".github/workflows/js-agent-native-isolation.yml";
const JOB: &str = "isolation";
const CALL: &str = "beta-macos-isolation";
const ISOLATION: &str = "crates/js-agent/tests/native_isolation/main.rs";
const PUSH: &str = "github.event_name == 'push' && github.ref == 'refs/heads/main'";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The trimmed lines, comments and blank lines left out, of the top-level job `name` of
/// `workflow`: from its `  <name>:` line to the next line indented two spaces or less.
fn job_lines<'a>(workflow: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let lines: Vec<&str> = workflow
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect();
    let at = lines.iter().position(|l| *l == format!("  {name}:"))?;
    Some(
        lines[at + 1..]
            .iter()
            .take_while(|l| l.starts_with("   "))
            .map(|l| l.trim())
            .collect(),
    )
}

/// The quoted directories of the `FIND_ROOTS` constant of the isolation test's source.
fn find_roots(isolation_src: &str) -> Vec<&str> {
    let Some(at) = isolation_src.find("const FIND_ROOTS") else {
        return Vec::new();
    };
    let rest = &isolation_src[at..];
    let body = rest.split_once("= [").map_or("", |(_, b)| b);
    let body = body.split_once("];").map_or("", |(b, _)| b);
    body.split('"').skip(1).step_by(2).collect()
}

/// The value after `flag` in `words`, for each time it is there.
fn values<'a>(words: &[&'a str], flag: &str) -> Vec<&'a str> {
    words
        .windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1])
        .collect()
}

/// The names in a job's `needs: [a, b]` line.
fn needs<'a>(job: &[&'a str]) -> Vec<&'a str> {
    job.iter()
        .find_map(|l| l.strip_prefix("needs: "))
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect()
}

/// One line per way the job of `workflow` is not the isolation test's own run, given the
/// test's source `isolation_src`.
fn check_job(workflow: &str, isolation_src: &str) -> Vec<String> {
    let Some(job) = job_lines(workflow, JOB) else {
        return vec![format!("{WORKFLOW} has no job `{JOB}`")];
    };
    let mut found = Vec::new();
    if !job.iter().any(|l| {
        l.strip_prefix("runs-on: ")
            .is_some_and(|r| r.starts_with("macos-"))
    }) {
        found.push("the job does not run on a hosted Mac (runs-on: macos-*)".to_owned());
    }
    if job.iter().any(|l| l.starts_with("continue-on-error:")) {
        found.push("the job passes over its failure (continue-on-error)".to_owned());
    }
    let runs: Vec<Vec<&str>> = job
        .iter()
        .filter_map(|l| l.trim_start_matches("- ").strip_prefix("run: "))
        .map(|r| r.split_whitespace().collect())
        .filter(|w: &Vec<&str>| w.starts_with(&["cargo", "xtask", "mac-root-tests"]))
        .collect();
    let [run] = runs.as_slice() else {
        found.push(format!(
            "the job has {} runs of `cargo xtask mac-root-tests`, not 1",
            runs.len()
        ));
        return found;
    };
    if values(run, "--only") != ["native_isolation"] {
        found.push(format!(
            "the run selects {:?}, not --only native_isolation alone",
            values(run, "--only")
        ));
    }
    let roots = find_roots(isolation_src);
    if roots.is_empty() {
        found.push(format!(
            "{ISOLATION} has no FIND_ROOTS to check the work directory against"
        ));
    }
    match values(run, "--work").as_slice() {
        [work] if work.starts_with('/') => {
            for root in roots {
                if Path::new(work).starts_with(root) {
                    found.push(format!(
                        "the work directory {work} is below {root}, which the test searches"
                    ));
                }
            }
        }
        other => found.push(format!(
            "the run's --work is {other:?}, not one absolute path"
        )),
    }
    found
}

/// One line per way release.yml's call of the workflow is not the one the release asks for:
/// the call gone, calling another workflow or without its push condition; a report that does
/// not count it; or `promote-macos` needing it while the test reaches no verdict on a hosted
/// runner.
fn check_release(release: &str) -> Vec<String> {
    let mut found = Vec::new();
    match job_lines(release, CALL) {
        None => found.push(format!("release.yml has no job {CALL}")),
        Some(call) => {
            let uses = format!("uses: ./{WORKFLOW}");
            if !call.contains(&uses.as_str()) {
                found.push(format!("{CALL} does not call {WORKFLOW}"));
            }
            if !call
                .iter()
                .any(|l| l.starts_with("if: ") && l.contains(PUSH))
            {
                found.push(format!("{CALL} runs for more than a push to main"));
            }
        }
    }
    for report in ["release-red", "release-green"] {
        let counted = job_lines(release, report).is_some_and(|job| needs(&job).contains(&CALL));
        if !counted {
            found.push(format!("{report} does not need {CALL}"));
        }
    }
    let promote_needs =
        job_lines(release, "promote-macos").is_some_and(|job| needs(&job).contains(&CALL));
    if promote_needs {
        found.push(format!(
            "promote-macos needs {CALL}, which reaches no verdict on a hosted runner"
        ));
    }
    found
}

/// Asserts that the repository's release runs the isolation test on a Mac of its own, that the
/// reports count it, and that the macOS archives do not wait for it.
#[test]
fn the_release_runs_the_isolation_test_and_reports_it() {
    let empty: Vec<String> = Vec::new();
    assert_eq!(check_job(&read(WORKFLOW), &read(ISOLATION)), empty);
    assert_eq!(check_release(&read(".github/workflows/release.yml")), empty);
    assert_eq!(
        find_roots(&read(ISOLATION)),
        [
            "/private/var/folders",
            "/private/tmp",
            "/Users/Shared",
            "/Library/Caches"
        ]
    );
}

/// Asserts that each way the job stops being the isolation test's run is refused with its own
/// line: the job gone, run elsewhere than a Mac, its failure passed over, the run gone or
/// doubled, another or a second target selected, the work directory relative, missing or
/// below a directory the test searches, and a test source without its list of searched
/// directories.
///
/// Catches a check that passes a workflow whose isolation run is wrong.
#[test]
fn a_job_that_breaks_the_isolation_run_is_refused() {
    let workflow = read(WORKFLOW);
    let src = read(ISOLATION);
    let edit = |from: &str, to: &str| {
        assert!(workflow.contains(from), "{from}");
        workflow.replacen(from, to, 1)
    };
    let run = "run: cargo xtask mac-root-tests --work /private/var/js-mac-root-tests --only native_isolation";
    assert!(workflow.contains(run));
    let with_run = |to: &str| edit(run, &format!("run: {to}"));

    assert_eq!(
        check_job(&edit("\n  isolation:\n", "\n  elsewhere:\n"), &src),
        [format!("{WORKFLOW} has no job `isolation`")]
    );
    let runner = "    runs-on: macos-15\n";
    assert_eq!(
        check_job(&edit(runner, "    runs-on: ubuntu-24.04\n"), &src),
        ["the job does not run on a hosted Mac (runs-on: macos-*)"]
    );
    assert_eq!(
        check_job(
            &edit(runner, &format!("{runner}    continue-on-error: true\n")),
            &src
        ),
        ["the job passes over its failure (continue-on-error)"]
    );
    assert_eq!(
        check_job(&with_run("true"), &src),
        ["the job has 0 runs of `cargo xtask mac-root-tests`, not 1"]
    );
    assert_eq!(
        check_job(&edit(run, &format!("{run}\n      - {run}")), &src),
        ["the job has 2 runs of `cargo xtask mac-root-tests`, not 1"]
    );
    assert_eq!(
        check_job(
            &with_run("cargo xtask mac-root-tests --work /private/var/w"),
            &src
        ),
        ["the run selects [], not --only native_isolation alone"]
    );
    assert_eq!(
        check_job(
            &with_run(
                "cargo xtask mac-root-tests --work /private/var/w --only native_isolation --only pf_as_root"
            ),
            &src
        ),
        [
            "the run selects [\"native_isolation\", \"pf_as_root\"], not --only native_isolation alone"
        ]
    );
    assert_eq!(
        check_job(
            &with_run("cargo xtask mac-root-tests --work w --only native_isolation"),
            &src
        ),
        ["the run's --work is [\"w\"], not one absolute path"]
    );
    assert_eq!(
        check_job(
            &with_run("cargo xtask mac-root-tests --only native_isolation"),
            &src
        ),
        ["the run's --work is [], not one absolute path"]
    );
    for root in ["/Users/Shared", "/private/tmp", "/private/var/folders"] {
        assert_eq!(
            check_job(
                &with_run(&format!(
                    "cargo xtask mac-root-tests --work {root}/w --only native_isolation"
                )),
                &src
            ),
            [format!(
                "the work directory {root}/w is below {root}, which the test searches"
            )]
        );
    }
    // A sibling whose name only starts like a searched directory is not below it.
    assert_eq!(
        check_job(
            &with_run("cargo xtask mac-root-tests --work /private/tmpwork --only native_isolation"),
            &src
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        check_job(&workflow, &src.replace("const FIND_ROOTS", "const RENAMED")),
        [format!(
            "{ISOLATION} has no FIND_ROOTS to check the work directory against"
        )]
    );
}

/// Asserts that each way release.yml's call stops being the one the release asks for is
/// refused with its own line: the call gone (renamed), pointing elsewhere or without its push
/// condition, a report that no longer counts it, and `promote-macos` made to need it.
///
/// Catches a release whose isolation verdict is never reported, runs on a pull request, or
/// holds back the macOS archives while the test cannot pass where it runs.
#[test]
fn a_call_that_is_not_reported_or_gates_the_archives_is_refused() {
    let release = read(".github/workflows/release.yml");
    let edit = |from: &str, to: &str| {
        assert!(release.contains(from), "{from}");
        release.replacen(from, to, 1)
    };
    assert_eq!(
        check_release(&edit(
            "\n  beta-macos-isolation:\n",
            "\n  beta-macos-other:\n"
        )),
        ["release.yml has no job beta-macos-isolation"]
    );
    assert_eq!(
        check_release(&edit(
            "    uses: ./.github/workflows/js-agent-native-isolation.yml\n",
            "    uses: ./.github/workflows/macos-probe.yml\n"
        )),
        [format!("{CALL} does not call {WORKFLOW}")]
    );
    let call = "  beta-macos-isolation:\n    needs: [plan, artifacts]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n";
    assert_eq!(
        check_release(&edit(
            call,
            "  beta-macos-isolation:\n    needs: [plan, artifacts]\n    if: ${{ needs.plan.outputs.release == 'true' }}\n"
        )),
        [format!("{CALL} runs for more than a push to main")]
    );
    for (report, at) in [
        ("release-red", "needs: [unit-tests, "),
        ("release-green", "needs: [plan, unit-tests, "),
    ] {
        let changed = edit(
            &format!(
                "{at}deployed, beta-macos, beta-macos-first-install, beta-macos-probe, beta-macos-isolation,"
            ),
            &format!("{at}deployed, beta-macos, beta-macos-first-install, beta-macos-probe,"),
        );
        assert_eq!(
            check_release(&changed),
            [format!("{report} does not need {CALL}")]
        );
    }
    assert_eq!(
        check_release(&edit(
            "needs: [plan, publish, beta-macos, beta-macos-pkg, beta-macos-first-install, beta-macos-probe]",
            "needs: [plan, publish, beta-macos, beta-macos-pkg, beta-macos-first-install, beta-macos-probe, beta-macos-isolation]"
        )),
        [format!(
            "promote-macos needs {CALL}, which reaches no verdict on a hosted runner"
        )]
    );
}
