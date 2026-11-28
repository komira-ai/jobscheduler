//! macos-farm.yml, the workflow that builds the Apple silicon archives and the installer package on
//! the build farm's Macs from hosted Linux runners (docs/mac-farm.md), and the gates that choose it
//! over the hosted macOS runners: the repository variable MACOS_ON_FARM in artifacts.yml and
//! js-agent-macos-pkg.yml. The rules of the token it holds are linted by `tests/apt_workflow`
//! (`token_problems`); these tests pin its shape: the runner, the permissions, the conditions,
//! the artifacts it hands to the jobs that download them, and the files it fetches.

use super::{MACOS_BUILDS, artifacts_workflow, job, jobs, read_workflow, step};

/// The jobs of macos-farm.yml, in file order.
const JOBS: [&str; 4] = ["xtask", "release", "pkg", "pkg-update"];

/// The condition of a job that holds the token: a push to main or a manual run, and its `what`.
fn token_if(what: &str) -> String {
    format!(
        "    if: ${{{{ inputs.what == '{what}' && ((github.event_name == 'push' && github.ref == 'refs/heads/main') || github.event_name == 'workflow_dispatch') }}}}"
    )
}

/// The bins of `package:binary` entries, in order.
fn bins(builds: &str) -> Vec<&str> {
    builds
        .split_whitespace()
        .map(|entry| entry.split_once(':').expect("package:binary").1)
        .collect()
}

/// The value of `key: value` among `lines` (trimmed), once.
fn value<'a>(lines: &'a [String], key: &str) -> &'a str {
    let found: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.trim().strip_prefix(&format!("{key}: ")))
        .collect();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0]
}

/// The lines of a job's `permissions:` block, trimmed.
fn permissions(lines: &[String]) -> Vec<&str> {
    let start = lines
        .iter()
        .position(|l| l == "    permissions:")
        .expect("permissions");
    lines[start + 1..]
        .iter()
        .take_while(|l| l.starts_with("      "))
        .map(|l| l.trim())
        .collect()
}

/// The `if:` lines of a job.
fn ifs(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .filter(|l| l.starts_with("    if:"))
        .map(String::as_str)
        .collect()
}

/// The lines of `lines` that are not comments.
fn code(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

/// What a step that uploads an artifact says: its name, its path and its retention.
#[derive(Debug, PartialEq, Eq)]
struct Upload {
    name: String,
    path: Vec<String>,
    retention: String,
}

/// The uploads of a job, in order, read from the `with:` of each `actions/upload-artifact` step.
fn uploads(lines: &[String]) -> Vec<Upload> {
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if !line.trim().starts_with("- uses: actions/upload-artifact@") {
            continue;
        }
        let body: Vec<&String> = lines[at + 1..]
            .iter()
            .take_while(|l| !l.starts_with("      - "))
            .collect();
        let mut upload = Upload {
            name: String::new(),
            path: Vec::new(),
            retention: String::new(),
        };
        let mut in_path = false;
        for l in body {
            let t = l.trim();
            if let Some(name) = t.strip_prefix("name: ") {
                upload.name = name.to_owned();
                in_path = false;
            } else if let Some(path) = t.strip_prefix("path: ") {
                in_path = path == "|";
                if !in_path {
                    upload.path.push(path.to_owned());
                }
            } else if let Some(days) = t.strip_prefix("retention-days: ") {
                upload.retention = days.to_owned();
                in_path = false;
            } else if in_path && l.starts_with("            ") {
                upload.path.push(t.to_owned());
            } else {
                in_path = false;
            }
        }
        found.push(upload);
    }
    found
}

/// The condition that is true exactly when `condition` (`<term> && <term>...`, each term `a != b`,
/// `a == b` or `!x`) is false, as De Morgan gives it: the terms negated and joined with `||`.
fn negation(condition: &str) -> String {
    let inner = condition
        .trim()
        .strip_prefix("if: ${{ ")
        .and_then(|c| c.strip_suffix(" }}"))
        .expect("an `if: ${{ ... }}`");
    let terms: Vec<String> = inner
        .split(" && ")
        .map(|term| {
            if let Some(x) = term.strip_prefix('!') {
                x.to_owned()
            } else if term.contains(" != ") {
                term.replace(" != ", " == ")
            } else {
                assert!(term.contains(" == "), "{term}");
                term.replace(" == ", " != ")
            }
        })
        .collect();
    format!("if: ${{{{ {} }}}}", terms.join(" || "))
}

