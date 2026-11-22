//! The smoke bundle's jobs (`smoke-bundle-pack`, `smoke-bundle-push`, `smoke-bundle-check`), as
//! the text of release.yml shows them: each runs for a release on main alone, on the plan's word;
//! only the push holds `packages: write`, it holds nothing else, and it checks out and builds
//! nothing, the token going to the push step alone through standard input; the pack and the
//! check hold `contents: read` alone; they run in that order after `deploy-tool`; no job of the
//! deploy path waits for them, and `release-red` and `release-green` do.

use super::fast_lane::{code, needs, permissions, refused, runs_no_repository_code};
use super::{holders, job, jobs, workflow};

/// The condition of each smoke-bundle job.
const RELEASE: &str = "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}";

/// Each job, the permissions it holds, and what it needs.
const JOBS: [(&str, &str, &str); 3] = [
    ("smoke-bundle-pack", "contents: read", "plan, deploy-tool"),
    (
        "smoke-bundle-push",
        "packages: write",
        "plan, deploy-tool, smoke-bundle-pack",
    ),
    (
        "smoke-bundle-check",
        "contents: read",
        "plan, deploy-tool, smoke-bundle-push",
    ),
];

/// The jobs whose end a release reports: they wait for the check, and so for the other two.
const REPORTERS: [&str; 2] = ["release-red", "release-green"];

/// What is wrong with the smoke bundle's jobs in `text`.
fn smoke_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let mut problems = Vec::new();
    for (name, grant, wanted) in JOBS {
        let Some((_, lines)) = all.iter().find(|(n, _)| n == name) else {
            problems.push(format!("no job {name}"));
            continue;
        };
        let conditions: Vec<&str> = lines
            .iter()
            .filter(|l| l.starts_with("    if:"))
            .map(String::as_str)
            .collect();
        if conditions != [RELEASE] {
            problems.push(format!("{name} runs on {conditions:?}"));
        }
        if permissions(lines) != Some(vec![grant.to_owned()]) {
            problems.push(format!(
                "{name} holds {:?}, not {grant}",
                permissions(lines)
            ));
        }
        if needs(lines).join(", ") != wanted {
            problems.push(format!("{name} needs {:?}, not [{wanted}]", needs(lines)));
        }
    }
    if holders(text, "packages: write") != ["smoke-bundle-push"] {
        problems.push(format!(
            "packages: write is held by {:?}",
            holders(text, "packages: write")
        ));
    }
    if let Some((_, push)) = all.iter().find(|(n, _)| n == "smoke-bundle-push") {
        problems.extend(runs_no_repository_code("smoke-bundle-push", push));
        let push = code(push);
        if push.matches("github.token").count() != 1
            || !push.contains("--password-file /dev/stdin <<< \"$GH_TOKEN\"")
        {
            problems
                .push("the push token goes elsewhere than the push's standard input".to_owned());
        }
    }
    for (name, lines) in &all {
        let waits = needs(lines).iter().any(|n| n.starts_with("smoke-bundle-"));
        let allowed = REPORTERS.contains(&name.as_str()) || name.starts_with("smoke-bundle-");
        if waits && !allowed {
            problems.push(format!("{name} waits for the smoke bundle"));
        }
    }
    for name in REPORTERS {
        if !needs(job(&all, name))
            .iter()
            .any(|n| n == "smoke-bundle-check")
        {
            problems.push(format!("{name} does not count the smoke bundle"));
        }
    }
    problems
}

/// Asserts the smoke bundle's jobs keep their rules, and that each defect is refused: a push
/// job that checks out the repository or holds more, a check job that holds the push token, a
/// job that runs for a pull request, a token passed in the push's arguments, a deploy-path job
/// that waits for the bundle, and a red report that does not count it.
#[test]
fn the_smoke_bundle_jobs_run_for_a_release_with_the_least_permissions() {
    let text = workflow();
    refused(
        smoke_problems,
        &text,
        "smoke-bundle-push",
        "      - uses: actions/download-artifact",
        "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - uses: actions/download-artifact",
    );
    refused(
        smoke_problems,
        &text,
        "smoke-bundle-push",
        "      packages: write\n",
        "      packages: write\n      contents: read\n",
    );
    refused(
        smoke_problems,
        &text,
        "smoke-bundle-check",
        "      contents: read\n",
        "      contents: read\n      packages: write\n",
    );
    refused(
        smoke_problems,
        &text,
        "smoke-bundle-pack",
        "github.event_name == 'push' && ",
        "",
    );
    refused(
        smoke_problems,
        &text,
        "smoke-bundle-push",
        "--password-file /dev/stdin <<< \"$GH_TOKEN\"",
        "--password-file \"$GH_TOKEN\"",
    );
    refused(
        smoke_problems,
        &text,
        "smoke-bundle-check",
        "needs: [plan, deploy-tool, smoke-bundle-push]",
        "needs: [plan, deploy-tool, smoke-bundle-pack]",
    );
    refused(
        smoke_problems,
        &text,
        "gamma",
        "    needs: [plan, publish, deploy-tool]",
        "    needs: [plan, publish, deploy-tool, smoke-bundle-check]",
    );
    refused(
        smoke_problems,
        &text,
        "release-red",
        ", smoke-bundle-check]",
        "]",
    );
}
