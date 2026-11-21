//! The merge queue's check: `ci.yml` runs every job `all-passed` needs on a merge group, under
//! the same name as on a pull request, and `docs/ci.md` holds the ruleset that requires it.
//!
//! `main` merges through GitHub's merge queue, which requires one check, `all-passed`, the last
//! job of `ci.yml` (docs/ci.md, "The merge queue"). A required check that is skipped counts as
//! passed, and a check that never reports holds the queue until its timeout, so [`check`] reads
//! the workflow as YAML ([`yaml`], the parser of `tests/apt_workflow`) and refuses:
//!
//! - a trigger that does not start the workflow for a merge group of `main` (`merge_group`,
//!   type `checks_requested`, branch `main`) beside a pull request and a push to main;
//! - a concurrency key that a merge group shares with another run, or a cancel of a run that
//!   is not a pull request's;
//! - no job `all-passed`, or one that does not need every other job, does not run `always()`,
//!   or whose step does not fail on every result but success;
//! - a job condition other than `!cancelled()`, a job that may fail without failing the
//!   aggregate (`continue-on-error`), a step condition that reads the event without running for
//!   a merge group, or one that reads the ref and does not save a cache;
//! - a read of the pull request's event (`github.event.pull_request`, `github.head_ref`) in a
//!   job, a base branch (`github.base_ref`) read in a step that does not also read the merge
//!   group's `base_sha`, and any read of the group's `head_sha`;
//! - the site step and the coverage comparison without the merge group's base;
//! - a job with no `timeout-minutes`, a chain of jobs through `needs` that may run as long as
//!   the queue's check timeout in `docs/ci.md`, and a ruleset there that requires anything but
//!   `all-passed`.
//!
//! [`cases`] applies one edit at a time to the real files and asserts the exact lines refused.

mod cases;
mod check;
#[allow(
    dead_code,
    reason = "shared with tests/apt_workflow; this target reads only part of it"
)]
#[path = "../apt_workflow/yaml.rs"]
mod yaml;

use check::{AGGREGATE, get, problems, ruleset, strs};

/// The text of the repository file `rel`.
pub fn read(rel: &str) -> String {
    std::fs::read_to_string(format!("{}/../../{rel}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Every workflow of the repository: file name and text.
fn workflows() -> Vec<(String, String)> {
    let dir = format!("{}/../../.github/workflows", env!("CARGO_MANIFEST_DIR"));
    let mut found: Vec<(String, String)> = std::fs::read_dir(dir)
        .expect("the workflows")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .map(|p| {
            let name = p.file_name().expect("name").to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&p).expect("read");
            (name, text)
        })
        .collect();
    found.sort();
    found
}

#[test]
fn the_real_ci_workflow_runs_every_required_job_on_a_merge_group() {
    assert_eq!(
        problems(&read(".github/workflows/ci.yml"), &read("docs/ci.md")),
        Vec::<String>::new()
    );
    // Not vacuous: the aggregate needs the jobs a pull request has always run.
    let root = yaml::load(&read(".github/workflows/ci.yml")).expect("ci.yml");
    let jobs = get(&root, "jobs").expect("jobs");
    let needs = strs(get(get(jobs, AGGREGATE).expect("the aggregate"), "needs"));
    for job in ["lint", "test", "deny", "coverage-shard", "coverage"] {
        assert!(needs.iter().any(|n| n == job), "{job} in {needs:?}");
    }
}

#[test]
fn the_required_check_has_one_source() {
    // Only ci.yml has a job `all-passed`, so no other workflow's run can report the context.
    let mut sources = Vec::new();
    for (file, text) in workflows() {
        let root = yaml::load(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
        if get(&root, "jobs").and_then(|j| get(j, AGGREGATE)).is_some() {
            sources.push(file);
        }
    }
    assert_eq!(sources, ["ci.yml"]);
}

#[test]
fn the_documented_ruleset_is_the_queue_the_section_describes() {
    let ruleset = ruleset(&read("docs/ci.md")).expect("the ruleset");
    let queue = ruleset["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .find(|r| r["type"] == "merge_queue")
        .expect("the merge_queue rule");
    assert_eq!(
        queue["parameters"],
        serde_json::json!({
            "merge_method": "SQUASH",
            "grouping_strategy": "ALLGREEN",
            "max_entries_to_build": 5,
            "min_entries_to_merge": 1,
            "max_entries_to_merge": 5,
            "min_entries_to_merge_wait_minutes": 5,
            "check_response_timeout_minutes": 360
        })
    );
    assert_eq!(
        ruleset["conditions"]["ref_name"]["include"],
        serde_json::json!(["refs/heads/main"])
    );
    // From GitHub Actions alone (its app id), and not held to an up-to-date branch: the queue
    // builds each pull request on top of what is ahead of it.
    let checks = ruleset["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .find(|r| r["type"] == "required_status_checks")
        .expect("the required checks");
    assert_eq!(
        checks["parameters"]["required_status_checks"],
        serde_json::json!([{ "context": AGGREGATE, "integration_id": 15368 }])
    );
    assert_eq!(
        checks["parameters"]["strict_required_status_checks_policy"],
        false
    );
}
