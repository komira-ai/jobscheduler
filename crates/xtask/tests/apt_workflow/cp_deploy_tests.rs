//! The control-plane deploy of `release.yml` ([`super::cp_deploy`]): the file passes, and each way
//! of running it at another time, handing it more than it needs, calling another workflow or
//! passing other inputs, or letting another job read its variables, run its verb or call the
//! updater, is named.

use super::cp_deploy::cp_deploy_problems;
use super::support::{each_named, text_of};
use super::yaml;

fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => cp_deploy_problems(&doc),
        Err(e) => vec![e],
    }
}

#[test]
fn the_control_plane_deploy_of_release_yml_passes() {
    assert_eq!(problems_of(&text_of("release.yml")), Vec::<String>::new());
}

/// Asserts that text that is not YAML is reported as the parser's error, not as a pass.
#[test]
fn text_that_is_not_yaml_is_named() {
    let problems = problems_of("jobs: [");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(!problems[0].is_empty());
}

const SHA: &str = "c1fcc47610e2886d4d9e561be3c0c76f0452d1f6";

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // cp-deploy-digest: when it runs
    (
        "cp-deploy-digest",
        "cp-deploy-digest:",
        "cp-deploy-digests:",
        "cp-deploy-digest: there is no such job",
    ),
    (
        "cp-deploy-digest",
        "needs: [plan, publish, deploy-tool]",
        "needs: [plan, deploy-tool]",
        "cp-deploy-digest: needs are not [\"deploy-tool\", \"plan\", \"publish\"]",
    ),
    (
        "cp-deploy-digest",
        " && vars.CP_DEPLOY_ENDPOINT != ''",
        "",
        "cp-deploy-digest: `if` is not",
    ),
    (
        "cp-deploy-digest",
        "github.event_name == 'push' && ",
        "",
        "cp-deploy-digest: `if` is not",
    ),
    (
        "cp-deploy-digest",
        " && vars.CP_DEPLOY_TAILSCALE_TAGS != ''",
        "",
        "cp-deploy-digest: `if` is not",
    ),
    // cp-deploy-digest: nothing private
    (
        "cp-deploy-digest",
        "    permissions: {}\n",
        "    permissions:\n      contents: read\n",
        "cp-deploy-digest: permissions are not {}",
    ),
    (
        "cp-deploy-digest",
        "    permissions: {}\n",
        "",
        "cp-deploy-digest: permissions are not {}",
    ),
    (
        "cp-deploy-digest",
        "    timeout-minutes: 15\n",
        "    timeout-minutes: 15\n    environment: controlplane\n",
        "cp-deploy-digest: names an environment",
    ),
    (
        "cp-deploy-digest",
        "    timeout-minutes: 15\n",
        "    timeout-minutes: 15\n    env:\n      X: y\n",
        "cp-deploy-digest: has a job `env:`",
    ),
    (
        "cp-deploy-digest",
        "          VERSION: ${{ needs.plan.outputs.version }}\n",
        "          VERSION: ${{ needs.plan.outputs.version }}\n          GH_TOKEN: ${{ github.token }}\n",
        "cp-deploy-digest: a step names github.token",
    ),
    (
        "cp-deploy-digest",
        "          VERSION: ${{ needs.plan.outputs.version }}\n",
        "          VERSION: ${{ needs.plan.outputs.version }}\n          X: ${{ secrets.ANY }}\n",
        "cp-deploy-digest: a step names secrets.",
    ),
    (
        "cp-deploy-digest",
        "          VERSION: ${{ needs.plan.outputs.version }}\n",
        "          VERSION: ${{ vars.CP_DEPLOY_ENDPOINT }}\n",
        "cp-deploy-digest: a step names vars.",
    ),
    (
        "cp-deploy-digest",
        "          VERSION: ${{ needs.plan.outputs.version }}\n",
        "          VERSION: ${{ needs.plan.outputs.version }}\n          REPO: x\n",
        "cp-deploy-digest: the verb's step is not given VERSION=${{ needs.plan.outputs.version }} alone",
    ),
    // cp-deploy-digest: what it runs
    (
        "cp-deploy-digest",
        "runs-on: ubuntu-24.04",
        "runs-on: ubuntu-latest",
        "cp-deploy-digest: runs-on is not ubuntu-24.04",
    ),
    (
        "cp-deploy-digest",
        "          name: bundles\n",
        "          name: dist-x86_64-unknown-linux-musl\n",
        "cp-deploy-digest: its actions are",
    ),
    (
        "cp-deploy-digest",
        "      - run: chmod +x tool/xtask\n",
        "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - run: chmod +x tool/xtask\n",
        "cp-deploy-digest: its actions are",
    ),
    (
        "cp-deploy-digest",
        "      - run: chmod +x tool/xtask\n",
        "      - run: chmod +x tool/xtask\n      - run: cargo build -p xtask --locked\n",
        "cp-deploy-digest: runs [",
    ),
    (
        "cp-deploy-digest",
        " --output \"$GITHUB_OUTPUT\"",
        "",
        "cp-deploy-digest: no step runs `tool/xtask release image-digest",
    ),
    (
        "cp-deploy-digest",
        "        id: image\n",
        "        id: digest\n",
        "cp-deploy-digest: the verb's step has not the id `image`",
    ),
    (
        "cp-deploy-digest",
        "digest: ${{ steps.image.outputs.digest }}",
        "digest: ${{ steps.other.outputs.digest }}",
        "cp-deploy-digest: outputs are",
    ),
    // cp-deploy: when it runs
    (
        "cp-deploy",
        "  cp-deploy:\n",
        "  cp-deployed:\n",
        "cp-deploy: there is no such job",
    ),
    (
        "cp-deploy",
        "needs: [plan, cp-deploy-digest]",
        "needs: [plan, cp-deploy-digest, gamma]",
        "cp-deploy: needs are not [\"cp-deploy-digest\", \"plan\"]",
    ),
    (
        "cp-deploy",
        " && vars.CP_DEPLOY_ENDPOINT != ''",
        "",
        "cp-deploy: `if` is not",
    ),
    (
        "cp-deploy",
        " && needs.plan.outputs.release == 'true'",
        "",
        "cp-deploy: `if` is not",
    ),
    (
        "cp-deploy",
        " && vars.CP_DEPLOY_TAILSCALE_TAGS != ''",
        "",
        "cp-deploy: `if` is not",
    ),
    // cp-deploy: what it holds and calls
    (
        "cp-deploy",
        "      id-token: write\n",
        "      id-token: write\n      contents: read\n",
        "cp-deploy: permissions are [\"contents: read\", \"id-token: write\"], not id-token: write alone",
    ),
    (
        "cp-deploy",
        "    permissions:\n      id-token: write\n",
        "",
        "cp-deploy: permissions are [], not id-token: write alone",
    ),
    (
        "cp-deploy",
        SHA,
        "537cf4f0fc7664cde92a6b66db311fad6e050ad7",
        "cp-deploy: calls \"komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@537cf4f0fc7664cde92a6b66db311fad6e050ad7\"",
    ),
    (
        "cp-deploy",
        "@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6 # v0.1.3",
        "@v0.1.3",
        "cp-deploy: calls",
    ),
    (
        "cp-deploy",
        "service: js-server",
        "service: js-lease-server",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "version: ${{ needs.plan.outputs.version }}",
        "version: ${{ needs.plan.outputs.tag }}",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "wait-minutes: 60",
        "wait-minutes: 0",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "environment: controlplane",
        "environment: apt-publish",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "      tailscale-oauth-client-id: ${{ vars.CP_DEPLOY_TAILSCALE_CLIENT_ID }}\n",
        "",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "      tailscale-audience: ${{ vars.CP_DEPLOY_TAILSCALE_AUDIENCE }}\n",
        "",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "      tailscale-tags: ${{ vars.CP_DEPLOY_TAILSCALE_TAGS }}\n",
        "",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "tailscale-tags: ${{ vars.CP_DEPLOY_TAILSCALE_TAGS }}",
        "tailscale-tags: tag:ci",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "      environment: controlplane\n",
        "      environment: controlplane\n      signer-ref: refs/heads/dev\n",
        "cp-deploy: passes",
    ),
    (
        "cp-deploy",
        "    permissions:\n",
        "    secrets: inherit\n    permissions:\n",
        "cp-deploy: has the keys",
    ),
    (
        "cp-deploy",
        "    permissions:\n",
        "    env:\n      X: y\n    permissions:\n",
        "cp-deploy: has the keys",
    ),
    // only these two jobs
    (
        "gamma",
        "          VERSION: ${{ needs.plan.outputs.version }}\n",
        "          VERSION: ${{ needs.plan.outputs.version }}\n          X: ${{ vars.CP_DEPLOY_AUDIENCE }}\n",
        "gamma: names a CP_DEPLOY_ variable; only cp-deploy-digest and cp-deploy may",
    ),
    (
        "gamma",
        "          chmod +x tool/xtask\n",
        "          chmod +x tool/xtask\n          tool/xtask release image-digest --dist dist\n",
        "gamma: runs `xtask release image-digest`; only cp-deploy-digest may",
    ),
    (
        "deploy-tool",
        "run: cargo build -p xtask --locked --release",
        "run: gh workflow run deploy.yml --repo komira-ai/docker-swarm-updater",
        "deploy-tool: names docker-swarm-updater; only cp-deploy calls it",
    ),
];

#[test]
fn each_defect_of_the_control_plane_deploy_is_named() {
    each_named("release.yml", problems_of, DEFECTS);
}
