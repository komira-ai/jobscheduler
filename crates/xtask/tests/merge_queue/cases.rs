//! Each edit of the real `ci.yml` or `docs/ci.md` that would let the queue merge what no job
//! checked, or hold the queue, is refused with exactly its own lines.

use crate::check::{
    AGGREGATE, ALWAYS, CANCEL, COMPARE_BASE, COMPARE_STEP, FAIL_IF, GROUP, GROUP_BASE,
    NOT_CANCELLED, problems, ruleset,
};
use crate::read;

/// Which file an edit changes.
#[derive(Clone, Copy)]
enum File {
    Ci,
    Doc,
}

/// The problems of the real files with `from` replaced by `to` in `file`; `from` must occur
/// exactly once.
fn edited(file: File, from: &str, to: &str) -> Vec<String> {
    let mut ci = read(".github/workflows/ci.yml");
    let mut doc = read("docs/ci.md");
    let text = match file {
        File::Ci => &mut ci,
        File::Doc => &mut doc,
    };
    assert_eq!(text.matches(from).count(), 1, "{from}");
    *text = text.replacen(from, to, 1);
    problems(&ci, &doc)
}

/// Asserts each edit gives exactly its lines.
fn refused(cases: &[(File, &str, &str, &[&str])]) {
    for (file, from, to, want) in cases {
        assert_eq!(edited(*file, from, to), *want, "{from} -> {to}");
    }
}

const MERGE_GROUP: &str = "  merge_group:\n    types: [checks_requested]\n    branches: [main]\n";
const QUEUE: &str = ", so a merge group of main's queue starts no run and all-passed never reports";

#[test]
fn a_trigger_that_does_not_start_a_merge_group_of_main_is_refused() {
    let no_group = [
        format!("on: no merge_group{QUEUE}"),
        format!("on: no merge_group{QUEUE}"),
    ];
    let no_group: Vec<&str> = no_group.iter().map(String::as_str).collect();
    let other_type =
        format!("on.merge_group.types is [\"destroyed\"], not [\"checks_requested\"]{QUEUE}");
    let no_type = format!("on.merge_group.types is [], not [\"checks_requested\"]{QUEUE}");
    let other_branch = format!("on.merge_group.branches is [\"release\"], not [\"main\"]{QUEUE}");
    let no_branch = format!("on.merge_group.branches is [], not [\"main\"]{QUEUE}");
    let both = [
        format!("on.merge_group.types is [], not [\"checks_requested\"]{QUEUE}"),
        format!("on.merge_group.branches is [], not [\"main\"]{QUEUE}"),
    ];
    let both: Vec<&str> = both.iter().map(String::as_str).collect();
    refused(&[
        (File::Ci, MERGE_GROUP, "", &no_group),
        (
            File::Ci,
            "[checks_requested]",
            "[destroyed]",
            &[&other_type],
        ),
        (File::Ci, "    types: [checks_requested]\n", "", &[&no_type]),
        (
            File::Ci,
            MERGE_GROUP,
            "  merge_group:\n    types: [checks_requested]\n    branches: [release]\n",
            &[&other_branch],
        ),
        (
            File::Ci,
            "    types: [checks_requested]\n    branches: [main]\n",
            "    types: [checks_requested]\n",
            &[&no_branch],
        ),
        (File::Ci, MERGE_GROUP, "  merge_group:\n", &both),
        (
            File::Ci,
            "    types: [opened, synchronize, reopened]\n",
            "    types: [opened, synchronize, reopened, labeled]\n",
            &[
                "on.pull_request.types is [\"opened\", \"synchronize\", \"reopened\", \"labeled\"], not [\"opened\", \"synchronize\", \"reopened\"]",
            ],
        ),
    ]);
}

