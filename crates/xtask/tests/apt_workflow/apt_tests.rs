//! The apt jobs of `release.yml` ([`super::apt`]): the file passes, and each way of giving a job
//! both the key and the role, leaving a job without its gate, its checks of `dist` or its
//! condition, handing the role or the token to the wrong job, or running a verb in a job of another
//! kind is named.

use super::apt::{JOBS, apt_problems};
use super::support::{each_named, in_job, text_of};
use super::yaml;

/// The problems of the release workflow text `text`, or its parse error.
fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => apt_problems(&doc),
        Err(e) => vec![e],
    }
}

const ROLE_STEP: &str = "        env:\n          ROLE_ARN: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n        run: tool/xtask channels assume --role-arn \"$ROLE_ARN\" --session-name \"jobscheduler-testing-apt-write-$GITHUB_RUN_ID\" --region us-east-1 --env-file \"$GITHUB_ENV\"\n";

/// The step that checks `dist` against SHA256SUMS, and the one that verifies each `.deb`.
const SUMS_STEP: &str =
    "      - name: The files are exactly the assets of this run, and match SHA256SUMS\n";
const VERIFY_STEP: &str = "      - name: The amd64 .deb is the one artifacts.yml attested, from main at this commit (from its bundle)\n";

#[test]
fn the_apt_jobs_of_release_yml_pass_and_are_the_eighteen_the_design_names() {
    // Catches: an apt job lost or renamed, which the defects below would not notice.
    assert_eq!(problems_of(&text_of("release.yml")), Vec::<String>::new());
    let jobs: Vec<&str> = JOBS.iter().map(|j| j.name).collect();
    assert_eq!(
        jobs,
        [
            "testing-apt-read",
            "testing-apt-index",
            "testing-apt-write",
            "testing-apt-read-2",
            "testing-apt-index-2",
            "testing-apt-write-2",
            "testing-apt-read-3",
            "testing-apt-index-3",
            "testing-apt-write-3",
            "stable-apt-read",
            "stable-apt-index",
            "stable-apt-write",
            "stable-apt-read-2",
            "stable-apt-index-2",
            "stable-apt-write-2",
            "stable-apt-read-3",
            "stable-apt-index-3",
            "stable-apt-write-3"
        ]
    );
}

