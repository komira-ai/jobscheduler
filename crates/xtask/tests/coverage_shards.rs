//! ci.yml's coverage shards, the job that builds the instrumented test binaries and the job
//! that reports over them, read as text.
//!
//! `coverage-shard` runs the workspace's tests in parts, one per entry of its `shard`
//! matrix, through `cargo xtask coverage-shard` and the plan in
//! `.config/coverage-shards.toml`, whose `shards` is the number of parts; each part uploads
//! its profile as `coverage-profile-<shard>`. `coverage-build` builds the instrumented test
//! binaries once and uploads them as the archive `coverage-tests`. `coverage` needs both,
//! downloads each profile by name and the archive, and reports with the archive. A shard
//! whose profile the report did not read would drop its tests' lines and branches from the
//! report, so the two lists must be the same: every shard is downloaded once, and nothing
//! else is.

use std::collections::BTreeSet;
use std::path::Path;

/// The text of job `name` in `workflow`: its key line and every line under it, up to
/// the next job (a line indented two spaces) or the end of the file.
fn job<'a>(workflow: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\n  {name}:\n");
    let start = workflow.find(&key)? + 1;
    let body = &workflow[start + key.len() - 1..];
    let end = body
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .find(|&i| {
            let next = &body[i..];
            next.starts_with("  ") && !next.starts_with("   ") && !next.starts_with("  #")
        })
        .unwrap_or(body.len());
    Some(&workflow[start..start + key.len() - 1 + end])
}

/// The value of the first `<key>: ` line of `text`, trimmed.
fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix(&format!("{key}: ")))
        .map(str::trim)
}

/// The one line of the `coverage-shard` matrix, as ci.yml spells it.
const MATRIX: &str = "shard: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]";

/// The path of the archive `coverage-build` writes and `coverage` reads.
const ARCHIVE: &str = "target/coverage-tests.tar.zst";

/// What is wrong with the coverage jobs of `workflow`, whose plan has `plan_shards` shards;
/// empty when they agree.
fn check(workflow: &str, plan_shards: usize) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(shard) = job(workflow, "coverage-shard") else {
        return vec!["no coverage-shard job".to_owned()];
    };
    let Some(build) = job(workflow, "coverage-build") else {
        return vec!["no coverage-build job".to_owned()];
    };
    let Some(report) = job(workflow, "coverage") else {
        return vec!["no coverage job".to_owned()];
    };
    let listed: Vec<String> = match value(shard, "shard") {
        Some(list) if list.starts_with('[') && list.ends_with(']') => list[1..list.len() - 1]
            .split(',')
            .map(|s| s.trim().to_owned())
            .collect(),
        _ => return vec!["coverage-shard has no `shard: [...]` matrix".to_owned()],
    };
    let counted: Vec<String> = (1..=listed.len()).map(|k| k.to_string()).collect();
    if listed != counted {
        errors.push("the matrix `shard` list is not 1, 2, ... up to its length".to_owned());
    }
    if listed.len() != plan_shards {
        errors.push(format!(
            "the matrix runs {} shards, .config/coverage-shards.toml plans {plan_shards}",
            listed.len()
        ));
    }
    let shards: BTreeSet<String> = listed.into_iter().collect();
    let run = "xtask coverage-shard --plan .config/coverage-shards.toml --shard ${{ matrix.shard }} --of ${{ strategy.job-total }} --";
    if !shard.contains(run) {
        errors.push(
            "the shard's run does not go through xtask coverage-shard with the plan, \
             <matrix.shard> and <strategy.job-total>"
                .to_owned(),
        );
    }
    if !shard.contains("name: coverage-profile-${{ matrix.shard }}\n") {
        errors.push("the shard does not upload coverage-profile-<shard>".to_owned());
    }
    // The shard's merged profile is the file it uploads.
    let merged = r#"xtask merge-profiles --dir target/llvm-cov-target --out "target/jobscheduler-shard-${{ matrix.shard }}.profraw""#;
    let uploaded = "path: target/jobscheduler-shard-${{ matrix.shard }}.profraw\n";
    if !(shard.contains(merged) && shard.contains(uploaded)) {
        errors.push("the shard does not upload the profile it merged".to_owned());
    }
    // The archive the build job writes is the file it uploads.
    let archived = format!(
        "llvm-cov nextest-archive --workspace --locked --features js-sim-cell/slow-families --cargo-profile coverage --branch --archive-file {ARCHIVE}"
    );
    let archive_upload = format!("name: coverage-tests\n          path: {ARCHIVE}\n");
    if !(build.contains(&archived) && build.contains(&archive_upload)) {
        errors.push(
            "coverage-build does not upload the archive it wrote as coverage-tests".to_owned(),
        );
    }
    if value(report, "needs") != Some("[coverage-shard, coverage-build]") {
        errors.push("coverage does not need coverage-shard and coverage-build".to_owned());
    }
    if !report.contains(&format!(
        "llvm-cov report --nextest-archive-file {ARCHIVE} "
    )) {
        errors.push(format!(
            "coverage does not report with --nextest-archive-file {ARCHIVE}"
        ));
    }
    let mut downloaded = Vec::new();
    let mut archive_downloaded = false;
    let lines: Vec<&str> = report.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        if !line.contains("uses: actions/download-artifact@") {
            continue;
        }
        // The step's own lines: up to the next step.
        let name = lines[at + 1..]
            .iter()
            .take_while(|l| !l.trim_start().starts_with("- "))
            .find_map(|l| l.trim().strip_prefix("name: "));
        match name {
            Some("coverage-tests") => archive_downloaded = true,
            Some(name) => match name.strip_prefix("coverage-profile-") {
                Some(k) => downloaded.push(k.to_owned()),
                None => errors.push(
                    "a download in coverage names neither a coverage profile nor coverage-tests"
                        .to_owned(),
                ),
            },
            None => errors.push("a download in coverage names no artifact".to_owned()),
        }
    }
    if !archive_downloaded {
        errors.push("coverage does not download coverage-tests".to_owned());
    }
    let unique: BTreeSet<String> = downloaded.iter().cloned().collect();
    if unique.len() != downloaded.len() {
        errors.push(format!("a shard is downloaded twice: {downloaded:?}"));
    }
    if unique != shards {
        errors.push(format!(
            "coverage downloads the profiles of shards {unique:?}, the matrix runs {shards:?}"
        ));
    }
    errors
}

