//! The job `pkg-upload` of `release.yml` ([`super::pkg_upload`]): the file passes, and each way of
//! leaving the job ungated, handing it more than the role, uploading without the role or from a
//! package that was not downloaded, or running its verb in another job is named.

use super::pkg_upload::{
    ASSUME, DESCRIPTOR, DESCRIPTOR_VERB, GATE, PKG_JOB, SIGNED_IF, UNSIGNED_IF, UPLOAD, VERB,
    pkg_upload_problems,
};
use super::support::{each_named, text_of};
use super::yaml;

fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => pkg_upload_problems(&doc),
        Err(e) => vec![e],
    }
}

#[test]
fn the_pkg_upload_job_of_release_yml_passes() {
    assert_eq!(problems_of(&text_of("release.yml")), Vec::<String>::new());
}

#[test]
fn a_release_without_the_job_is_named() {
    let text = text_of("release.yml").replace("\n  pkg-upload:\n", "\n  pkg-uploaded:\n");
    assert_eq!(
        problems_of(&text),
        ["pkg-upload: there is no such job".to_owned()]
    );
}

/// `(job, text to replace once in it, its replacement, what a problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // The job runs for a pull request's dry run, or without the plan's release.
    (
        "pkg-upload",
        "github.event_name == 'push' && github.ref == 'refs/heads/main' && ",
        "",
        "pkg-upload: `if` is not",
    ),
    (
        "pkg-upload",
        " && needs.plan.outputs.release == 'true' && ",
        " && ",
        "pkg-upload: `if` is not",
    ),
    // Without `!cancelled()` a failed package job would skip the upload and `desired`'s wait.
    (
        "pkg-upload",
        "${{ !cancelled() && ",
        "${{ ",
        "pkg-upload: `if` is not",
    ),
    // Without `publish`'s success nothing is uploaded for a release that was never published.
    (
        "pkg-upload",
        " && needs.publish.result == 'success'",
        "",
        "pkg-upload: `if` is not",
    ),
    (
        "pkg-upload",
        " && needs.deploy-tool.result == 'success'",
        "",
        "pkg-upload: `if` is not",
    ),
    // It does not wait for the package job or for `publish`, or it waits for the beta tests.
    (
        "pkg-upload",
        "needs: [plan, deploy-tool, publish, beta-macos-pkg]",
        "needs: [plan, deploy-tool, publish]",
        "pkg-upload: needs are not",
    ),
    (
        "pkg-upload",
        "needs: [plan, deploy-tool, publish, beta-macos-pkg]",
        "needs: [plan, deploy-tool, beta-macos-pkg]",
        "pkg-upload: needs are not",
    ),
    (
        "pkg-upload",
        "needs: [plan, deploy-tool, publish, beta-macos-pkg]",
        "needs: [plan, deploy-tool, publish, promote-macos]",
        "pkg-upload: needs are not",
    ),
    (
        "pkg-upload",
        "needs: [plan, deploy-tool, publish, beta-macos-pkg]",
        "needs: [plan, deploy-tool, publish, beta-macos-pkg, beta-macos]",
        "pkg-upload: needs are not",
    ),
    (
        "pkg-upload",
        "    environment: apt-publish\n",
        "    environment: apt-signing\n",
        "pkg-upload: environment is not apt-publish",
    ),
    (
        "pkg-upload",
        "      id-token: write\n",
        "",
        "pkg-upload: permissions are not",
    ),
    (
        "pkg-upload",
        "      contents: read\n",
        "      contents: write\n",
        "pkg-upload: permissions are not",
    ),
    (
        "pkg-upload",
        "VERSION: ${{ needs.plan.outputs.version }}",
        "VERSION: 0.0.0",
        "pkg-upload: VERSION is not the plan's version",
    ),
    // The job compiles, so a build script would run beside the role's token.
    (
        "pkg-upload",
        "    steps:\n",
        "    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n",
        "pkg-upload: checks out or compiles",
    ),
    (
        "pkg-upload",
        "        run: chmod +x tool/xtask\n",
        "        run: cargo build -p xtask\n",
        "pkg-upload: checks out or compiles",
    ),
    (
        "pkg-upload",
        "        run: chmod +x tool/xtask\n",
        "        run: |\n          chmod +x tool/xtask\n          true\n",
        "pkg-upload: a step runs more than one command",
    ),
    (
        "pkg-upload",
        "        run: chmod +x tool/xtask\n",
        "        run: chmod +x tool/other\n",
        "pkg-upload: the tool is not downloaded and made executable before the gate",
    ),
    // The gate: without a setting, or reading none.
    (
        "pkg-upload",
        " --setting \"APT_BUCKET=${APT_BUCKET:+set}\"",
        "",
        "pkg-upload: the gate does not run",
    ),
    (
        "pkg-upload",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n        run: tool/xtask channels gate",
        "          APT_BUCKET: ''\n        run: tool/xtask channels gate",
        "pkg-upload: the gate does not read APT_BUCKET",
    ),
    // The signed package.
    (
        "pkg-upload",
        "name: js-agent-macos-pkg-signed",
        "name: js-agent-macos-pkg-unsigned",
        "pkg-upload: the step `signed` is not the download of js-agent-macos-pkg-signed into pkg",
    ),
    (
        "pkg-upload",
        "        continue-on-error: true\n",
        "",
        "pkg-upload: the download of the signed package may not fail the job",
    ),
    // The role and the upload run only for a downloaded package, after the gate.
    (
        "pkg-upload",
        "        if: ${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome == 'success' }}\n        env:\n          ROLE_ARN",
        "        env:\n          ROLE_ARN",
        "runs `if: `, not `",
    ),
    (
        "pkg-upload",
        "        if: ${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome == 'success' }}\n        env:\n          APT_BUCKET",
        "        if: ${{ steps.signed.outcome == 'success' }}\n        env:\n          APT_BUCKET",
        "runs `if: ${{ steps.signed.outcome == 'success' }}`",
    ),
    (
        "pkg-upload",
        "        if: ${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome != 'success' }}\n",
        "",
        "runs `if: `, not `",
    ),
    (
        "pkg-upload",
        "        env:\n          ROLE_ARN: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "        env:\n          ROLE_ARN: ${{ vars.OTHER_ROLE }}",
        "pkg-upload: the role assumed is not",
    ),
    (
        "pkg-upload",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n        run: tool/xtask channels pkg upload",
        "          APT_BUCKET: other\n        run: tool/xtask channels pkg upload",
        "pkg-upload: the bucket is not vars.APT_BUCKET",
    ),
    (
        "pkg-upload",
        " --version \"$VERSION\"",
        "",
        "pkg-upload: the steps are not the gate, the download, the role and the upload",
    ),
    // No role is assumed: the upload would run with no credentials, or with another job's.
    (
        "pkg-upload",
        "tool/xtask channels assume",
        "echo assume",
        "pkg-upload: the steps are not the gate, the download, the role and the upload",
    ),
    // The descriptor mapping: missing, misspelt, without its inputs, or under another `if`.
    (
        "pkg-upload",
        "      - name: Map js-agent to pkg/jobscheduler in the repository's descriptor\n        if: ${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome == 'success' }}\n        env:\n          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n        run: tool/xtask channels descriptor write --bucket \"$APT_BUCKET\" --distribution-id \"$APT_DISTRIBUTION_ID\" --work \"$RUNNER_TEMP/descriptor\"\n",
        "",
        "pkg-upload: there is no step after the upload that runs",
    ),
    (
        "pkg-upload",
        "tool/xtask channels descriptor write",
        "tool/xtask channels descriptor read",
        "pkg-upload: there is no step after the upload that runs",
    ),
    (
        "pkg-upload",
        " --bucket \"$APT_BUCKET\" --distribution-id",
        " --distribution-id",
        "pkg-upload: there is no step after the upload that runs",
    ),
    (
        "pkg-upload",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID: ''\n",
        "pkg-upload: the descriptor step does not read APT_DISTRIBUTION_ID",
    ),
    (
        "pkg-upload",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID",
        "          APT_BUCKET: other\n          APT_DISTRIBUTION_ID",
        "pkg-upload: the descriptor step does not read APT_BUCKET",
    ),
    (
        "pkg-upload",
        "        if: ${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome == 'success' }}\n        env:\n          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID",
        "        if: ${{ steps.gate.outputs.enabled == 'true' }}\n        env:\n          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID",
        "pkg-upload: the descriptor step does not run under the upload's `if`",
    ),
    // Another job runs the verb that writes the descriptor.
    (
        "publish",
        "          sha256sum --check --strict SHA256SUMS\n",
        "          tool/xtask channels descriptor write --bucket b --work w\n          sha256sum --check --strict SHA256SUMS\n",
        "publish: runs `xtask channels descriptor write...`, which only pkg-upload may",
    ),
    // A secret reaches the job.
    (
        "pkg-upload",
        "VERSION: ${{ needs.plan.outputs.version }}",
        "VERSION: ${{ needs.plan.outputs.version }}\n      KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}",
        "pkg-upload: names a secret or the signing key",
    ),
    // Another job runs the verb that writes the package repository.
    (
        "publish",
        "          sha256sum --check --strict SHA256SUMS\n",
        "          tool/xtask channels pkg upload --dir dist\n          sha256sum --check --strict SHA256SUMS\n",
        "publish: runs `xtask channels pkg upload...`, which only pkg-upload may",
    ),
    // Another job holds the role of the package repository's prefix, by name or by the step.
    (
        "gamma",
        "    timeout-minutes: 150\n",
        "    timeout-minutes: 150\n    environment: apt-publish\n",
        "gamma: names the environment apt-publish or apt-signing, which only the apt jobs and pkg-upload may",
    ),
];

