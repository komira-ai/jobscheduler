//! The jobs of the release that call a workflow instead of running steps of their own.

/// The jobs of the release that call a workflow, and the workflow each calls: artifacts.yml, the
/// five workflows of the beta stage (the platforms other than Linux x86_64),
/// docker-swarm-updater's deploy workflow at the full commit its allow-list names, and the build
/// farm lane, which `blackbox` calls to build the black-box tests' target.
pub const CALLERS: [(&str, &str); 8] = [
    ("artifacts", "./.github/workflows/artifacts.yml"),
    ("beta-macos", "./.github/workflows/beta.yml"),
    (
        "beta-macos-pkg",
        "./.github/workflows/js-agent-macos-pkg.yml",
    ),
    (
        "beta-macos-first-install",
        "./.github/workflows/js-agent-macos-first-install.yml",
    ),
    ("beta-macos-probe", "./.github/workflows/macos-probe.yml"),
    (
        "beta-macos-isolation",
        "./.github/workflows/js-agent-native-isolation.yml",
    ),
    (
        "cp-deploy",
        "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6 # v0.1.3",
    ),
    ("blackbox", "./.github/workflows/farm.yml"),
];

/// The jobs of artifacts.yml that call a workflow, and the workflow each calls: the build of the
/// Apple silicon archives on the build farm.
pub const ARTIFACTS_CALLS: [(&str, &str); 1] =
    [("macos-farm", "./.github/workflows/macos-farm.yml")];

/// The `uses:` value of a job that calls a workflow (`uses:` at the job's own level), if it is one.
pub fn calls(lines: &[String]) -> Option<&str> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("    uses: "))
        .map(str::trim)
}