#[test]
fn every_condition_of_the_table_requires_a_push_to_main_and_the_plans_release() {
    // Catches: a job of the table whose condition lets a pull request's dry run, or a run the plan
    // did not release, publish: the lint holds each `if:` whole against this table, so a
    // condition written into the table without the base would be held to nothing.
    let base = "github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true'";
    for job in &JOBS {
        assert!(
            job.condition.contains(base),
            "{}: {}",
            job.name,
            job.condition
        );
    }
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // needs and ifs
    (
        "testing-apt-read",
        "needs: [plan, publish, deploy-tool]",
        "needs: [plan, publish]",
        "testing-apt-read: needs are not",
    ),
    (
        "testing-apt-read-2",
        " && needs.testing-apt-write.outputs.conflict == 'true'",
        "",
        "testing-apt-read-2: `if` is not",
    ),
    (
        "testing-apt-read-3",
        "!cancelled() && !failure() && ",
        "",
        "testing-apt-read-3: `if` is not",
    ),
    (
        "testing-apt-index",
        "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n",
        "",
        "testing-apt-index: `if` is not",
    ),
    (
        "testing-apt-write",
        "github.event_name == 'push' && github.ref == 'refs/heads/main' && ",
        "",
        "testing-apt-write: `if` is not",
    ),
    (
        "stable-apt-index-2",
        "github.ref == 'refs/heads/main' && ",
        "",
        "stable-apt-index-2: `if` is not",
    ),
    // environments and permissions
    (
        "testing-apt-read",
        "environment: apt-publish",
        "environment: apt-signing",
        "testing-apt-read: environment is not apt-publish",
    ),
    (
        "testing-apt-index",
        "environment: apt-signing",
        "environment: apt-publish",
        "testing-apt-index: environment is not apt-signing",
    ),
    (
        "testing-apt-read",
        "      id-token: write\n",
        "",
        "testing-apt-read: permissions are [\"contents: read\"]",
    ),
    (
        "testing-apt-index",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      contents: read\n      id-token: write\n",
        "testing-apt-index: permissions are [\"contents: read\", \"id-token: write\"], not [\"contents: read\"]",
    ),
    (
        "testing-apt-write",
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "testing-apt-write: permissions are [\"contents: write\", \"id-token: write\"]",
    ),
    // the gate
    (
        "testing-apt-read",
        "id: gate\n",
        "id: check\n",
        "testing-apt-read: no step has the id `gate`",
    ),
    (
        "testing-apt-read",
        "tool/xtask channels gate --label testing-apt-read ",
        "tool/xtask channels gate --label apt ",
        "the gate does not run `tool/xtask channels gate --label testing-apt-read ...`",
    ),
    (
        "testing-apt-read",
        " --setting \"APT_HOSTNAME=${APT_HOSTNAME:+set}\"",
        "",
        "testing-apt-read: the gate is not given the setting APT_HOSTNAME",
    ),
    (
        "testing-apt-read",
        "--setting \"APT_BUCKET=${APT_BUCKET:+set}\"",
        "--setting \"APT_BUCKET=${{ vars.APT_BUCKET }}\"",
        "testing-apt-read: the gate's command line holds a value",
    ),
    (
        "testing-apt-index",
        " --setting \"APT_GPG_PASSPHRASE=${APT_GPG_PASSPHRASE:+set}\"",
        " --setting \"APT_GPG_PASSPHRASE=${APT_GPG_PASSPHRASE:+set}\" --setting \"APT_BUCKET=x\"",
        "testing-apt-index: the gate is given a setting it should not be",
    ),
    (
        "testing-apt-index",
        " --require \"READ_ENABLED=$READ_ENABLED\"",
        "",
        "testing-apt-index: the gate is not given --require \"READ_ENABLED=$READ_ENABLED\"",
    ),
    (
        "testing-apt-write",
        " --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
        "",
        "testing-apt-write: the gate is not given --require \"INDEX_SIGNED=$INDEX_SIGNED\"",
    ),
    (
        "testing-apt-read",
        "--setting \"APT_HOSTNAME",
        "--require \"X=$X\" --setting \"APT_HOSTNAME",
        "testing-apt-read: the gate is given a requirement it should not be",
    ),
    (
        "testing-apt-read",
        "      - name: Assume the apt publish role through OIDC\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "      - name: Assume the apt publish role through OIDC\n",
        "testing-apt-read: the step `Assume the apt publish role through OIDC",
    ),
    // the role
    (
        "testing-apt-read",
        "      - name: Gate on the settings",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n      - name: Gate on the settings",
        "testing-apt-read: a step before the gate names aws-actions/",
    ),
    (
        "testing-apt-write",
        "          ROLE_ARN: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        "          ROLE_ARN: ${{ vars.OTHER_ROLE_ARN }}",
        "testing-apt-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}",
    ),
    (
        "testing-apt-write",
        " --session-name \"jobscheduler-testing-apt-write-$GITHUB_RUN_ID\"",
        "",
        "testing-apt-write: the assume step is not given --session-name ",
    ),
    (
        "testing-apt-read",
        " --env-file \"$GITHUB_ENV\"",
        "",
        "testing-apt-read: the assume step is not given --env-file \"$GITHUB_ENV\"",
    ),
    (
        "testing-apt-read",
        "      - name: Gate on the settings",
        "      - run: tool/xtask channels assume --role-arn x\n      - name: Gate on the settings",
        "testing-apt-read: a step before the gate names channels assume",
    ),
    (
        "testing-apt-index",
        "      - name: Merge the .debs into the index",
        "      - run: tool/xtask channels assume --role-arn x\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n      - name: Merge the .debs into the index",
        "testing-apt-index: runs `xtask channels assume...`, which only the apt jobs that read and write may",
    ),
    (
        "testing-apt-write",
        ROLE_STEP,
        "        run: echo\n",
        "testing-apt-write: it needs one step that assumes the role and one that runs `tool/xtask channels apt write...`",
    ),
    (
        "testing-apt-write-3",
        " --last",
        "",
        "testing-apt-write-3: `--last` must be passed by the last attempt's write and by no other",
    ),
    (
        "testing-apt-write",
        "\"$VERSION\" --output",
        "\"$VERSION\" --last --output",
        "testing-apt-write: `--last` must be passed by the last attempt's write and by no other",
    ),
    // the files of dist
    (
        "testing-apt-write",
        "run: sha256sum --check --strict SHA256SUMS",
        "run: echo sums",
        "testing-apt-write: no step checks dist with `sha256sum --check --strict SHA256SUMS`",
    ),
    (
        "testing-apt-index",
        "        working-directory: dist\n",
        "",
        "testing-apt-index: no step checks dist with `sha256sum --check --strict SHA256SUMS`",
    ),
    (
        "testing-apt-index",
        "gh attestation verify \"dist/js-agent_${VERSION}_amd64.deb\"",
        "echo \"dist/js-agent_${VERSION}_amd64.deb\"",
        "testing-apt-index: no step verifies the amd64 .deb from its bundle with `gh attestation verify \"dist/js-agent_${VERSION}_amd64.deb\"",
    ),
    (
        "testing-apt-write",
        "gh attestation verify \"dist/js-agent_${VERSION}_arm64.deb\"",
        "echo \"dist/js-agent_${VERSION}_arm64.deb\"",
        "testing-apt-write: no step verifies the arm64 .deb from its bundle",
    ),
    (
        "stable-apt-index",
        "gh attestation verify \"dist/js-agent_${VERSION}_amd64.deb\"",
        "echo \"dist/js-agent_${VERSION}_amd64.deb\"",
        "stable-apt-index: no step verifies the amd64 .deb from its bundle",
    ),
    (
        "testing-apt-index-3",
        "gh attestation verify \"dist/js-agent_${VERSION}_arm64.deb\"",
        "echo \"dist/js-agent_${VERSION}_arm64.deb\"",
        "testing-apt-index-3: no step verifies the arm64 .deb from its bundle",
    ),
    (
        "testing-apt-index-2",
        " --bundle \"dist/js-agent_${VERSION}_amd64.deb.sigstore.json\"",
        "",
        "testing-apt-index-2: no step verifies the amd64 .deb from its bundle",
    ),
    (
        "testing-apt-write",
        " --source-digest \"$COMMIT\"",
        "",
        "testing-apt-write: the verification of the amd64 .deb lacks --source-digest \"$COMMIT\"",
    ),
    (
        "testing-apt-index",
        " --deny-self-hosted-runners",
        "",
        "testing-apt-index: the verification of the amd64 .deb lacks --deny-self-hosted-runners",
    ),
    (
        "testing-apt-index",
        "--signer-workflow \"$REPO/.github/workflows/artifacts.yml\"",
        "--signer-workflow \"$REPO/.github/workflows/release.yml\"",
        "testing-apt-index: the verification of the amd64 .deb lacks --signer-workflow \"$REPO/.github/workflows/artifacts.yml\"",
    ),
    (
        "testing-apt-write",
        "--source-ref refs/heads/main",
        "--source-ref refs/heads/other",
        "testing-apt-write: the verification of the amd64 .deb lacks --source-ref refs/heads/main",
    ),
    (
        "testing-apt-index",
        "          REPO: ${{ github.repository }}\n",
        "          REPO: other/repo\n",
        "testing-apt-index: the verification of the amd64 .deb does not read REPO from ${{ github.repository }}",
    ),
    (
        "testing-apt-index",
        "        run: gh attestation verify \"dist/js-agent_${VERSION}_amd64.deb\"",
        "        run: for deb in ./*.deb; do gh attestation verify \"$deb\"",
        "testing-apt-index: no step verifies the amd64 .deb from its bundle",
    ),
    (
        "testing-apt-write",
        "        run: gh attestation verify \"dist/js-agent_${VERSION}_amd64.deb\"",
        "        run: |\n          set -e\n          gh attestation verify \"dist/js-agent_${VERSION}_amd64.deb\"",
        "testing-apt-write: the step `The amd64 .deb is the one artifacts.yml attested",
    ),
    (
        "testing-apt-write",
        "          name: bundles\n",
        "          name: other\n",
        "testing-apt-write: the artifact bundles is not downloaded into dist",
    ),
    (
        "stable-apt-index-3",
        "          name: release-assets\n",
        "          name: other\n",
        "stable-apt-index-3: the artifact release-assets is not downloaded into dist",
    ),
    (
        "testing-apt-index",
        "          name: release-assets\n          path: dist\n",
        "          name: release-assets\n          path: elsewhere\n",
        "testing-apt-index: the artifact release-assets is not downloaded into dist",
    ),
    // the key
    (
        "testing-apt-read",
        "    env:\n      VERSION: ${{ needs.plan.outputs.version }}\n    steps:",
        "    env:\n      VERSION: ${{ needs.plan.outputs.version }}\n      KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n    steps:",
        "testing-apt-read: names APT_GPG_, the signing key, which only the apt index jobs may",
    ),
    (
        "plan",
        "    outputs:\n",
        "    env:\n      ROLE: ${{ vars.APT_PUBLISH_ROLE_ARN }}\n    outputs:\n",
        "plan: names APT_PUBLISH_ROLE_ARN, which only the apt jobs that read and write may",
    ),
    (
        "testing-apt-index",
        "      - name: Merge the .debs into the index",
        "      - uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n      - name: Merge the .debs into the index",
        "testing-apt-index: names aws-actions/; this job holds the key, not the role",
    ),
    (
        "testing-apt-index",
        "      VERSION: ${{ needs.plan.outputs.version }}\n",
        "      VERSION: ${{ needs.plan.outputs.version }}\n      AWS_REGION: us-east-1\n",
        "testing-apt-index: names AWS_; this job holds the key, not the role",
    ),
    (
        "testing-apt-index",
        "tool/xtask channels apt sign ",
        "echo ",
        "testing-apt-index: it needs a step that merges (`tool/xtask channels apt index...`) and then one that signs",
    ),
    (
        "testing-apt-index",
        "        id: sign\n",
        "",
        "testing-apt-index: the signing step does not have the id `sign`",
    ),
    (
        "testing-apt-index",
        "          APT_GPG_PASSPHRASE: ${{ secrets.APT_GPG_PASSPHRASE }}\n        run: tool/xtask channels apt sign",
        "        run: tool/xtask channels apt sign",
        "testing-apt-index: the signing step does not read secrets.APT_GPG_PASSPHRASE",
    ),
    (
        "testing-apt-index",
        "        env:\n          OWNER: ${{ github.repository_owner }}\n",
        "        env:\n          OWNER: ${{ github.repository_owner }}\n          KEY: ${{ secrets.APT_GPG_PRIVATE_KEY }}\n",
        "testing-apt-index: the step `Merge the .debs into the index that was read (the entries of every other product are kept)` names a secret; only the gate and the signing step may",
    ),
    (
        "testing-apt-write",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n          APT_HOSTNAME",
        "          APT_BUCKET: ${{ vars.APT_BUCKET }}\n          X: ${{ secrets.ANYTHING }}\n          APT_DISTRIBUTION_ID: ${{ vars.APT_DISTRIBUTION_ID }}\n          APT_HOSTNAME",
        "this job holds the role, not a key",
    ),
    // the token
    (
        "plan",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      contents: read\n      id-token: write\n",
        "plan: holds `id-token: write`; only artifacts, gamma and the apt jobs that read and write may",
    ),
    (
        "publish",
        "    permissions:\n      contents: write\n",
        "    permissions:\n      contents: write\n      id-token: write\n",
        "publish: holds `id-token: write`; only artifacts, gamma and the apt jobs that read and write may",
    ),
    (
        "gamma",
        "    timeout-minutes: 150\n",
        "    timeout-minutes: 150\n    environment: apt-publish\n",
        "gamma: names the environment apt-publish or apt-signing, which only the apt jobs may",
    ),
    (
        "deploy-tool",
        "run: cargo build -p xtask --locked --release",
        "run: tool/xtask channels assume --role-arn x",
        "deploy-tool: runs `xtask channels assume...`, which only the apt jobs that read and write may",
    ),
    // the verbs and the suites
    (
        "testing-apt-read",
        "tool/xtask channels apt read --suite testing --bucket",
        "tool/xtask channels apt sign --bucket",
        "testing-apt-read: runs `xtask channels apt sign...`, which only the apt index jobs may",
    ),
    (
        "testing-apt-index",
        "tool/xtask channels apt index --suite testing --current",
        "tool/xtask channels apt write --suite testing --current",
        "testing-apt-index: runs `xtask channels apt write...`, which only the Write jobs of the apt repository may",
    ),
    (
        "testing-apt-index",
        "tool/xtask channels apt index --suite testing --current",
        "tool/xtask channels apt read --current",
        "testing-apt-index: runs `xtask channels apt read...`, which only the Read jobs of the apt repository may",
    ),
    (
        "testing-apt-read",
        "channels apt read --suite testing --bucket",
        "channels apt read --suite stable --bucket",
        "testing-apt-read: `tool/xtask channels apt read...` is not given `--suite testing`",
    ),
    (
        "testing-apt-index",
        "channels apt index --suite testing --current",
        "channels apt index --current",
        "testing-apt-index: `tool/xtask channels apt index...` is not given `--suite testing`",
    ),
    (
        "testing-apt-index",
        "--version \"$VERSION\" --origin",
        "--version 0.0.0 --origin",
        "testing-apt-index: does not index the release's version",
    ),
    (
        "testing-apt-index",
        "--dir \"$RUNNER_TEMP/apt-next/dists/testing\" ",
        "--dir \"$RUNNER_TEMP/apt-next/dists/stable\" ",
        "testing-apt-index: the index is not signed in dists/testing, the suite the job writes",
    ),
    (
        "testing-apt-write",
        "channels apt write --suite testing --bucket",
        "channels apt write --suite stable --bucket",
        "testing-apt-write: `tool/xtask channels apt write...` is not given `--suite testing`",
    ),
    (
        "testing-apt-write",
        "channels apt write --suite testing --bucket",
        "channels apt write --suite testing --no-pool --bucket",
        "testing-apt-write: passes --no-pool; the release's write puts the packages in the pool",
    ),
    (
        "testing-apt-write",
        "--debs dist --version \"$VERSION\" ",
        "--debs dist --version \"$TAG\" ",
        "testing-apt-write: does not upload the .debs of dist for the release's version",
    ),
    (
        "testing-apt-read",
        "channels apt read --suite testing --bucket",
        "channels apt read --suite testing --suite stable --bucket",
        "testing-apt-read: names `--suite stable`; this job is for the suite testing",
    ),
    // the promotion into stable
    (
        "stable-apt-index",
        "needs: [plan, check-apt, stable-apt-read, testing-apt-index, deploy-tool]",
        "needs: [plan, stable-apt-read, testing-apt-index]",
        "stable-apt-index: needs are not",
    ),
    (
        "stable-apt-write",
        " && needs.check-apt.result == 'success' }}",
        " }}",
        "stable-apt-write: `if` is not",
    ),
    (
        "stable-apt-read",
        "needs: [plan, check-apt, testing-apt-read, deploy-tool]",
        "needs: [plan, testing-apt-read]",
        "stable-apt-read: needs are not",
    ),
    (
        "stable-apt-read",
        " --require \"TESTING_ENABLED=$TESTING_ENABLED\"",
        "",
        "stable-apt-read: the gate is not given --require \"TESTING_ENABLED=$TESTING_ENABLED\"",
    ),
    (
        "stable-apt-read",
        "channels apt read --suite testing --bucket",
        "channels apt read --bucket",
        "stable-apt-read: no step runs `tool/xtask channels apt read...` with `--suite testing`: the promotion reads both",
    ),
    (
        "stable-apt-read",
        "channels apt read --suite stable --bucket",
        "channels apt read --suite testing --bucket",
        "stable-apt-read: no step runs `tool/xtask channels apt read...` with `--suite stable`: the promotion reads both",
    ),
    (
        "stable-apt-index",
        "--sums dist/SHA256SUMS ",
        "",
        "stable-apt-index: `tool/xtask channels apt promote...` is not given --sums dist/SHA256SUMS",
    ),
    (
        "stable-apt-index",
        "--testing \"$RUNNER_TEMP/apt-read/testing\" ",
        "--testing \"$RUNNER_TEMP/apt-read/stable\" ",
        "stable-apt-index: `tool/xtask channels apt promote...` is not given --testing",
    ),
    (
        "stable-apt-index",
        "channels apt promote ",
        "channels apt index --suite stable ",
        "stable-apt-index: runs `xtask channels apt index...`, which only the Index jobs of the suite testing of the apt repository may",
    ),
    (
        "testing-apt-index",
        "channels apt index --suite testing ",
        "channels apt promote ",
        "testing-apt-index: runs `xtask channels apt promote...`, which only the Index jobs of the suite stable of the apt repository may",
    ),
    (
        "stable-apt-index",
        "--dir \"$RUNNER_TEMP/apt-next/dists/stable\" ",
        "--dir \"$RUNNER_TEMP/apt-next/dists/testing\" ",
        "stable-apt-index: the index is not signed in dists/stable, the suite the job writes",
    ),
    (
        "stable-apt-write",
        "--suite stable --bucket",
        "--suite testing --bucket",
        "stable-apt-write: `tool/xtask channels apt write...` is not given `--suite stable`",
    ),
    (
        "stable-apt-write",
        " --no-pool ",
        " ",
        "stable-apt-write: does not pass --no-pool; the promotion writes the index only",
    ),
    (
        "stable-apt-write",
        " --no-pool ",
        " --no-pool --debs dist ",
        "stable-apt-write: passes --debs; the promotion uploads no package",
    ),
    (
        "stable-apt-write",
        " ROLE_ARN: ${{ vars.APT_PUBLISH_ROLE_ARN }}",
        " ROLE_ARN: ${{ vars.OTHER_ROLE_ARN }}",
        "stable-apt-write: the role assumed is not ${{ vars.APT_PUBLISH_ROLE_ARN }}",
    ),
    (
        "stable-apt-write-3",
        " --last",
        "",
        "stable-apt-write-3: `--last` must be passed by the last attempt's write and by no other",
    ),
    // no build in a job that holds a token or the key
    (
        "testing-apt-read",
        "      - name: Make the tool executable\n",
        "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - name: Make the tool executable\n",
        "testing-apt-read: checks out the repository; this job runs the tool deploy-tool built and compiles nothing",
    ),
    (
        "testing-apt-index",
        "      - name: Make the tool executable\n",
        "      - run: rustup toolchain install\n      - name: Make the tool executable\n",
        "testing-apt-index: the step `rustup toolchain install` runs cargo or rustup",
    ),
    (
        "stable-apt-index-2",
        "      - name: Make the tool executable\n",
        "      - name: Compile xtask\n        run: cargo xtask --help\n      - name: Make the tool executable\n",
        "stable-apt-index-2: the step `Compile xtask` runs cargo or rustup",
    ),
    (
        "testing-apt-write",
        "tool/xtask channels apt write --suite",
        "cargo xtask channels apt write --suite",
        "testing-apt-write: the step `Upload the .debs once each",
    ),
    (
        "stable-apt-read",
        "tool/xtask channels gate --label stable-apt-read ",
        "cargo xtask channels gate --label stable-apt-read ",
        "stable-apt-read: the gate does not run `tool/xtask channels gate --label stable-apt-read ...`",
    ),
    (
        "testing-apt-write-2",
        "run: tool/xtask channels assume",
        "run: cargo xtask channels assume",
        "testing-apt-write-2: it needs one step that assumes the role and one that runs",
    ),
    // the tool of deploy-tool
    (
        "testing-apt-write",
        "needs: [plan, testing-apt-read, testing-apt-index, deploy-tool]",
        "needs: [plan, testing-apt-read, testing-apt-index]",
        "testing-apt-write: needs are not [\"deploy-tool\", \"plan\", \"testing-apt-index\", \"testing-apt-read\"]",
    ),
    (
        "stable-apt-index-3",
        "needs: [plan, stable-apt-read-3, deploy-tool]",
        "needs: [plan, stable-apt-read-3]",
        "stable-apt-index-3: needs are not",
    ),
    (
        "testing-apt-index",
        "          name: deploy-tool\n",
        "          name: apt-current\n",
        "testing-apt-index: the artifact deploy-tool is not downloaded into tool",
    ),
    (
        "stable-apt-write-2",
        "          name: deploy-tool\n          path: tool\n",
        "          name: deploy-tool\n          path: elsewhere\n",
        "stable-apt-write-2: the artifact deploy-tool is not downloaded into tool",
    ),
    (
        "testing-apt-read-2",
        "        run: chmod +x tool/xtask\n",
        "        run: echo\n",
        "testing-apt-read-2: no step makes the tool executable with `chmod +x tool/xtask`",
    ),
    // one command to a step
    (
        "stable-apt-read",
        "        run: tool/xtask channels apt read --suite testing",
        "        run: |\n          echo first\n          tool/xtask channels apt read --suite testing",
        "stable-apt-read: the step `Read the index of testing (what is promoted), with the ETag of each object, from S3` runs more than one command; each step is one",
    ),
    (
        "stable-apt-read",
        "channels apt read --suite stable --bucket",
        "channels apt read --suite stable --suite testing --bucket",
        "stable-apt-read: `tool/xtask channels apt read...` is not given exactly one of `--suite testing` and `--suite stable`",
    ),
    (
        "testing-apt-index",
        "        run: tool/xtask channels apt sign ",
        "        run: |\n          umask 077\n          tool/xtask channels apt sign ",
        "testing-apt-index: the step `Sign Release with the signing subkey (InRelease and Release.gpg), then remove the key",
    ),
    // the key reaches the verb in the environment
    (
        "testing-apt-index",
        " --key-env APT_GPG_PRIVATE_KEY --passphrase-env APT_GPG_PASSPHRASE",
        " --key-file k --passphrase-file p",
        "testing-apt-index: the signing step is not given --key-env APT_GPG_PRIVATE_KEY ",
    ),
    (
        "stable-apt-index",
        " --key-env APT_GPG_PRIVATE_KEY --passphrase-env APT_GPG_PASSPHRASE",
        " --key-file k --passphrase-env APT_GPG_PASSPHRASE",
        "stable-apt-index: the signing step passes --key-file; the secrets go by --key-env and --passphrase-env",
    ),
    (
        "testing-apt-index-2",
        " --passphrase-env APT_GPG_PASSPHRASE",
        "",
        "testing-apt-index-2: the signing step is not given --passphrase-env APT_GPG_PASSPHRASE ",
    ),
    (
        "stable-apt-index-3",
        " --passphrase-env APT_GPG_PASSPHRASE",
        " --passphrase-file p",
        "stable-apt-index-3: the signing step passes --passphrase-file",
    ),
    (
        "publish",
        "          sha256sum --check --strict SHA256SUMS\n",
        "          tool/xtask channels apt index --suite testing\n          sha256sum --check --strict SHA256SUMS\n",
        "publish: runs `xtask channels apt index...`, which only the Index jobs of the suite testing of the apt repository may",
    ),
];

