//! artifacts.yml, the workflow that builds and signs the release's files: release.yml calls it
//! and nothing else starts it, its `attest` job is the one place anything is signed, and it
//! signs only for a push to main, from files it checks against the sums the build made.

use super::{artifacts_workflow, job, jobs, workflow};

/// The condition of the jobs that sign or check signatures: a push to main.
const PUSH_TO_MAIN: &str =
    "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The text between `on:` and the next top-level key.
fn on_block(text: &str) -> &str {
    text.split_once("\non:\n")
        .and_then(|(_, rest)| rest.split_once("\npermissions:"))
        .map(|(on, _)| on)
        .expect("on block")
}

/// The `if:` lines of a job.
fn ifs(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .filter(|l| l.starts_with("    if:"))
        .map(String::as_str)
        .collect()
}

/// Asserts that artifacts.yml starts on `workflow_call` alone, with one required string input,
/// the version; and that release.yml's `artifacts` job calls it with the plan's version and
/// grants exactly what `attest` needs. Catches a push or pull request trigger of its own (which
/// would build under a version `plan` did not choose, or sign outside a release) and a call that
/// passes another version or grants a write besides the two the signing needs.
#[test]
fn artifacts_yml_is_started_only_by_the_release_s_call() {
    assert_eq!(
        on_block(&artifacts_workflow()),
        "  workflow_call:\n    inputs:\n      version:\n        description: \"The release version X.Y.Z that release.yml's plan chose: the binaries print it and the files are named by it\"\n        type: string\n        required: true\n"
    );
    let release = workflow();
    let jobs = jobs(&release);
    let call = job(&jobs, "artifacts");
    let body: Vec<&str> = call
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect();
    assert_eq!(
        body,
        [
            "    needs: [plan]",
            "    if: ${{ !cancelled() && needs.plan.outputs.release == 'true' }}",
            "    permissions:",
            "      contents: read",
            "      id-token: write",
            "      attestations: write",
            "    uses: ./.github/workflows/artifacts.yml",
            "    with:",
            "      version: ${{ needs.plan.outputs.version }}",
        ]
    );
}

/// Asserts that `attest` and `verify` each run only for a push to main (one `if:`, that one),
/// in that order after `sums`, and that `build` and `sums` have no condition, so the dry run of a
/// pull request builds and sums and signs nothing. Catches a signing job that a pull request
/// would run, and a build the dry run would skip.
#[test]
fn attest_and_verify_run_only_for_a_push_to_main() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    for (name, needs) in [("attest", "[sums]"), ("verify", "[attest]")] {
        let lines = job(&jobs, name);
        assert_eq!(ifs(lines), [PUSH_TO_MAIN], "{name}");
        assert!(
            lines.iter().any(|l| *l == format!("    needs: {needs}")),
            "{name}"
        );
    }
    assert!(ifs(job(&jobs, "build")).is_empty());
    assert!(ifs(job(&jobs, "sums")).is_empty());
    assert!(job(&jobs, "sums").iter().any(|l| l == "    needs: [build]"));
}

/// Asserts that `attest`, which holds `id-token: write`, checks out nothing, builds nothing and
/// runs no `cargo` or `rustup`; that before its first signing step it refuses a SHA256SUMS whose
/// SHA-256 is not the one `sums` wrote as its output (from the step that wrote the sums), and then
/// any listed file that differs from the sums. Catches a signing job that runs repository code
/// beside the token, and one that signs whatever reached the artifact store.
#[test]
fn attest_runs_no_repository_code_and_signs_only_the_files_the_build_summed() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let attest = job(&jobs, "attest").join("\n");
    for banned in ["actions/checkout", "cargo ", "rustup"] {
        assert!(!attest.contains(banned), "{banned}: {attest}");
    }
    let refuse_sums = attest
        .find("SUMS_SHA256: ${{ needs.sums.outputs.sha256 }}\n        run: /usr/bin/sha256sum --check --strict <<< \"$SUMS_SHA256  dist/SHA256SUMS\"")
        .expect("the check of the sums' digest");
    let refuse_files = attest
        .find(
            "working-directory: dist\n        run: /usr/bin/sha256sum --check --strict SHA256SUMS",
        )
        .expect("the check of the files");
    let first_signing = attest
        .find("actions/attest-build-provenance@")
        .expect("a signing step");
    assert!(
        refuse_sums < refuse_files && refuse_files < first_signing,
        "{attest}"
    );
    // The digest is the sums job's output, written by the step that wrote SHA256SUMS.
    let sums = job(&jobs, "sums").join("\n");
    assert!(
        sums.contains("    outputs:\n      sha256: ${{ steps.sums.outputs.sha256 }}"),
        "{sums}"
    );
    let write = sums
        .find("      - name: Write and check SHA256SUMS\n        id: sums\n")
        .expect("the sums step");
    let written = sums
        .find("sha256sum -- *.tar.gz *.tar.zst js-agent-index.json > SHA256SUMS")
        .expect("the sums");
    let output = sums
        .find("          sha256=\"$(sha256sum SHA256SUMS | cut -d ' ' -f 1)\"\n          echo \"sha256=$sha256\" >> \"$GITHUB_OUTPUT\"\n")
        .expect("the digest output");
    assert!(write < written && written < output, "{sums}");
}

/// Each `<file>/<job>` of `workflows` (file name and text) with a step that signs: one of
/// GitHub's attestation actions, or a `cosign sign` or `cosign attest` command.
fn signing_jobs(workflows: &[(&str, String)]) -> Vec<String> {
    let mut found = Vec::new();
    for (file, text) in workflows {
        for (name, lines) in jobs(text) {
            if lines.iter().any(|l| {
                let l = l.trim_start();
                !l.starts_with('#')
                    && (l.contains("actions/attest-build-provenance@")
                        || l.contains("actions/attest-sbom@")
                        || l.contains("actions/attest@")
                        || l.contains("cosign sign")
                        || l.contains("cosign attest"))
            }) {
                found.push(format!("{file}/{name}"));
            }
        }
    }
    found
}

/// Asserts that artifacts.yml's `attest` is the one job of the two files that signs, with four
/// provenance steps (the sums and the three packages), and that no file names release.yml as a
/// signer. And, on release.yml with an attestation step put back into `publish`, that the same
/// check finds it. Catches a signing step left in or added to release.yml, whose attestations no
/// farm host trusts.
#[test]
fn only_artifacts_yml_signs() {
    let release = workflow();
    let artifacts = artifacts_workflow();
    let both = [
        ("release.yml", release.clone()),
        ("artifacts.yml", artifacts.clone()),
    ];
    assert_eq!(signing_jobs(&both), ["artifacts.yml/attest"]);
    assert_eq!(
        artifacts
            .matches("uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2")
            .count(),
        4
    );
    for (file, text) in &both {
        assert!(
            !text.contains("--signer-workflow \"$REPO/.github/workflows/release.yml\""),
            "{file}"
        );
    }
    // The same check on release.yml with a signing step in `publish`.
    let at = "      - name: Draft the release, add the files it lacks, publish it\n";
    assert_eq!(release.matches(at).count(), 1);
    let signed = release.replace(
        at,
        &format!(
            "      - uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2\n        with:\n          subject-checksums: dist/SHA256SUMS\n{at}"
        ),
    );
    assert_eq!(
        signing_jobs(&[("release.yml", signed), ("artifacts.yml", artifacts)]),
        ["release.yml/publish", "artifacts.yml/attest"]
    );
}