fn ci() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml");
    std::fs::read_to_string(path).expect("read ci.yml")
}

/// The `shards` of `.config/coverage-shards.toml`.
fn plan_shards() -> usize {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.config/coverage-shards.toml");
    let plan: toml::Table = std::fs::read_to_string(path)
        .expect("read the coverage shard plan")
        .parse()
        .expect("the coverage shard plan parses");
    let shards = plan
        .get("shards")
        .and_then(toml::Value::as_integer)
        .expect("the plan has an integer `shards`");
    usize::try_from(shards).expect("the plan's `shards` is not negative")
}

/// The profile numbers 1 to `n` as a set, the way the error messages print them.
fn set(numbers: impl IntoIterator<Item = usize>) -> String {
    format!(
        "{:?}",
        numbers
            .into_iter()
            .map(|k| k.to_string())
            .collect::<BTreeSet<String>>()
    )
}

/// Asserts that ci.yml's report job reads the profile of every shard its matrix runs, and
/// of no other, that the matrix is as long as the plan, and that the report reads the
/// archive the build job wrote.
#[test]
fn the_report_reads_every_shard_of_ci_yml() {
    assert_eq!(check(&ci(), plan_shards()), Vec::<String>::new());
}

/// Asserts that the plan's shard count is the matrix's length, and that a plan of another
/// count is refused.
#[test]
fn the_matrix_is_as_long_as_the_plan() {
    let ci = ci();
    assert_eq!(plan_shards(), 12);
    assert_eq!(
        check(&ci, 11),
        vec!["the matrix runs 12 shards, .config/coverage-shards.toml plans 11".to_owned()]
    );
}

/// Asserts that a thirteenth shard with no download of its profile, and a matrix longer
/// than the plan, are refused.
#[test]
fn a_shard_whose_profile_is_not_downloaded_is_refused() {
    let ci = ci();
    let more = ci.replacen(
        MATRIX,
        "shard: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]",
        1,
    );
    assert_ne!(more, ci);
    assert_eq!(
        check(&more, 12),
        vec![
            "the matrix runs 13 shards, .config/coverage-shards.toml plans 12".to_owned(),
            format!(
                "coverage downloads the profiles of shards {}, the matrix runs {}",
                set(1..=12),
                set(1..=13)
            ),
        ]
    );
}

/// Asserts that a matrix that does not count from 1 without a gap is refused.
#[test]
fn a_matrix_that_skips_a_number_is_refused() {
    let ci = ci();
    let gap = ci.replacen(MATRIX, "shard: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13]", 1);
    assert_ne!(gap, ci);
    assert_eq!(
        check(&gap, 12),
        vec![
            "the matrix `shard` list is not 1, 2, ... up to its length".to_owned(),
            format!(
                "coverage downloads the profiles of shards {}, the matrix runs {}",
                set(1..=12),
                set((1..=11).chain([13]))
            ),
        ]
    );
}

/// Asserts that downloading one shard's profile twice, in place of another's, is refused.
#[test]
fn a_profile_downloaded_twice_is_refused() {
    let ci = ci();
    let twice = ci.replacen(
        "name: coverage-profile-3\n",
        "name: coverage-profile-2\n",
        1,
    );
    assert_ne!(twice, ci);
    let listed: Vec<String> = [1, 2, 2]
        .into_iter()
        .chain(4..=12)
        .map(|k| k.to_string())
        .collect();
    assert_eq!(
        check(&twice, 12),
        vec![
            format!("a shard is downloaded twice: {listed:?}"),
            format!(
                "coverage downloads the profiles of shards {}, the matrix runs {}",
                set((1..=12).filter(|&k| k != 3)),
                set(1..=12)
            ),
        ]
    );
}