#[test]
fn each_defect_in_the_apt_jobs_is_named_by_the_lint() {
    // Catches: a pin of the apt jobs that holds nothing, because the file passes whether or not
    // the thing it pins is there.
    each_named("release.yml", problems_of, DEFECTS);
}

#[test]
fn a_job_that_is_missing_or_too_short_is_named() {
    // Catches: an apt job deleted, and one with a gate but no merge, no signature or no role,
    // which the rules about the steps would otherwise skip.
    let doc = yaml::load("jobs: {}\n").expect("parses");
    let problems = apt_problems(&doc);
    assert_eq!(problems.len(), 18);
    assert!(
        problems
            .iter()
            .all(|p| p.ends_with(": there is no such job"))
    );
    let short = "jobs:
  testing-apt-read:
    steps:
      - id: gate
        run: echo
  testing-apt-index:
    steps:
      - id: gate
        run: echo
";
    let problems = apt_problems(&yaml::load(short).expect("parses"));
    assert!(
        problems.contains(
            &"testing-apt-read: it needs one step that assumes the role and one that runs `tool/xtask channels apt read...`"
                .to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.starts_with(
            "testing-apt-index: it needs a step that merges (`tool/xtask channels apt index...`) and then one that signs"
        )),
        "{problems:?}"
    );
    assert!(
        problems.contains(
            &"testing-apt-index: no step checks dist with `sha256sum --check --strict SHA256SUMS`"
                .to_owned()
        ),
        "{problems:?}"
    );
}