fn release_problems(text: &str) -> Vec<String> {
    let mut found = problems_of(text);
    if let Ok(doc) = yaml::load(text) {
        found.extend(super::apt::apt_problems(&doc));
    }
    found
}

#[test]
fn each_defect_in_the_pkg_upload_job_is_named_by_the_lint() {
    each_named("release.yml", release_problems, DEFECTS);
}

#[test]
fn the_commands_of_the_table_are_the_ones_the_job_runs() {
    let text = text_of("release.yml");
    for command in [GATE, ASSUME, UPLOAD, SIGNED_IF, UNSIGNED_IF, PKG_JOB] {
        assert!(text.contains(command), "{command}");
    }
    // No other job of the file runs the verb.
    assert_eq!(text.matches(VERB).count(), 1);
    assert!(text.contains(DESCRIPTOR));
    assert_eq!(text.matches(DESCRIPTOR_VERB).count(), 1);
}

#[test]
fn a_descriptor_step_before_the_upload_is_named() {
    let text = text_of("release.yml");
    let start = text
        .find("      - name: Map js-agent to pkg/jobscheduler")
        .expect("step");
    let end = text[start..]
        .find("\n\n")
        .map_or(text.len(), |n| start + n + 1);
    let step = text[start..end].to_owned();
    let moved = text.replace(&step, "");
    let upload = moved
        .find("      - name: Upload the package, then its SHA256SUMS")
        .expect("upload");
    let moved = format!("{}{}{}", &moved[..upload], step, &moved[upload..]);
    let found = problems_of(&moved);
    assert!(
        found
            .iter()
            .any(|p| p.starts_with("pkg-upload: there is no step after the upload that runs")),
        "{found:?}"
    );
}
