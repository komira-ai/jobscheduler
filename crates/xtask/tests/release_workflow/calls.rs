//! The jobs of the release that call a workflow instead of running steps of their own.

/// The one local workflow a job of the release calls, as its `uses:` line writes it.
pub const CALLED: &str = "uses: ./.github/workflows/artifacts.yml";

/// The jobs of the release that call a workflow, and the workflow each calls: artifacts.yml, and
/// docker-swarm-updater's deploy workflow at the full commit its allow-list names.
pub const CALLERS: [(&str, &str); 2] = [
    ("artifacts", "./.github/workflows/artifacts.yml"),
    (
        "cp-deploy",
        "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6 # v0.1.3",
    ),
];

/// The `uses:` value of a job that calls a workflow (`uses:` at the job's own level), if it is one.
pub fn calls(lines: &[String]) -> Option<&str> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("    uses: "))
        .map(str::trim)
}