/// The negation reader, on the forms the gates use and the one it refuses.
#[test]
fn the_negation_reader_applies_de_morgan_to_the_gates() {
    assert_eq!(
        negation("if: ${{ vars.A != 'true' }}"),
        "if: ${{ vars.A == 'true' }}"
    );
    assert_eq!(
        negation("if: ${{ vars.A != 'true' && !inputs.farm }}"),
        "if: ${{ vars.A == 'true' || inputs.farm }}"
    );
    assert_eq!(negation("if: ${{ a == 'x' }}"), "if: ${{ a != 'x' }}");
    let refused = std::panic::catch_unwind(|| negation("if: ${{ always() }}"));
    assert!(refused.is_err());
}

/// Asserts that macos-farm.yml is started by a call or by hand and by nothing else, grants nothing
/// at its top, has exactly its four jobs, each on `ubuntu-24.04` alone (no macOS, no arm64, no
/// other runner: the Macs are the farm's, reached over the network), and names no secret.
/// Catches a trigger a pull request could start, a job that moved to a hosted Mac, and a secret.
#[test]
fn macos_farm_yml_is_called_or_run_by_hand_and_runs_on_ubuntu_24_04_alone() {
    let text = read_workflow("macos-farm.yml");
    let on = text
        .split_once("\non:\n")
        .and_then(|(_, rest)| rest.split_once("\npermissions:"))
        .map(|(on, _)| on)
        .expect("on block");
    assert_eq!(
        on,
        "  workflow_call:\n    inputs:\n      version:\n        description: The release version X.Y.Z, which the binaries print and the files are named by\n        type: string\n        required: true\n      what:\n        description: \"What to build: release (the archives) or pkg (the installer package and its update package)\"\n        type: string\n        required: true\n  workflow_dispatch:\n    inputs:\n      version:\n        description: The version X.Y.Z, which the binaries print and the files are named by\n        type: string\n        required: true\n      what:\n        description: What to build\n        type: choice\n        options: [release, pkg]\n        required: true\n"
    );
    assert!(text.lines().any(|l| l == "permissions: {}"));
    let all = jobs(&text);
    assert_eq!(
        all.iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        JOBS
    );
    for (name, lines) in &all {
        let runs_on: Vec<&String> = lines
            .iter()
            .filter(|l| l.trim_start().starts_with("runs-on:"))
            .collect();
        assert_eq!(runs_on, ["    runs-on: ubuntu-24.04"], "{name}");
    }
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for banned in [
            "macos-1",
            "macos-latest",
            "ubuntu-24.04-arm",
            "self-hosted",
            "secrets.",
            "pull_request",
        ] {
            assert!(!line.contains(banned), "{banned}: {line}");
        }
    }
}

/// Asserts the rights and conditions of the jobs: `xtask` holds `contents: read` alone and runs
/// whenever a caller or a manual run names a `what`; each of the three jobs that join the farm's
/// network holds `contents: read` and `id-token: write` and nothing else, under a condition that
/// names its `what` and a push to main or a manual run (the push to main with its ref, which
/// `tests/apt_workflow` holds every token job to). Catches a token job without the event pin,
/// one for the wrong `what`, and a write besides the token.
#[test]
fn only_the_jobs_that_join_the_farm_hold_the_token_and_only_for_a_push_to_main_or_a_manual_run() {
    let text = read_workflow("macos-farm.yml");
    let all = jobs(&text);
    let xtask = job(&all, "xtask");
    assert_eq!(permissions(xtask), ["contents: read"]);
    assert_eq!(
        ifs(xtask),
        ["    if: ${{ inputs.what == 'release' || inputs.what == 'pkg' }}"]
    );
    for (name, what) in [
        ("release", "release"),
        ("pkg", "pkg"),
        ("pkg-update", "pkg"),
    ] {
        let lines = job(&all, name);
        assert_eq!(
            permissions(lines),
            ["contents: read", "id-token: write"],
            "{name}"
        );
        assert_eq!(ifs(lines), [token_if(what)], "{name}");
        assert!(
            lines.iter().any(|l| l == "    needs: [xtask]"),
            "{name} waits for the build of xtask"
        );
    }
}