#[test]
fn a_job_renamed_or_added_is_named_by_the_list_of_jobs() {
    // Catches: a fourth attempt, or an apt job the lint does not hold.
    let text = text_of("release.yml");
    let renamed = text.replace("\n  testing-apt-index-3:\n", "\n  testing-apt-index-4:\n");
    let problems = problems_of(&renamed);
    assert!(
        problems.contains(
            &"testing-apt-index-4: an apt job this lint does not hold; the apt jobs are the eighteen it lists"
                .to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"testing-apt-index-3: there is no such job".to_owned()),
        "{problems:?}"
    );
}

#[test]
fn the_role_is_assumed_before_the_index_is_read_and_before_it_is_written() {
    // Catches: a read or write that runs without credentials, and the credentials action (which the
    // Actions policy refuses) coming back in the place of the step that assumes the role.
    let text = text_of("release.yml");
    for (job, verb) in [
        ("testing-apt-read", "      - name: Read the current index"),
        (
            "testing-apt-write",
            "      - name: Upload the .debs once each",
        ),
    ] {
        let role = "      - name: Assume the apt publish role through OIDC\n";
        let body_start = text.find(&format!("\n  {job}:\n")).expect("job");
        let (head, rest) = text.split_at(body_start);
        let (role_at, verb_at) = (
            rest.find(role).expect("role"),
            rest.find(verb).expect("verb"),
        );
        assert!(role_at < verb_at);
        let after_verb = rest[verb_at..]
            .find("\n  testing-apt-")
            .or_else(|| rest[verb_at..].find("\n\n  # "))
            .map_or(rest.len(), |n| verb_at + n);
        let swapped = format!(
            "{head}{}{}{}{}",
            &rest[..role_at],
            &rest[verb_at..after_verb],
            &rest[role_at..verb_at],
            &rest[after_verb..]
        );
        let problems = problems_of(&swapped);
        assert!(
            problems
                .iter()
                .any(|p| p == &format!("{job}: the role is assumed after the verb runs")),
            "{job}: {problems:?}"
        );
    }
    let back_to_the_action = in_job(
        &text,
        "testing-apt-write-2",
        "        run: tool/xtask channels assume --role-arn \"$ROLE_ARN\" --session-name \"jobscheduler-testing-apt-write-2-$GITHUB_RUN_ID\" --region us-east-1 --env-file \"$GITHUB_ENV\"\n",
        "        uses: aws-actions/configure-aws-credentials@e1253824e5c10ff9df46874f81ed3ec929e19cfd # v6.3.0\n",
    );
    assert!(
        problems_of(&back_to_the_action)
            .iter()
            .any(|p| p.starts_with(
                "testing-apt-write-2: it needs one step that assumes the role and one that runs"
            )),
        "{back_to_the_action}"
    );
}