/// Asserts that a run that skips the plan or the shard verb, a shard number or count
/// written by hand, a renamed upload, an upload of another file than the merged profile,
/// an archive that is not the one uploaded, a report that does not read the archive, and a
/// report job that does not wait for the shards and the build are each refused.
#[test]
fn each_broken_link_between_the_jobs_is_refused() {
    let ci = ci();
    let one = |from: &str, to: &str, error: &str| {
        assert_eq!(ci.matches(from).count(), 1, "{from}");
        assert_eq!(
            check(&ci.replacen(from, to, 1), 12),
            vec![error.to_owned()],
            "{from}"
        );
    };
    let run_error = "the shard's run does not go through xtask coverage-shard with the plan, \
                     <matrix.shard> and <strategy.job-total>";
    one("--of ${{ strategy.job-total }}", "--of 12", run_error);
    one("--shard ${{ matrix.shard }}", "--shard 1", run_error);
    one(
        "--plan .config/coverage-shards.toml",
        "--plan other.toml",
        run_error,
    );
    one(
        "xtask coverage-shard --plan",
        "xtask merge-profiles --plan",
        run_error,
    );
    one(
        "${{ strategy.job-total }} -- cargo",
        "${{ strategy.job-total }} cargo",
        run_error,
    );
    one(
        "name: coverage-profile-${{ matrix.shard }}\n",
        "name: profile-${{ matrix.shard }}\n",
        "the shard does not upload coverage-profile-<shard>",
    );
    one(
        "path: target/jobscheduler-shard-${{ matrix.shard }}.profraw\n",
        "path: target/llvm-cov-target/*.profraw\n",
        "the shard does not upload the profile it merged",
    );
    one(
        r#"--out "target/jobscheduler-shard-${{ matrix.shard }}.profraw""#,
        r#"--out "target/merged.profraw""#,
        "the shard does not upload the profile it merged",
    );
    one(
        "--archive-file target/coverage-tests.tar.zst",
        "--archive-file target/other.tar.zst",
        "coverage-build does not upload the archive it wrote as coverage-tests",
    );
    one(
        "path: target/coverage-tests.tar.zst\n",
        "path: target/other.tar.zst\n",
        "coverage-build does not upload the archive it wrote as coverage-tests",
    );
    one(
        "needs: [coverage-shard, coverage-build]\n",
        "needs: coverage-shard\n",
        "coverage does not need coverage-shard and coverage-build",
    );
    one(
        "llvm-cov report --nextest-archive-file target/coverage-tests.tar.zst ",
        "llvm-cov report ",
        "coverage does not report with --nextest-archive-file target/coverage-tests.tar.zst",
    );
    let renamed = ci.replacen("name: coverage-profile-1\n", "name: something-else\n", 1);
    assert_eq!(
        check(&renamed, 12),
        vec![
            "a download in coverage names neither a coverage profile nor coverage-tests".to_owned(),
            format!(
                "coverage downloads the profiles of shards {}, the matrix runs {}",
                set(2..=12),
                set(1..=12)
            ),
        ]
    );
}

/// Asserts that a report job that does not download the archive, or downloads an artifact
/// with no name, is refused.
#[test]
fn a_report_that_does_not_download_the_archive_is_refused() {
    let ci = ci();
    let from = "name: coverage-tests\n          path: target\n";
    assert_eq!(ci.matches(from).count(), 1);
    assert_eq!(
        check(
            &ci.replacen(from, "name: other\n          path: target\n", 1),
            12
        ),
        vec![
            "a download in coverage names neither a coverage profile nor coverage-tests".to_owned(),
            "coverage does not download coverage-tests".to_owned(),
        ]
    );
    assert_eq!(
        check(&ci.replacen(from, "path: target\n", 1), 12),
        vec![
            "a download in coverage names no artifact".to_owned(),
            "coverage does not download coverage-tests".to_owned(),
        ]
    );
}

/// Asserts that a workflow without any of the three jobs, or without the matrix, is refused
/// by name.
#[test]
fn a_missing_job_or_matrix_is_refused() {
    let ci = ci();
    assert_eq!(
        check(
            &ci.replacen("\n  coverage-shard:\n", "\n  shards:\n", 1),
            12
        ),
        vec!["no coverage-shard job".to_owned()]
    );
    assert_eq!(
        check(
            &ci.replacen("\n  coverage-build:\n", "\n  archive:\n", 1),
            12
        ),
        vec!["no coverage-build job".to_owned()]
    );
    assert_eq!(
        check(&ci.replacen("\n  coverage:\n", "\n  report:\n", 1), 12),
        vec!["no coverage job".to_owned()]
    );
    assert_eq!(
        check(&ci.replacen(MATRIX, "shard: 1", 1), 12),
        vec!["coverage-shard has no `shard: [...]` matrix".to_owned()]
    );
}