/// Asserts that no job that holds the token runs code compiled from the repository: it runs no
/// `cargo`, downloads the xtask the token-less `xtask` job built and uploaded under the name its
/// `what` gives (`mac-farm-xtask-<what>`, the choices of the manual run), joins the farm's network
/// with the pinned action under the variables only, and joins it before the verb that talks to
/// the farm. Catches a build step put beside the token, a download of an artifact the `xtask` job
/// does not upload, and a join that can run without the farm's variables.
#[test]
fn the_jobs_that_hold_the_token_run_only_the_prebuilt_xtask() {
    let text = read_workflow("macos-farm.yml");
    let all = jobs(&text);
    let builder = job(&all, "xtask").join("\n");
    assert!(
        builder.contains("run: cargo build -p xtask --locked\n"),
        "{builder}"
    );
    assert!(
        builder.contains(
            "          name: mac-farm-xtask-${{ inputs.what }}\n          path: target/debug/xtask\n          if-no-files-found: error\n          retention-days: 1\n"
        ),
        "{builder}"
    );
    assert!(
        read_workflow("macos-farm.yml").contains("        options: [release, pkg]\n"),
        "the manual run's choices are the names the jobs download"
    );
    for (name, what) in [
        ("release", "release"),
        ("pkg", "pkg"),
        ("pkg-update", "pkg"),
    ] {
        let lines = job(&all, name);
        let body = code(lines).join("\n");
        assert!(!body.contains("cargo "), "{name}: {body}");
        assert!(!body.contains("cargo\n"), "{name}: {body}");
        assert!(
            body.contains(&format!(
                "          name: mac-farm-xtask-{what}\n          path: tools\n"
            )),
            "{name}: {body}"
        );
        assert!(body.contains("run: chmod +x tools/xtask\n"), "{name}");
        let join = step(lines, "Join the farm's Tailscale network").join("\n");
        assert!(
            join.contains("        if: ${{ vars.TS_CLIENT_ID != '' }}\n        uses: tailscale/github-action@d1b6cd204f8dceda5b3eaad7f1f767be390056cd # v4.2.0\n        with:\n          oauth-client-id: ${{ vars.TS_CLIENT_ID }}\n          audience: ${{ vars.TS_AUDIENCE }}\n          tags: tag:ci"),
            "{name}: {join}"
        );
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("{name}: {needle}"))
        };
        assert!(
            at("name: Join the farm's Tailscale network") < at("tools/xtask mac-farm"),
            "{name}"
        );
        assert!(
            at("tools/xtask release stamp") < at("tools/xtask mac-farm"),
            "{name}"
        );
        // The farm is the organisation's variables, and the verb's environment holds only them.
        for (key, from) in [
            ("FARM_ADDRESS", "vars.FARM_ADDRESS"),
            ("FARM_INSTANCE", "vars.FARM_INSTANCE"),
            ("FARM_TLS", "vars.FARM_TLS || 'false'"),
            ("FARM_PROPERTIES", "vars.FARM_DARWIN_ARM64_PROPERTIES"),
        ] {
            assert!(
                body.contains(&format!("          {key}: ${{{{ {from} }}}}\n")),
                "{name}: {key}"
            );
        }
        assert!(
            body.contains(r#"--address "$FARM_ADDRESS" --instance "$FARM_INSTANCE" --tls "$FARM_TLS" --properties "$FARM_PROPERTIES" --scratch "$RUNNER_TEMP/mac-farm""#),
            "{name}"
        );
    }
}

