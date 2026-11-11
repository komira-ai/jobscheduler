//! ci.yml's coverage shards and the job that reports over them, read as text.
//!
//! `coverage-shard` runs the workspace's tests in parts, one per entry of its `shard`
//! matrix, each part nextest's `--partition slice:<shard>/<number of shards>`, and
//! uploads its profile as `coverage-profile-<shard>`. `coverage` needs it and downloads
//! each profile by that name. A shard whose profile the report did not read would drop
//! its tests' lines and branches from the report, so the two lists must be the same:
//! every shard is downloaded once, and nothing else is.

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

/// What is wrong with the coverage jobs of `workflow`; empty when they agree.
fn check(workflow: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(shard) = job(workflow, "coverage-shard") else {
        return vec!["no coverage-shard job".to_owned()];
    };
    let Some(report) = job(workflow, "coverage") else {
        return vec!["no coverage job".to_owned()];
    };
    let shards: BTreeSet<String> = match value(shard, "shard") {
        Some(list) if list.starts_with('[') && list.ends_with(']') => list[1..list.len() - 1]
            .split(',')
            .map(|s| s.trim().to_owned())
            .collect(),
        _ => return vec!["coverage-shard has no `shard: [...]` matrix".to_owned()],
    };
    if value(shard, "SHARDS") != Some("${{ strategy.job-total }}") {
        errors.push("SHARDS is not the matrix's size (strategy.job-total)".to_owned());
    }
    if !shard.contains(r#"--partition "slice:$SHARD/$SHARDS""#) {
        errors.push("the shard's run is not partitioned slice:$SHARD/$SHARDS".to_owned());
    }
    if !shard.contains("name: coverage-profile-${{ matrix.shard }}\n") {
        errors.push("the shard does not upload coverage-profile-<shard>".to_owned());
    }
    if value(report, "needs") != Some("coverage-shard") {
        errors.push("coverage does not need coverage-shard".to_owned());
    }
    let mut downloaded = Vec::new();
    let lines: Vec<&str> = report.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        if !line.contains("uses: actions/download-artifact@") {
            continue;
        }
        // The step's own lines: up to the next step.
        let name = lines[at + 1..]
            .iter()
            .take_while(|l| !l.trim_start().starts_with("- "))
            .find_map(|l| l.trim().strip_prefix("name: coverage-profile-"));
        match name {
            Some(name) => downloaded.push(name.to_owned()),
            None => errors.push("a download in coverage names no coverage profile".to_owned()),
        }
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

/// Asserts that ci.yml's report job reads the profile of every shard its matrix runs, and
/// of no other.
#[test]
fn the_report_reads_every_shard_of_ci_yml() {
    assert_eq!(check(&ci()), Vec::<String>::new());
}

/// Asserts that a fourth shard with no download of its profile is refused.
#[test]
fn a_shard_whose_profile_is_not_downloaded_is_refused() {
    let ci = ci();
    let four = ci.replacen("shard: [1, 2, 3]", "shard: [1, 2, 3, 4]", 1);
    assert_ne!(four, ci);
    assert_eq!(
        check(&four),
        vec![
            r#"coverage downloads the profiles of shards {"1", "2", "3"}, the matrix runs {"1", "2", "3", "4"}"#
                .to_owned()
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
    assert_eq!(
        check(&twice),
        vec![
            r#"a shard is downloaded twice: ["1", "2", "2"]"#.to_owned(),
            r#"coverage downloads the profiles of shards {"1", "2"}, the matrix runs {"1", "2", "3"}"#
                .to_owned(),
        ]
    );
}

/// Asserts that a shard count written by hand, an unpartitioned run, a renamed upload, and a
/// report job that does not wait for the shards are each refused.
#[test]
fn each_broken_link_between_the_jobs_is_refused() {
    let ci = ci();
    let one = |from: &str, to: &str, error: &str| {
        assert_eq!(ci.matches(from).count(), 1, "{from}");
        assert_eq!(
            check(&ci.replacen(from, to, 1)),
            vec![error.to_owned()],
            "{from}"
        );
    };
    one(
        "SHARDS: ${{ strategy.job-total }}",
        "SHARDS: 3",
        "SHARDS is not the matrix's size (strategy.job-total)",
    );
    one(
        r#" --partition "slice:$SHARD/$SHARDS""#,
        "",
        "the shard's run is not partitioned slice:$SHARD/$SHARDS",
    );
    one(
        "name: coverage-profile-${{ matrix.shard }}\n",
        "name: profile-${{ matrix.shard }}\n",
        "the shard does not upload coverage-profile-<shard>",
    );
    one(
        "needs: coverage-shard\n",
        "needs: lint\n",
        "coverage does not need coverage-shard",
    );
    let renamed = ci.replacen("name: coverage-profile-1\n", "name: something-else\n", 1);
    assert_eq!(
        check(&renamed),
        vec![
            "a download in coverage names no coverage profile".to_owned(),
            r#"coverage downloads the profiles of shards {"2", "3"}, the matrix runs {"1", "2", "3"}"#
                .to_owned(),
        ]
    );
}

/// Asserts that a workflow without either job, or without the matrix, is refused by name.
#[test]
fn a_missing_job_or_matrix_is_refused() {
    let ci = ci();
    assert_eq!(
        check(&ci.replacen("\n  coverage-shard:\n", "\n  shards:\n", 1)),
        vec!["no coverage-shard job".to_owned()]
    );
    assert_eq!(
        check(&ci.replacen("\n  coverage:\n", "\n  report:\n", 1)),
        vec!["no coverage job".to_owned()]
    );
    assert_eq!(
        check(&ci.replacen("shard: [1, 2, 3]", "shard: 1", 1)),
        vec!["coverage-shard has no `shard: [...]` matrix".to_owned()]
    );
}