#[test]
fn a_concurrency_key_a_merge_group_shares_or_a_cancelled_group_is_refused() {
    let number = "${{ github.workflow }}-${{ github.event.pull_request.number }}";
    let ref_key = "${{ github.workflow }}-${{ github.ref }}";
    let group = |got: &str| {
        format!(
            "concurrency.group is `{got}`, not `{GROUP}`: a merge group has no pull request number and needs a group of its own"
        )
    };
    let cancel = |got: &str| {
        format!(
            "concurrency.cancel-in-progress is `{got}`, not `{CANCEL}`: only a pull request's run may be cancelled by a newer one"
        )
    };
    for (from, to, want) in [
        (GROUP, number, group(number)),
        (GROUP, ref_key, group(ref_key)),
        (
            &format!("cancel-in-progress: {CANCEL}") as &str,
            "cancel-in-progress: true",
            cancel("true"),
        ),
        (
            &format!("cancel-in-progress: {CANCEL}"),
            "cancel-in-progress: ${{ github.event_name != 'push' }}",
            cancel("${{ github.event_name != 'push' }}"),
        ),
        (
            &format!("  cancel-in-progress: {CANCEL}\n"),
            "",
            cancel("missing"),
        ),
    ] {
        assert_eq!(edited(File::Ci, from, to), [want], "{to}");
    }
    let block = format!("concurrency:\n  group: {GROUP}\n  cancel-in-progress: {CANCEL}\n");
    assert_eq!(edited(File::Ci, &block, ""), ["concurrency: missing"]);
}

/// The `needs:` line of the aggregate.
const NEEDS: &str = "    needs: [lint, test, state-postgres, live-podman, agent-privilege, crun-rootless, crun-root, agent-egress, deny, coverage-shard, coverage]\n";

#[test]
fn an_aggregate_that_may_pass_without_every_job_is_refused() {
    let skipped = format!(
        "{AGGREGATE}: its if is `{NOT_CANCELLED}`, not `{ALWAYS}`: skipped after a failed job, and a skipped required check counts as passed"
    );
    let no_if = format!(
        "{AGGREGATE}: its if is `missing`, not `{ALWAYS}`: skipped after a failed job, and a skipped required check counts as passed"
    );
    let step = format!(
        "{AGGREGATE}: its steps are not the one step `if: {FAIL_IF}` `run: exit 1`, so it may pass when a job it needs did not"
    );
    let skip_clause = " || contains(needs.*.result, 'skipped')";
    let one_step = "        run: exit 1\n";
    refused(&[
        (
            File::Ci,
            NEEDS,
            &NEEDS.replace("deny, ", ""),
            &["all-passed: does not need deny, so the queue would merge whatever deny did"],
        ),
        (
            File::Ci,
            NEEDS,
            &NEEDS.replace(", coverage]", "]"),
            &["all-passed: does not need coverage, so the queue would merge whatever coverage did"],
        ),
        (
            File::Ci,
            "    if: ${{ always() }}\n",
            &format!("    if: {NOT_CANCELLED}\n"),
            &[&skipped],
        ),
        (File::Ci, "    if: ${{ always() }}\n", "", &[&no_if]),
        (File::Ci, skip_clause, "", &[&step]),
        (File::Ci, one_step, "        run: echo failed\n", &[&step]),
        (
            File::Ci,
            one_step,
            "        run: exit 1\n        continue-on-error: true\n",
            &[&step],
        ),
        (
            File::Ci,
            one_step,
            "        run: exit 1\n      - run: exit 0\n",
            &[&step],
        ),
    ]);
    // Renamed, the context the ruleset requires is gone, and the job is an ordinary one whose
    // condition keeps it off nothing.
    assert_eq!(
        edited(File::Ci, "  all-passed:\n", "  all-green:\n"),
        [
            "jobs: no job all-passed, the check main's ruleset requires".to_owned(),
            format!(
                "all-green: its if is `{ALWAYS}`; only `{NOT_CANCELLED}` keeps no job off a merge group"
            ),
        ]
    );
}