/// `text` with the two steps that check `dist` swapped with what follows them: the lines of the
/// job `job` from `first` up to `second`, and from `second` up to `third`, exchanged.
fn swapped_steps(text: &str, job: &str, first: &str, second: &str, third: &str) -> String {
    let key = format!("\n  {job}:\n");
    let start = text.find(&key).expect("job");
    let (head, rest) = text.split_at(start);
    let (a, b, c) = (
        rest.find(first).expect("first step"),
        rest.find(second).expect("second step"),
        rest.find(third).expect("third step"),
    );
    assert!(a < b && b < c);
    format!(
        "{head}{}{}{}{}",
        &rest[..a],
        &rest[b..c],
        &rest[a..b],
        &rest[c..]
    )
}

#[test]
fn the_files_of_dist_are_checked_in_order_before_the_verb_runs() {
    // Catches: the bundles verified before the files are held to SHA256SUMS (a bundle that
    // names a digest the sums do not list), and a verb that merges or uploads the .debs before
    // they are verified.
    let text = text_of("release.yml");
    let reversed = swapped_steps(
        &text,
        "testing-apt-write",
        SUMS_STEP,
        VERIFY_STEP,
        "      - name: Assume the apt publish role through OIDC\n",
    );
    assert!(
        problems_of(&reversed).contains(
            &"testing-apt-write: the .debs are verified before the files are checked against SHA256SUMS"
                .to_owned()
        ),
        "{reversed}"
    );
    let late = swapped_steps(
        &text,
        "testing-apt-index",
        VERIFY_STEP,
        "      - name: Merge the .debs into the index",
        "      - name: Sign Release with the signing subkey",
    );
    assert!(
        problems_of(&late).contains(
            &"testing-apt-index: the verb runs before the .debs are verified from their bundles"
                .to_owned()
        ),
        "{late}"
    );
}

#[test]
fn the_tool_is_downloaded_and_made_executable_before_the_gate_that_runs_it() {
    // Catches: the gate (or any verb) running before the tool is downloaded and made executable,
    // which the table of defects cannot name because every step is still there.
    let text = text_of("release.yml");
    let late = swapped_steps(
        &text,
        "testing-apt-read",
        "      - name: Make the tool executable\n",
        "      - name: Gate on the settings",
        "      - name: Assume the apt publish role through OIDC\n",
    );
    assert!(
        problems_of(&late).contains(
            &"testing-apt-read: the tool is not downloaded and made executable (`chmod +x tool/xtask`), in that order, before the gate"
                .to_owned()
        ),
        "{late}"
    );
}