/// Asserts that the `release` job builds on the farm and archives what the hosted build does: the
/// same four `package:binary` entries (`BUILDS`, read from `build-macos`), `BINS` the binaries
/// of those entries, which the four literal `--output` flags the verb fetches name and `release
/// pack --bins` archives, the deployment target and the floor both 11.0, and the artifact
/// `dist-aarch64-apple-darwin` uploaded as `build-macos` uploads it, which `sums` expects the
/// four archives of. Catches an output the pack step does not archive, an archive of a file the
/// verb does not fetch, a binary dropped from one of the two builds, and an artifact `sums`
/// would not find.
#[test]
fn the_release_job_fetches_what_it_packs_and_uploads_what_sums_downloads() {
    let text = read_workflow("macos-farm.yml");
    let all = jobs(&text);
    let release = job(&all, "release");
    let artifacts = artifacts_workflow();
    let hosted_jobs = jobs(&artifacts);
    let hosted = job(&hosted_jobs, "build-macos");
    assert_eq!(value(release, "BUILDS"), MACOS_BUILDS);
    assert_eq!(value(release, "BUILDS"), value(hosted, "BUILDS"));
    assert_eq!(value(release, "TARGET"), "aarch64-apple-darwin");
    assert_eq!(value(release, "TARGET"), value(hosted, "TARGET"));
    assert_eq!(
        value(release, "BINS")
            .split_whitespace()
            .collect::<Vec<_>>(),
        bins(MACOS_BUILDS)
    );
    let run = step(release, "Build the binaries on a farm Mac").join("\n");
    let flags: String = bins(MACOS_BUILDS)
        .iter()
        .map(|bin| format!(" --output target/aarch64-apple-darwin/release/{bin}"))
        .collect();
    assert!(
        run.contains(&format!(
            r#" --env MACOSX_DEPLOYMENT_TARGET=11.0{flags} -- xtask release macos-leg --target "$TARGET" --builds "$BUILDS" --min-macos 11.0 --out target/macos-leg"#
        )),
        "{run}"
    );
    assert_eq!(run.matches("--output").count(), 4, "{run}");
    let pack = step(release, "Pack the archives").join("\n");
    assert!(
        pack.contains(r#"run: tools/xtask release pack --bins "$BINS" --target "$TARGET" --version "$VERSION" --mtime-of "$COMMIT" --out dist"#),
        "{pack}"
    );
    assert_eq!(pack.matches("release pack").count(), 1, "{pack}");
    assert!(!pack.contains("tar "), "{pack}");
    // The uploads are the same artifact, so `sums` downloads either.
    let theirs = uploads(release);
    assert_eq!(theirs, uploads(hosted));
    assert_eq!(theirs.len(), 1, "{theirs:?}");
    assert_eq!(theirs[0].name, "dist-aarch64-apple-darwin");
    let sums = artifacts_workflow();
    let sums_jobs = jobs(&sums);
    let sums = job(&sums_jobs, "sums").join("\n");
    assert!(
        sums.contains("pattern: dist-*\n          merge-multiple: true\n"),
        "{sums}"
    );
    for bin in bins(MACOS_BUILDS) {
        let asset = format!("{bin}-$VERSION-aarch64-apple-darwin.tar.gz");
        assert!(sums.contains(&asset), "{asset} is not expected: {sums}");
    }
    // Nothing that packs is guarded by the farm's address: the verb fails without it (see
    // `a_run_without_a_farm_fails_by_name_in_the_verb`), so no archive is made from nothing and
    // none is skipped.
    assert!(!pack.contains("        if:"), "{pack}");
}

/// Asserts that the package jobs upload exactly the artifacts, under the names, paths and
/// retention, that the hosted `build` and `build-update` of js-agent-macos-pkg.yml upload (the
/// signing and first-install jobs download them by name), and that the files the verb fetches are
/// those the uploads and the checks of the fetched files name. Catches an artifact renamed on one
/// side, a file uploaded that was not fetched, and a fetched file nobody uses.
#[test]
fn the_package_jobs_upload_what_the_hosted_package_jobs_upload() {
    let text = read_workflow("macos-farm.yml");
    let all = jobs(&text);
    let package = read_workflow("js-agent-macos-pkg.yml");
    let hosted = jobs(&package);
    assert_eq!(uploads(job(&all, "pkg")), uploads(job(&hosted, "build")));
    assert_eq!(
        uploads(job(&all, "pkg-update")),
        uploads(job(&hosted, "build-update"))
    );
    let names: Vec<String> = uploads(job(&all, "pkg"))
        .into_iter()
        .chain(uploads(job(&all, "pkg-update")))
        .map(|u| u.name)
        .collect();
    assert_eq!(
        names,
        [
            "js-agent-macos-pkg-unsigned",
            "js-agent-macos-programs-unsigned",
            "js-agent-macos-checker",
            "js-agent-macos-pkg-update-unsigned"
        ]
    );
    let outputs = |name: &str, step_name: &str| -> Vec<String> {
        let run = step(job(&all, name), step_name).join("\n");
        run.split(" --output ")
            .skip(1)
            .map(|rest| {
                rest.split_whitespace()
                    .next()
                    .expect("a path")
                    .trim_matches('"')
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(
        outputs("pkg", "Build the package on a farm Mac"),
        [
            "dist/js-agent-$VERSION-macos-arm64.pkg",
            "dist/SHA256SUMS",
            "target/release/js-agent",
            "target/release/js-mac-provision",
            "target/release/PROGRAMS.SHA256SUMS",
            "target/debug/xtask",
        ]
    );
    assert_eq!(
        outputs("pkg-update", "Build the update package on a farm Mac"),
        ["dist/js-agent-$NEXT-macos-arm64.pkg", "dist/SHA256SUMS"]
    );
    // The fetched sums are checked against the fetched files, on this runner.
    let checked = step(
        job(&all, "pkg"),
        "The fetched package is the one that was summed",
    )
    .join("\n");
    assert!(
        checked.contains(
            "        working-directory: dist\n        run: sha256sum --check --strict SHA256SUMS"
        ),
        "{checked}"
    );
    let programs = step(
        job(&all, "pkg"),
        "The fetched programs are the ones that were summed",
    )
    .join("\n");
    assert!(
        programs.contains("        working-directory: target/release\n        run: sha256sum --check --strict PROGRAMS.SHA256SUMS"),
        "{programs}"
    );
    // The verb that builds is the one the hosted action's steps became, for the version asked.
    let pkg = step(job(&all, "pkg"), "Build the package on a farm Mac").join("\n");
    assert!(
        pkg.contains(r#"--env MACOSX_DEPLOYMENT_TARGET=11.0 --output"#)
            && pkg.ends_with(r#"-- xtask pkg build-macos --version "$VERSION""#),
        "{pkg}"
    );
    let update = step(
        job(&all, "pkg-update"),
        "Build the update package on a farm Mac",
    )
    .join("\n");
    assert!(
        update.ends_with(r#"-- xtask pkg build-macos --version "$NEXT""#),
        "{update}"
    );
    // The update package is of the version after the release's, named by the one place that names it.
    let next = step(job(&all, "pkg-update"), "Name the next version").join("\n");
    assert!(
        next.contains(r#"run: tools/xtask release next-version --version "$VERSION" --output "$GITHUB_OUTPUT""#),
        "{next}"
    );
}

/// Asserts the gates that choose the farm over the hosted runners, in artifacts.yml and in
/// js-agent-macos-pkg.yml: each hosted job's condition is exactly the negation of its farm
/// call's, so one of the two runs and never both (the same artifact names would clash), the
/// package workflow's call asks for `what: pkg` with its own version and grants what its jobs
/// hold, and the hosted ones hold no token. Catches a gate edited on one side only.
#[test]
fn the_hosted_jobs_and_the_farm_calls_are_each_others_negation() {
    let artifacts = artifacts_workflow();
    let a = jobs(&artifacts);
    let hosted = ifs(job(&a, "build-macos"))[0].trim().to_owned();
    let farm = ifs(job(&a, "macos-farm"))[0].trim().to_owned();
    assert_eq!(negation(&hosted), farm);
    assert_eq!(negation(&farm), hosted);

    let package = read_workflow("js-agent-macos-pkg.yml");
    let p = jobs(&package);
    let farm_call = ifs(job(&p, "farm"));
    assert_eq!(
        farm_call,
        ["    if: ${{ vars.MACOS_ON_FARM == 'true' || inputs.farm }}"]
    );
    for name in ["build", "build-update"] {
        let lines = job(&p, name);
        assert_eq!(
            ifs(lines),
            ["    if: ${{ vars.MACOS_ON_FARM != 'true' && !inputs.farm }}"],
            "{name}"
        );
        assert!(!lines.join("\n").contains("id-token"), "{name}");
    }
    // `||` of two terms negates to `&&`: checked from the hosted side, which has the `&&`.
    assert_eq!(
        negation(ifs(job(&p, "build"))[0].trim()),
        "if: ${{ vars.MACOS_ON_FARM == 'true' || inputs.farm }}"
    );
    let body: Vec<&str> = job(&p, "farm")
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect();
    assert_eq!(
        body,
        [
            "    if: ${{ vars.MACOS_ON_FARM == 'true' || inputs.farm }}",
            "    permissions:",
            "      contents: read",
            "      id-token: write",
            "    uses: ./.github/workflows/macos-farm.yml",
            "    with:",
            "      version: ${{ inputs.version }}",
            "      what: pkg",
        ]
    );
}

/// Asserts that a run without a farm fails in the verb and not in the workflow: the `xtask
/// mac-farm` step of each job that holds the token is unguarded (no `if:`) and reads the address
/// from the organisation variable, no step is named for a check of the farm, and no step of the
/// token jobs is guarded by FARM_ADDRESS (only the join is guarded, by the tailnet identity).
/// Catches a farm run that ends green having built nothing and a guard that skips the verb.
#[test]
fn a_run_without_a_farm_fails_by_name_in_the_verb() {
    let text = read_workflow("macos-farm.yml");
    let all = jobs(&text);
    for (name, build) in [
        ("release", "Build the binaries on a farm Mac"),
        ("pkg", "Build the package on a farm Mac"),
        ("pkg-update", "Build the update package on a farm Mac"),
    ] {
        let lines = job(&all, name);
        let verb = step(lines, build).join("\n");
        assert!(
            verb.contains("          FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}\n"),
            "{name}: {verb}"
        );
        assert!(
            verb.contains("run: tools/xtask mac-farm --address \"$FARM_ADDRESS\""),
            "{name}: {verb}"
        );
        assert!(
            !verb.contains("        if:"),
            "{name}: the verb is unguarded"
        );
        let body = code(lines).join("\n");
        assert!(!body.contains("FARM_ADDRESS != ''"), "{name}: {body}");
        assert!(!body.contains("Check the farm"), "{name}: {body}");
        assert!(!body.contains("-z \"$FARM_ADDRESS\""), "{name}: {body}");
        let gated: Vec<&str> = code(lines)
            .into_iter()
            .filter(|l| l.starts_with("        if:"))
            .collect();
        assert_eq!(
            gated,
            ["        if: ${{ vars.TS_CLIENT_ID != '' }}"],
            "{name}: only the join is guarded"
        );
    }
}

/// The lines of `text` that are `run:` steps, with the line itself, and the reasons a step is more
/// than one command: a block scalar, or a line with `&&`, `||`, `;`, `$(`, a backtick, or a `for `
/// or `if ` keyword.
fn multi_command_runs(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let Some((_, rest)) = line.split_once("run:") else {
            continue;
        };
        if !(line.trim_start().starts_with("run:") || line.trim_start().starts_with("- run:")) {
            continue;
        }
        let rest = rest.trim();
        if rest.starts_with('|') || rest.starts_with('>') {
            problems.push(format!("block scalar: {line}"));
            continue;
        }
        let words = format!(" {rest} ");
        for banned in ["&&", "||", ";", "$(", "`", " for ", " if "] {
            if words.contains(banned) {
                problems.push(format!("{banned:?}: {line}"));
            }
        }
    }
    problems
}

/// Asserts that every `run:` of macos-farm.yml is one command line: no block scalar, and no `&&`,
/// `||`, `;`, command substitution, backtick, `for` or `if`; what a step does is a verb of xtask.
/// Catches a shell script grown back into a step. The negative case runs the same reader on a
/// copy of the text with a multi-line run put in, and on one with each banned form.
#[test]
fn every_step_of_macos_farm_yml_is_one_command() {
    let text = read_workflow("macos-farm.yml");
    assert!(text.contains("run: "), "the file has run steps");
    assert_eq!(multi_command_runs(&text), Vec::<String>::new());

    let with_block = text.replacen(
        "        run: chmod +x tools/xtask\n",
        "        run: |\n          set -euo pipefail\n          chmod +x tools/xtask\n",
        1,
    );
    assert_ne!(with_block, text);
    assert_eq!(
        multi_command_runs(&with_block),
        ["block scalar:         run: |"]
    );
    for banned in ["&&", "||", ";", "$(", "`"] {
        let with_form = text.replacen(
            "run: chmod +x tools/xtask\n",
            &format!("run: chmod +x tools/xtask {banned} true\n"),
            1,
        );
        assert_eq!(multi_command_runs(&with_form).len(), 1, "{banned}");
    }
    for keyword in ["for", "if"] {
        let with_keyword = text.replacen(
            "run: chmod +x tools/xtask\n",
            &format!("run: {keyword} true tools/xtask\n"),
            1,
        );
        assert_eq!(multi_command_runs(&with_keyword).len(), 1, "{keyword}");
    }
}