#[test]
fn a_condition_that_keeps_a_job_or_a_deciding_step_off_a_merge_group_is_refused() {
    let doctests = "- name: Doctests\n        if: ${{ !cancelled() && matrix.shard == 1 }}\n";
    let compare_if =
        "if: ${{ github.event_name == 'pull_request' || github.event_name == 'merge_group' }}";
    refused(&[
        (
            File::Ci,
            "  deny:\n    runs-on: ubuntu-latest\n",
            "  deny:\n    if: ${{ github.event_name != 'merge_group' }}\n    runs-on: ubuntu-latest\n",
            &[
                "deny: its if is `${{ github.event_name != 'merge_group' }}`; only `${{ !cancelled() }}` keeps no job off a merge group",
            ],
        ),
        (
            File::Ci,
            "  test:\n    runs-on: ubuntu-latest\n",
            "  test:\n    if: github.event_name == 'pull_request'\n    runs-on: ubuntu-latest\n",
            &[
                "test: its if is `github.event_name == 'pull_request'`; only `${{ !cancelled() }}` keeps no job off a merge group",
            ],
        ),
        (
            File::Ci,
            "  crun-root:\n    runs-on: ubuntu-24.04\n",
            "  crun-root:\n    runs-on: ubuntu-24.04\n    continue-on-error: true\n",
            &["crun-root: continue-on-error lets it fail without failing all-passed"],
        ),
        (
            File::Ci,
            compare_if,
            "if: ${{ github.event_name == 'pull_request' }}",
            &[
                "coverage, step Compare with the base branch: its if `${{ github.event_name == 'pull_request' }}` reads the event and does not run on a merge group",
            ],
        ),
        (
            File::Ci,
            doctests,
            "- name: Doctests\n        if: ${{ github.ref == 'refs/heads/main' }}\n",
            &[
                "test, step Doctests: its if `${{ github.ref == 'refs/heads/main' }}` reads the ref, which a merge group's is not main's; only a cache save may",
            ],
        ),
        (
            File::Ci,
            doctests,
            "- name: Doctests\n        if: ${{ github.event_name == 'push' }}\n",
            &[
                "test, step Doctests: its if `${{ github.event_name == 'push' }}` reads the event and does not run on a merge group",
            ],
        ),
    ]);
}

#[test]
fn a_read_of_the_pull_request_event_or_of_the_wrong_base_is_refused() {
    let site_base = "          BASE: ${{ github.base_ref }}\n          GROUP_BASE:";
    let site_env = format!("          GROUP_BASE: {GROUP_BASE}\n");
    let site_run = " --group-base \"$GROUP_BASE\"";
    let no_group_base = format!(
        "lint, step site: does not pass `{GROUP_BASE}` as --group-base, so a merge group's change is not read"
    );
    let compare =
        format!("coverage, step {COMPARE_STEP}: does not read the baseline of `{COMPARE_BASE}`");
    refused(&[
        (
            File::Ci,
            site_base,
            "          BASE: ${{ github.event.pull_request.base.ref }}\n          GROUP_BASE:",
            &["lint: reads github.event.pull_request, which a merge group does not have"],
        ),
        (
            File::Ci,
            site_base,
            "          BASE: ${{ github.head_ref }}\n          GROUP_BASE:",
            &["lint: reads github.head_ref, which is empty on a merge group"],
        ),
        (
            File::Ci,
            &site_env,
            "          GROUP_BASE: ${{ github.event.merge_group.head_sha }}\n",
            &[
                "lint: reads merge_group.head_sha, where its base_sha is meant (HEAD is the group's head)",
                "lint, step site: reads github.base_ref, empty on a merge group, and not github.event.merge_group.base_sha",
                &no_group_base,
            ],
        ),
        // The step before this change: the base branch alone.
        (
            File::Ci,
            &site_env,
            "",
            &[
                "lint, step site: reads github.base_ref, empty on a merge group, and not github.event.merge_group.base_sha",
                &no_group_base,
            ],
        ),
        (File::Ci, site_run, "", &[&no_group_base]),
        (
            File::Ci,
            COMPARE_BASE,
            "${{ format('origin/{0}', github.base_ref) }}",
            &[
                "coverage, step Compare with the base branch: reads github.base_ref, empty on a merge group, and not github.event.merge_group.base_sha",
                &compare,
            ],
        ),
        (
            File::Ci,
            COMPARE_BASE,
            "${{ github.event.merge_group.base_sha }}",
            &[&compare],
        ),
        (
            File::Ci,
            "git show \"$BASE:coverage-baseline\"",
            "git show \"HEAD^1:coverage-baseline\"",
            &[&compare],
        ),
        (
            File::Ci,
            "  lint:\n    runs-on: ubuntu-latest\n",
            "  lint:\n    runs-on: ubuntu-latest\n    env:\n      PR: ${{ github.base_ref }}\n",
            &[
                "lint: reads github.base_ref, empty on a merge group, and not github.event.merge_group.base_sha",
            ],
        ),
    ]);
    assert_eq!(
        edited(File::Ci, "      - id: site\n", "      - id: sites\n"),
        ["lint: no step `site`"]
    );
    assert_eq!(
        edited(File::Ci, &format!("name: {COMPARE_STEP}"), "name: Compare"),
        [format!("coverage: no step `{COMPARE_STEP}`")]
    );
}

