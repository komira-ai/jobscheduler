//! The beta stage's run of the two-run isolation test (`.github/workflows/beta.yml`, job
//! `isolation`) read as it is in the repository: a job of its own on a hosted Mac, which runs
//! `cargo xtask mac-root-tests` for the `native_isolation` target alone, from a work directory
//! outside every directory the test searches for what a run left (`FIND_ROOTS` of
//! `crates/js-agent/tests/native_isolation/main.rs`), and whose failure is not passed over; and
//! release.yml's `promote-macos`, which needs `beta-macos`, the call of beta.yml, and runs only
//! when it passed.

use std::path::Path;

const JOB: &str = "isolation";
const ISOLATION: &str = "crates/js-agent/tests/native_isolation/main.rs";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The trimmed lines, comments left out, of the top-level job `name` of `workflow`: from its
/// `  <name>:` line to the next line indented two spaces or less.
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

/// One line per way the job of `beta_yml` is not the isolation test's own run, given the
/// test's source `isolation_src`.
fn check_job(beta_yml: &str, isolation_src: &str) -> Vec<String> {
    let Some(job) = job_lines(beta_yml, JOB) else {
        return vec![format!("beta.yml has no job `{JOB}`")];
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

/// One line per way release.yml's `promote-macos` would add the macOS archives without the
/// isolation job's pass: `beta-macos` does not call beta.yml, `promote-macos` does not need
/// it, or its condition runs it after a failure.
fn check_release(release_yml: &str) -> Vec<String> {
    let mut found = Vec::new();
    let calls = job_lines(release_yml, "beta-macos")
        .is_some_and(|job| job.contains(&"uses: ./.github/workflows/beta.yml"));
    if !calls {
        found.push("release.yml's beta-macos does not call beta.yml".to_owned());
    }
    let Some(promote) = job_lines(release_yml, "promote-macos") else {
        found.push("release.yml has no job promote-macos".to_owned());
        return found;
    };
    let needs = promote
        .iter()
        .find_map(|l| l.strip_prefix("needs: "))
        .unwrap_or_default();
    let needs: Vec<&str> = needs
        .trim_matches(['[', ']'])
        .split(',')
        .map(str::trim)
        .collect();
    if !needs.contains(&"beta-macos") {
        found.push("promote-macos does not need beta-macos".to_owned());
    }
    let condition = promote
        .iter()
        .find_map(|l| l.strip_prefix("if: "))
        .unwrap_or_default();
    for status in ["always()", "failure()", "cancelled()"] {
        if condition.contains(status) {
            found.push(format!(
                "promote-macos's condition has {status}: it runs after a failed need"
            ));
        }
    }
    found
}

/// Asserts that the repository's beta stage runs the isolation test on a Mac of its own and
/// that its failure holds back the macOS archives.
#[test]
fn the_beta_stage_runs_the_isolation_test_and_promotion_waits_for_it() {
    let empty: Vec<String> = Vec::new();
    assert_eq!(
        check_job(&read(".github/workflows/beta.yml"), &read(ISOLATION)),
        empty
    );
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
/// Catches a check that passes a beta stage whose isolation run is wrong.
#[test]
fn a_beta_job_that_breaks_the_isolation_run_is_refused() {
    let beta = read(".github/workflows/beta.yml");
    let src = read(ISOLATION);
    let edit = |from: &str, to: &str| {
        assert!(beta.contains(from), "{from}");
        beta.replacen(from, to, 1)
    };
    let run = "run: cargo xtask mac-root-tests --work /private/var/js-mac-root-tests --only native_isolation";
    assert!(beta.contains(run));
    let with_run = |to: &str| edit(run, &format!("run: {to}"));

    assert_eq!(
        check_job(&edit("\n  isolation:\n", "\n  elsewhere:\n"), &src),
        ["beta.yml has no job `isolation`"]
    );
    let runner = "  isolation:\n    runs-on: macos-15\n";
    assert!(beta.contains(runner));
    assert_eq!(
        check_job(
            &edit(runner, "  isolation:\n    runs-on: ubuntu-24.04\n"),
            &src
        ),
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
        check_job(&beta, &src.replace("const FIND_ROOTS", "const RENAMED")),
        [format!(
            "{ISOLATION} has no FIND_ROOTS to check the work directory against"
        )]
    );
}

/// Asserts that release.yml's promotion of the macOS archives is refused when it would not
/// wait for the beta.yml call's pass: the call gone or pointing elsewhere, the need dropped,
/// the job gone, or a condition that runs it after a failure.
///
/// Catches a check that passes a release that promotes the macOS archives over a failed
/// isolation run.
#[test]
fn a_promotion_that_does_not_wait_for_the_isolation_run_is_refused() {
    let release = read(".github/workflows/release.yml");
    let edit = |from: &str, to: &str| {
        assert!(release.contains(from), "{from}");
        release.replacen(from, to, 1)
    };
    let call = "    uses: ./.github/workflows/beta.yml\n";
    assert_eq!(
        check_release(&edit(
            call,
            "    uses: ./.github/workflows/macos-probe.yml\n"
        )),
        ["release.yml's beta-macos does not call beta.yml"]
    );
    let needs = "needs: [plan, publish, beta-macos, beta-macos-pkg,";
    assert_eq!(
        check_release(&edit(needs, "needs: [plan, publish, beta-macos-pkg,")),
        ["promote-macos does not need beta-macos"]
    );
    assert_eq!(
        check_release(&edit("\n  promote-macos:\n", "\n  promote-other:\n")),
        ["release.yml has no job promote-macos"]
    );
    let gate = "  promote-macos:\n";
    let at = release.find(gate).expect("promote-macos");
    let condition = "    if: ${{ github.event_name == 'push'";
    let cond_at = at + release[at..].find(condition).expect("its condition");
    for (status, to) in [
        (
            "always()",
            "    if: ${{ always() && github.event_name == 'push'",
        ),
        (
            "failure()",
            "    if: ${{ (success() || failure()) && github.event_name == 'push'",
        ),
        (
            "cancelled()",
            "    if: ${{ !cancelled() && github.event_name == 'push'",
        ),
    ] {
        let changed = format!(
            "{}{to}{}",
            &release[..cond_at],
            &release[cond_at + condition.len()..]
        );
        assert_eq!(
            check_release(&changed),
            [format!(
                "promote-macos's condition has {status}: it runs after a failed need"
            )]
        );
    }
}
