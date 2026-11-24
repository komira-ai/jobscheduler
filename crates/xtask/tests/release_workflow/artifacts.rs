//! artifacts.yml, the workflow that builds and signs the release's files: release.yml calls it
//! and nothing else starts it, its `attest` job is the one place anything is signed, and it
//! signs only for a push to main, from files it checks against the sums the build made.

use super::{artifacts_workflow, job, jobs, workflow};

/// The condition of `attest`: `sums` passed, and a push to main. `!cancelled()` because without a
/// status function GitHub skips a job below a skipped one, and one of `build-macos` and
/// `macos-farm` is skipped in every run.
const ATTEST_IF: &str = "    if: ${{ !cancelled() && needs.sums.result == 'success' && github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The condition of `verify`: `attest` passed, and a push to main (as [`ATTEST_IF`]).
const VERIFY_IF: &str = "    if: ${{ !cancelled() && needs.attest.result == 'success' && github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The `if:` that artifacts.yml's `attest` and `verify` had while they skipped in every run.
const SKIPPED_IF: &str =
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
            "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}",
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

/// The condition of the hosted Apple silicon build: the repository variable MACOS_ON_FARM is not
/// `true`.
pub const HOSTED_MACOS_IF: &str = "    if: ${{ vars.MACOS_ON_FARM != 'true' }}";

/// The condition of the call of the farm's build: the variable is `true`. Exactly the negation of
/// [`HOSTED_MACOS_IF`], so one of the two jobs runs and never both.
pub const FARM_MACOS_IF: &str = "    if: ${{ vars.MACOS_ON_FARM == 'true' }}";

/// The condition of `sums`: the Linux legs built, and one of the two Apple silicon builds did.
const SUMS_IF: &str = "    if: ${{ !cancelled() && needs.build.result == 'success' && (needs.build-macos.result == 'success' || needs.macos-farm.result == 'success') }}";

/// Asserts that `attest` and `verify` each run only for a push to main and only after the job
/// before them passed (one `if:`, [`ATTEST_IF`] and [`VERIFY_IF`]), in that order after `sums`,
/// and that `build` has no condition of its own (the release's call runs it for a push alone)
/// and `sums` only the one that takes whichever of `build-macos` and `macos-farm` ran. Catches a
/// signing job that a pull request would run, a signing job that GitHub skips because the Apple
/// silicon build it does not use was skipped (no `bundles` artifact, so release.yml's `publish`
/// fails), a signing job that runs after `sums` failed, a build that skips itself, and a `sums`
/// that runs after a failed Linux leg or with neither Apple silicon build.
#[test]
fn attest_and_verify_run_only_for_a_push_to_main() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    for (name, needs, condition) in [
        ("attest", "[sums]", ATTEST_IF),
        ("verify", "[attest]", VERIFY_IF),
    ] {
        let lines = job(&jobs, name);
        assert_eq!(ifs(lines), [condition], "{name}");
        assert!(
            lines.iter().any(|l| *l == format!("    needs: {needs}")),
            "{name}"
        );
    }
    assert!(ifs(job(&jobs, "build")).is_empty());
    assert_eq!(ifs(job(&jobs, "sums")), [SUMS_IF]);
    assert!(
        job(&jobs, "sums")
            .iter()
            .any(|l| l == "    needs: [build, build-macos, macos-farm]")
    );
}

/// Asserts that the Apple silicon archives come from exactly one of two jobs: `build-macos`
/// under [`HOSTED_MACOS_IF`] and the call `macos-farm` under [`FARM_MACOS_IF`], which are each
/// other's negation. The call asks for the release version and `what: release`, grants
/// `contents: read` and `id-token: write` (the most its jobs can hold) and nothing else, and has
/// no steps of its own. Catches both jobs running (a duplicate `dist-aarch64-apple-darwin`
/// artifact), neither running, and a call that grants a write besides the token.
#[test]
fn the_apple_silicon_archives_come_from_one_of_the_hosted_build_and_the_farm() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    assert_eq!(ifs(job(&jobs, "build-macos")), [HOSTED_MACOS_IF]);
    assert_eq!(ifs(job(&jobs, "macos-farm")), [FARM_MACOS_IF]);
    assert_eq!(
        HOSTED_MACOS_IF.replace(" != ", " == "),
        FARM_MACOS_IF,
        "the two conditions are each other's negation"
    );
    let body: Vec<&str> = job(&jobs, "macos-farm")
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect();
    assert_eq!(
        body,
        [
            FARM_MACOS_IF,
            "    permissions:",
            "      contents: read",
            "      id-token: write",
            "    uses: ./.github/workflows/macos-farm.yml",
            "    with:",
            "      version: ${{ inputs.version }}",
            "      what: release",
        ]
    );
    let hosted = job(&jobs, "build-macos")
        .iter()
        .filter(|l| !l.trim_start().starts_with('#'))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        hosted.contains("    permissions:\n      contents: read\n    env:"),
        "{hosted}"
    );
    assert!(!hosted.contains("id-token"), "{hosted}");
    // Both upload the one artifact `sums` downloads with the other legs' (`dist-*`).
    assert!(
        hosted
            .contains("          name: dist-aarch64-apple-darwin\n          path: dist/*.tar.gz\n"),
        "{hosted}"
    );
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
        .find("sha256sum -- *.tar.gz *.deb *.oci.tar > SHA256SUMS")
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