#[test]
fn a_job_without_a_timeout_or_a_chain_as_long_as_the_queue_waits_is_refused() {
    let shard = "  coverage-shard:\n    runs-on: ubuntu-latest\n    timeout-minutes: 60\n";
    let queue = "\"check_response_timeout_minutes\": 360";
    refused(&[
        (
            File::Ci,
            "    timeout-minutes: 5\n",
            "",
            &["all-passed: no timeout-minutes, so it may run as long as the queue waits"],
        ),
        (
            File::Ci,
            shard,
            &shard.replace("60", "325"),
            &[
                "the longest chain of jobs (coverage-shard, coverage, all-passed) may run 360 minutes, not under the queue's check timeout of 360 (docs/ci.md)",
            ],
        ),
        (
            File::Doc,
            queue,
            "\"check_response_timeout_minutes\": 90",
            &[
                "the longest chain of jobs (coverage-shard, coverage, all-passed) may run 95 minutes, not under the queue's check timeout of 90 (docs/ci.md)",
            ],
        ),
        (
            File::Doc,
            queue,
            "\"check_response_timeout\": 360",
            &["docs/ci.md: the ruleset has no check_response_timeout_minutes"],
        ),
        (
            File::Doc,
            "{ \"context\": \"all-passed\", \"integration_id\": 15368 }",
            "{ \"context\": \"test\", \"integration_id\": 15368 }",
            &["docs/ci.md: the ruleset requires [\"test\"], not [\"all-passed\"]"],
        ),
        (
            File::Doc,
            "\n## The merge queue\n",
            "\n## Merging\n",
            &["docs/ci.md: no JSON ruleset in \"The merge queue\""],
        ),
    ]);
    // A job one minute under the timeout passes: the bound is strict, not off by one.
    assert_eq!(
        edited(File::Ci, shard, &shard.replace("60", "324")),
        Vec::<String>::new()
    );
}

#[test]
fn the_readers_see_each_spelling_they_are_given() {
    // Text that is not YAML is one line naming it.
    let bad = problems("on: [\n", &read("docs/ci.md"));
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].starts_with("ci.yml: line "), "{bad:?}");
    // No `on` and no jobs: every check reports what it misses, once.
    assert_eq!(
        problems("name: ci\n", &read("docs/ci.md")),
        [
            "on: missing",
            "concurrency: missing",
            "jobs: no job all-passed, the check main's ruleset requires",
            "lint: no step `site`",
            "coverage: no step `Compare with the base branch`",
        ]
    );
    // The ruleset is the first JSON block of its own section, not of a later one.
    let doc = read("docs/ci.md");
    assert!(ruleset(&doc).is_some());
    let moved = doc.replacen("```json\n", "```text\n", 1) + "\n## Later\n\n```json\n{}\n```\n";
    assert!(ruleset(&moved).is_none());
    // A block that is not JSON is no ruleset.
    assert!(ruleset(&doc.replacen("\"rules\": [", "\"rules\": [,", 1)).is_none());
    // A timeout that is not a number is no timeout.
    assert_eq!(
        edited(
            File::Ci,
            "    timeout-minutes: 5\n",
            "    timeout-minutes: five\n"
        ),
        ["all-passed: no timeout-minutes, so it may run as long as the queue waits"]
    );
}