/// Asserts that artifacts.yml's `attest` is the one job of the two files that signs, with five
/// provenance steps (the sums, the two .debs and the two images), and that no file names release.yml as a
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
        5
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

/// The jobs `needs:` names of a job, from the flow form `[a, b]` or a single name.
fn needs(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("    needs: "))
        .map(|list| {
            list.trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The jobs of a workflow that sit below a job gated on MACOS_ON_FARM (one of each gated pair is
/// skipped in every run) and whose `if:` does not open with `!cancelled()` or checks no need's
/// result, in file order. GitHub skips such a job whenever a job anywhere above it was skipped,
/// so it never runs; and `!cancelled()` without a result check would run it after a failed need.
fn unguarded_below_the_farm_gate(text: &str) -> Vec<String> {
    let jobs = jobs(text);
    let mut below: Vec<&str> = jobs
        .iter()
        .filter(|(_, lines)| ifs(lines).iter().any(|l| l.contains("vars.MACOS_ON_FARM")))
        .map(|(name, _)| name.as_str())
        .collect();
    loop {
        let more: Vec<&str> = jobs
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| !below.contains(name))
            .filter(|name| {
                needs(job(&jobs, name))
                    .iter()
                    .any(|n| below.contains(&n.as_str()))
            })
            .collect();
        if more.is_empty() {
            break;
        }
        below.extend(more);
    }
    jobs.iter()
        .filter(|(name, lines)| {
            let gated = ifs(lines).iter().any(|l| l.contains("vars.MACOS_ON_FARM"));
            let guarded = ifs(lines).iter().any(|l| {
                l.starts_with("    if: ${{ !cancelled() && ")
                    && needs(lines)
                        .iter()
                        .any(|n| l.contains(&format!("needs.{n}.result == 'success'")))
            });
            below.contains(&name.as_str()) && !gated && !guarded
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Asserts that every job below the MACOS_ON_FARM gate, in artifacts.yml and in
/// js-agent-macos-pkg.yml, opens its `if:` with `!cancelled()` and checks a need's result; and
/// that the check names artifacts.yml's `attest` and `verify` with the `if:` they had before
/// (each alone and both), a job that has `!cancelled()` but checks no result, a job that checks
/// a result without `!cancelled()`, and a job with no `if:` at all. Catches a job below the gate
/// that GitHub skips in every run, as `attest` and `verify` were, which leaves release.yml's
/// `publish` without the `bundles` artifact.
#[test]
fn every_job_below_the_farm_gate_runs_when_one_side_of_it_is_skipped() {
    let text = artifacts_workflow();
    assert_eq!(unguarded_below_the_farm_gate(&text), Vec::<String>::new());
    assert_eq!(
        unguarded_below_the_farm_gate(&super::read_workflow("js-agent-macos-pkg.yml")),
        Vec::<String>::new()
    );
    let with = |pairs: &[(&str, &str)]| {
        pairs.iter().fold(text.clone(), |t, (from, to)| {
            assert_eq!(t.matches(from).count(), 1, "{from}");
            t.replace(from, to)
        })
    };
    assert_eq!(
        unguarded_below_the_farm_gate(&with(&[(ATTEST_IF, SKIPPED_IF)])),
        ["attest"]
    );
    assert_eq!(
        unguarded_below_the_farm_gate(&with(&[(VERIFY_IF, SKIPPED_IF)])),
        ["verify"]
    );
    assert_eq!(
        unguarded_below_the_farm_gate(&with(&[(ATTEST_IF, SKIPPED_IF), (VERIFY_IF, SKIPPED_IF)])),
        ["attest", "verify"]
    );
    let no_result = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' }}";
    assert_eq!(
        unguarded_below_the_farm_gate(&with(&[(VERIFY_IF, no_result)])),
        ["verify"]
    );
    let no_status = "    if: ${{ needs.attest.result == 'success' && github.event_name == 'push' && github.ref == 'refs/heads/main' }}";
    assert_eq!(
        unguarded_below_the_farm_gate(&with(&[(VERIFY_IF, no_status)])),
        ["verify"]
    );
    assert_eq!(
        unguarded_below_the_farm_gate(&with(&[(&format!("{VERIFY_IF}\n"), "")])),
        ["verify"]
    );
}
