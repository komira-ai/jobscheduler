//! The workflow that builds the macOS package (`.github/workflows/js-agent-macos-pkg.yml`)
//! read as it is in the repository. Two jobs: `build` runs in the release's beta stage and by hand
//! (never on a pull request), holds no secret and signs no package (its one signature is
//! `js-vmm`'s ad hoc one from `pkg entitle-macos`, which needs no key), and builds a hostkeeper
//! `macos-pkg` package (no script, the file `js-agent-<version>-macos-arm64.pkg`, the helper's
//! plist naming the agent of the same package by its cdhash); `sign` runs only on main (by hand,
//! or in the release's call that asks for it), in the protected environment `pkg-signing`, takes
//! the checked unsigned package from `build` without rebuilding, and is the only place a secret
//! is named, on the step that signs and nowhere else. Nothing is notarized. Neither job
//! publishes anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const WORKFLOW: &str = ".github/workflows/js-agent-macos-pkg.yml";
const README: &str = "packaging/js-agent-macos/README.md";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of the workflow without comments.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

/// The jobs: name and the lines of the block (those indented more than the name).
fn jobs(text: &str) -> BTreeMap<String, Vec<&str>> {
    let mut found: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut in_jobs = false;
    let mut current: Option<String> = None;
    for line in code(text) {
        if line == "jobs:" {
            in_jobs = true;
        } else if in_jobs && !line.starts_with(' ') && !line.is_empty() {
            in_jobs = false;
            current = None;
        } else if in_jobs {
            let indent = line.len() - line.trim_start().len();
            if indent == 2 && line.trim_end().ends_with(':') {
                let name = line.trim().trim_end_matches(':').to_owned();
                found.insert(name.clone(), Vec::new());
                current = Some(name);
            } else if let Some(name) = &current {
                found.get_mut(name).unwrap().push(line);
            }
        }
    }
    found
}

/// The steps of a job: name and lines, from one `- name:` or `- uses:` to the next.
fn steps<'a>(job: &[&'a str]) -> Vec<(String, Vec<&'a str>)> {
    let mut found: Vec<(String, Vec<&str>)> = Vec::new();
    for line in job {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent == 6 && t.starts_with("- ") {
            let name = t.trim_start_matches("- name: ").trim().to_owned();
            found.push((name, vec![*line]));
        } else if let Some((_, lines)) = found.last_mut() {
            lines.push(line);
        }
    }
    found
}

fn job_text(text: &str, name: &str) -> String {
    jobs(text)[name].join("\n")
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[test]
fn the_workflow_has_a_build_job_and_a_sign_job() {
    let names: Vec<String> = jobs(&read(WORKFLOW)).into_keys().collect();
    assert_eq!(names, ["build", "sign"]);
}

/// The lines of the `on:` block, without comments.
fn on_block(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|l| *l != "on:")
        .skip(1)
        .take_while(|l| l.starts_with("  ") || l.starts_with('#') || l.is_empty())
        .filter(|l| l.starts_with("  ") && !l.trim_start().starts_with('#'))
        .collect()
}

#[test]
fn the_workflow_is_called_by_the_release_and_started_by_hand_but_never_by_a_pull_request() {
    let text = read(WORKFLOW);
    let on: Vec<&str> = on_block(&text).into_iter().map(str::trim).collect();
    assert_eq!(
        on,
        [
            "workflow_call:",
            "inputs:",
            "version:",
            "description: The package version, MAJOR.MINOR.PATCH",
            "required: false",
            "type: string",
            "default: 0.0.0",
            "sign:",
            "description: Sign the package (on main, for a push)",
            "required: false",
            "type: boolean",
            "default: false",
            "workflow_dispatch:",
            "inputs:",
            "version:",
            "description: The package version, MAJOR.MINOR.PATCH",
            "required: true",
            "type: string",
        ]
    );
    assert!(!text.contains("pull_request"));
    // A call without a version builds version 0.0.0.
    assert!(
        text.contains("  VERSION: ${{ inputs.version || '0.0.0' }}\n"),
        "the version"
    );
}

#[test]
fn manual_runs_of_one_version_on_one_ref_queue() {
    let text = read(WORKFLOW);
    assert!(
        text.contains("\nconcurrency:\n  group: js-agent-macos-pkg-${{ github.ref }}-${{ inputs.version }}\n  cancel-in-progress: false\n"),
        "the workflow's concurrency"
    );
}

#[test]
fn the_workflow_holds_read_access_only_and_publishes_nothing() {
    let text = read(WORKFLOW);
    assert!(text.lines().any(|l| l == "permissions: {}"));
    for (name, job) in jobs(&text) {
        let perms: Vec<&str> = job
            .iter()
            .skip_while(|l| l.trim() != "permissions:")
            .skip(1)
            .take_while(|l| indent(l) > 4)
            .map(|l| l.trim())
            .collect();
        assert_eq!(perms, ["contents: read"], "{name}");
    }
    let body = code(&text).join("\n");
    assert!(!body.contains("write"), "a write scope");
    assert!(!body.contains("id-token"));
    for word in [
        "gh release",
        "releases",
        "upload-release",
        "aws ",
        "gsutil",
        "gh api",
    ] {
        assert!(!body.contains(word), "{word}");
    }
}

#[test]
fn every_action_is_pinned_to_a_commit_and_both_jobs_run_on_macos_15() {
    let text = read(WORKFLOW);
    for line in code(&text).into_iter().filter(|l| l.contains("uses:")) {
        let pin = line.split('@').nth(1).unwrap_or("");
        assert!(
            pin.len() >= 40 && pin[..40].chars().all(|c| c.is_ascii_hexdigit()),
            "{line}"
        );
    }
    for (name, job) in jobs(&text) {
        assert!(
            job.iter().any(|l| l.trim() == "runs-on: macos-15"),
            "{name}"
        );
    }
    assert!(
        text.lines()
            .any(|l| l.contains("persist-credentials: false"))
    );
}

/// The build job's one signature is `pkg entitle-macos`'s ad hoc one, which needs no key: it
/// names no secret, opens no keychain and signs no package.
#[test]
fn the_build_job_names_no_secret_and_signs_with_no_key() {
    let text = read(WORKFLOW);
    let build = job_text(&text, "build");
    assert!(!build.contains("secret"), "the build job mentions a secret");
    for word in [
        "--keychain",
        "productsign",
        "notarytool",
        "stapler",
        "security ",
        "environment:",
    ] {
        assert!(!build.contains(word), "{word}");
    }
    assert!(!build.contains("MACOS_INSTALLER") && !build.contains("MACOS_NOTARY"));
}

#[test]
fn no_job_has_a_secret_in_its_environment() {
    let text = read(WORKFLOW);
    for (name, job) in jobs(&text) {
        // A job's own `env:` is a key at four spaces; its entries are at six.
        let job_env: Vec<&&str> = job
            .iter()
            .skip_while(|l| **l != "    env:")
            .skip(1)
            .take_while(|l| indent(l) >= 6)
            .collect();
        assert!(
            job_env.iter().all(|l| !l.contains("secrets.")),
            "{name}: {job_env:?}"
        );
    }
    let top: Vec<&str> = code(&text)
        .into_iter()
        .take_while(|l| *l != "jobs:")
        .filter(|l| l.contains("secrets."))
        .collect();
    assert!(top.is_empty(), "{top:?}");
}

#[test]
fn a_secret_is_named_only_in_the_environment_of_the_step_that_uses_it() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let mut named = BTreeSet::new();
    for (step, lines) in steps(&sign) {
        for line in lines.iter().filter(|l| l.contains("secrets.")) {
            let t = line.trim();
            let (key, value) = t.split_once(": ").unwrap_or_else(|| panic!("{line}"));
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "{line}"
            );
            if t.starts_with("if:") {
                continue;
            }
            assert!(
                value.starts_with("${{ secrets.") && value.ends_with("}}"),
                "{line}"
            );
            assert_eq!(indent(line), 10, "not an entry of a step's env: {line}");
            assert!(
                step == "Check the signing secrets" || step.starts_with("Sign"),
                "{step} names a secret"
            );
        }
        for line in &lines {
            for rest in line.split("secrets.").skip(1) {
                named.insert(
                    rest.chars()
                        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                        .collect::<String>(),
                );
            }
        }
    }
    let want: BTreeSet<String> = ["MACOS_INSTALLER_P12", "MACOS_INSTALLER_P12_PASSWORD"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(named, want);
    let readme = read(README);
    for name in &named {
        assert!(
            readme.contains(name.as_str()),
            "{name} is not in the README"
        );
    }
}

#[test]
fn the_sign_job_runs_only_on_main_by_hand_or_when_the_release_asks_in_the_protected_environment() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let at_four: Vec<&str> = sign
        .iter()
        .filter(|l| indent(l) == 4)
        .map(|l| l.trim())
        .collect();
    assert!(at_four.contains(&SIGN_IF), "{at_four:?}");
    assert!(at_four.contains(&"environment: pkg-signing"), "{at_four:?}");
    assert!(at_four.contains(&"needs: [build]"), "{at_four:?}");
}

#[test]
fn the_sign_job_runs_no_cargo_and_rebuilds_nothing() {
    let text = read(WORKFLOW);
    let sign = job_text(&text, "sign");
    assert!(!sign.contains("cargo"), "the sign job runs cargo");
    assert!(
        !sign.contains("rustup") && !sign.contains("pkgbuild") && !sign.contains("productbuild")
    );
    assert!(
        !sign.contains("actions/checkout"),
        "the sign job checks the tree out"
    );
    assert!(sign.contains("actions/download-artifact"));
}

const PRESENT: &str = "steps.signing.outputs.state == 'present'";

/// The `if:` of the sign job: a manual run on main, or the release's call that sets `sign` (a
/// push to main); a pull request is neither.
const SIGN_IF: &str = "if: ${{ (github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main') || (inputs.sign && github.ref == 'refs/heads/main' && github.event_name == 'push') }}";

#[test]
fn signing_is_one_step_gated_on_the_presence_check_and_nothing_is_notarized() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let all = steps(&sign);
    let gated: Vec<_> = all
        .iter()
        .filter(|(name, _)| name.starts_with("Sign"))
        .collect();
    assert_eq!(
        gated.len(),
        1,
        "{:?}",
        all.iter().map(|s| &s.0).collect::<Vec<_>>()
    );
    for (name, lines) in gated {
        let cond = lines
            .iter()
            .find(|l| l.trim_start().starts_with("if:"))
            .unwrap_or_else(|| panic!("{name} has no condition"));
        assert!(cond.contains(PRESENT), "{name}: {cond}");
    }
    let build = jobs(&text)["build"].clone();
    assert!(steps(&build).iter().all(|(n, _)| !n.starts_with("Sign")));
    for word in ["notarytool", "stapler", "xcrun", "Notarize"] {
        assert!(!code(&text).join("\n").contains(word), "{word}");
    }
}

#[test]
fn the_presence_check_comes_first_and_every_later_signing_step_waits_for_it() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let all = steps(&sign);
    let (name, lines) = &all[0];
    assert_eq!(name, "Check the signing secrets");
    let body = lines.join("\n");
    assert!(lines.iter().any(|l| l.trim() == "id: signing"), "{body}");
    // All six secrets reach it through the step's environment, and the script prints a
    // state and never a value.
    assert_eq!(body.matches("secrets.").count(), 2, "{body}");
    assert!(body.contains("state=present"), "{body}");
    assert!(body.contains("state=absent"), "{body}");
    assert!(body.contains("::notice::"), "{body}");
    assert!(body.contains("pkg-signing"), "{body}");
    assert!(body.contains(">> \"$GITHUB_OUTPUT\""), "{body}");
    for line in lines
        .iter()
        .filter(|l| l.contains("echo") || l.contains("printf"))
    {
        assert!(!line.contains("$MACOS_"), "a secret is printed: {line}");
    }
    // The version check follows; every step after the two waits for the state.
    assert_eq!(all[1].0, "Check the version");
    for (name, lines) in all.iter().skip(2) {
        assert!(
            lines
                .iter()
                .any(|l| l.trim_start().starts_with("if:") && l.contains(PRESENT)),
            "{name} does not wait for the presence check"
        );
    }
}

#[test]
fn the_key_is_imported_for_productsign_alone() {
    let sign = job_text(&read(WORKFLOW), "sign");
    let import = sign
        .lines()
        .find(|l| l.contains("security import"))
        .expect("the sign job imports the key");
    assert!(import.contains("-T /usr/bin/productsign"), "{import}");
    assert!(!import.contains(" -A"), "{import}");
}

#[test]
fn both_jobs_check_the_version_before_anything_else_but_the_presence_check() {
    let text = read(WORKFLOW);
    for (name, job) in jobs(&text) {
        let all = steps(&job);
        let at = usize::from(name == "sign");
        let (step, lines) = &all[at];
        assert_eq!(step, "Check the version", "{name}");
        let body = lines.join("\n");
        assert!(
            body.contains(
                r#"[[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]"#
            ),
            "{name}: {body}"
        );
        assert!(body.contains("exit 1"), "{name}");
    }
}

#[test]
fn the_package_is_built_checked_summed_and_checked_again_after_signing() {
    let text = read(WORKFLOW);
    let build = job_text(&text, "build");
    for needle in [
        "cargo xtask pkg stage-macos",
        "pkgbuild ",
        "productbuild ",
        "SHA256SUMS",
        "shasum -a 256 --check",
    ] {
        assert!(build.contains(needle), "build: {needle}");
    }
    assert_eq!(build.matches("pkg check-macos").count(), 1);
    let sign = job_text(&text, "sign");
    assert_eq!(
        sign.matches("pkg check-macos").count(),
        1,
        "once, after signing"
    );
    assert!(sign.contains("/usr/bin/shasum -a 256 --check"));
}

/// The lines of the steps of the sign job before its first signing step, as one text.
fn before_signing() -> String {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    steps(&sign)
        .into_iter()
        .take_while(|(name, _)| !name.starts_with("Sign"))
        .flat_map(|(_, lines)| lines)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn nothing_built_from_the_repository_runs_in_the_sign_job_before_the_secret_steps() {
    let before = before_signing();
    assert!(before.contains("Check the signing secrets"), "{before}");
    for word in ["tools/xtask", "check-macos", "chmod", "./"] {
        assert!(!before.contains(word), "a step before signing has {word:?}");
    }
    // The only program run from the downloaded files is shasum, by absolute path.
    for line in before.lines().filter(|l| l.contains("shasum")) {
        assert!(line.contains("/usr/bin/shasum"), "{line}");
    }
    assert!(before.contains("/usr/bin/shasum -a 256 --check SHA256SUMS"));
}

#[test]
fn the_checker_runs_in_the_sign_job_only_after_signing() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let all = steps(&sign);
    let at = |prefix: &str| all.iter().position(|(n, _)| n.starts_with(prefix)).unwrap();
    let notarize = at("Sign");
    let checks: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, (_, lines))| lines.iter().any(|l| l.contains("check-macos")))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(checks[0] > notarize, "{checks:?} {notarize}");
}

#[test]
fn the_sign_job_calls_the_system_tools_by_absolute_path() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let mut calls = 0;
    for line in sign.iter().filter(|l| !l.trim_start().starts_with("name:")) {
        for token in line.split(|c: char| c.is_whitespace() || "();|&\"'".contains(c)) {
            if ["security", "productsign", "xcrun", "shasum"].contains(&token) {
                panic!("{token} is called by name: {line}");
            }
            if [
                "/usr/bin/security",
                "/usr/bin/productsign",
                "/usr/bin/xcrun",
                "/usr/bin/shasum",
            ]
            .contains(&token)
            {
                calls += 1;
            }
        }
    }
    assert!(calls >= 10, "{calls}");
}

#[test]
fn no_command_expands_an_expression() {
    // Every `${{ }}` is in an `env:`, `with:` or `if:` value, never inside a script.
    let text = read(WORKFLOW);
    let mut in_script = false;
    for line in code(&text) {
        let t = line.trim_start();
        if t.starts_with("run: |") {
            in_script = true;
            continue;
        }
        if in_script && indent(line) < 10 {
            in_script = false;
        }
        if in_script {
            assert!(!line.contains("${{"), "{line}");
        }
        if t.starts_with("run: ") && !t.starts_with("run: |") {
            assert!(!line.contains("${{"), "{line}");
        }
    }
}

/// One line per way the workflow `text` is not the build of a hostkeeper `macos-pkg` package that
/// is then signed with `productsign` alone: the package has no script and the file name of the
/// design, the root helper's plist names the agent of the same package by its cdhash, the agent's
/// plist is the fixed one, js-vmm is built and given its entitlement by `pkg entitle-macos` once,
/// between its build and the staging, no program is signed by the workflow's own `codesign`,
/// the signing is one `productsign --timestamp` with the identity found in the temporary
/// keychain, nothing is notarized, and the artifacts are the two the release reads.
fn package_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let build = all.get("build").map_or(String::new(), |j| j.join("\n"));
    let sign = all.get("sign").map_or(String::new(), |j| j.join("\n"));
    let body = code(text).join("\n");
    let mut problems = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            problems.push(what.to_owned());
        }
    };
    want(
        !build.contains("--scripts") && !body.contains("stage/scripts"),
        "the package has scripts",
    );
    want(
        build.contains("pkgbuild --root stage/root --identifier com.komira.js-agent --version \"$VERSION\" --install-location / --ownership recommended stage/component.pkg"),
        "pkgbuild does not build the component of com.komira.js-agent from stage/root",
    );
    want(
        build.contains("\"dist/js-agent-$VERSION-macos-arm64.pkg\"")
            && !body.contains("aarch64-apple-darwin.pkg"),
        "the package is not named js-agent-<version>-macos-arm64.pkg",
    );
    want(
        build.contains("target/release/js-mac-provision agent-plist > inputs/agent.plist")
            && !body.contains("--server")
            && !body.contains("SET-THE"),
        "the agent's plist is not the fixed one",
    );
    want(
        build.contains(
            "session-plist --daemon-requirement \"cdhash H\\\"$cdhash\\\"\" > inputs/session.plist",
        ),
        "the helper's plist does not name the agent by its cdhash",
    );
    want(
        build.contains("cdhash=\"$(/usr/bin/codesign --display --verbose=4 target/release/js-agent 2>&1 | /usr/bin/sed -n 's/^CDHash=//p')\""),
        "the cdhash is not read from the js-agent binary that is built",
    );
    want(
        build.contains("[[ \"$cdhash\" =~ ^[0-9a-f]{40}$ ]]"),
        "the cdhash is not checked to be 40 hex digits",
    );
    want(
        build.contains("if [ \"$staged\" != \"$cdhash\" ]"),
        "the cdhash of the staged js-agent is not compared with the one in the plist",
    );
    want(
        build.contains("cargo build --locked --release -p js-vmm --bin js-vmm")
            && build.contains("for bin in js-agent js-mac-session js-mac-provision js-vmm; do"),
        "js-vmm is not built for the package",
    );
    let entitle = build.find("run: cargo xtask pkg entitle-macos --bin-dir target/release");
    let built = build.find("-p js-vmm --bin js-vmm");
    let staged = build.find("cargo xtask pkg stage-macos");
    want(
        matches!((built, entitle, staged), (Some(b), Some(e), Some(s)) if b < e && e < s)
            && build.matches("pkg entitle-macos").count() == 1,
        "js-vmm is not given its entitlement once, after it is built and before it is staged",
    );
    want(
        !body.contains("codesign --force") && !body.contains("codesign -f"),
        "the workflow signs a program itself, not with pkg entitle-macos",
    );
    want(
        !body.contains("notarytool") && !body.contains("stapler") && !body.contains("xcrun"),
        "something is notarized",
    );
    want(
        sign.contains("/usr/bin/productsign --timestamp --sign \"$identity\" --keychain \"$keychain\" \"$pkg\" \"$pkg.signed\""),
        "the package is not signed by productsign --timestamp with the identity of the keychain",
    );
    want(
        sign.contains("/usr/bin/security find-identity -v -p basic \"$keychain\"")
            && sign.contains("'/Developer ID Installer/ { print $2 }'")
            && sign.contains("[[ \"$identity\" =~ ^[0-9A-F]{40}$ ]]"),
        "the identity is not the one Developer ID Installer certificate of the keychain, by its SHA-1",
    );
    want(
        !body.contains("Installer:") && !body.contains("secrets.MACOS_INSTALLER_IDENTITY"),
        "an identity name is written or given as a secret",
    );
    want(
        build.contains("name: js-agent-macos-pkg-unsigned")
            && sign.contains("name: js-agent-macos-pkg-unsigned")
            && sign.contains("name: js-agent-macos-pkg-signed")
            && !build.contains("js-agent-macos-pkg-signed"),
        "the artifacts are not the unsigned one from build and the signed one from sign",
    );
    problems
}

#[test]
fn the_workflow_builds_a_script_free_hostkeeper_package_and_signs_it_with_productsign_alone() {
    assert_eq!(package_problems(&read(WORKFLOW)), Vec::<String>::new());
}

#[test]
fn each_way_of_leaving_the_hostkeeper_package_shape_is_named() {
    let text = read(WORKFLOW);
    for (from, to, says) in [
        (
            "--install-location / --ownership recommended stage/component.pkg",
            "--install-location / --scripts stage/scripts --ownership recommended stage/component.pkg",
            "the package has scripts",
        ),
        (
            "pkgbuild --root stage/root --identifier com.komira.js-agent",
            "pkgbuild --root stage/root --identifier js-agent",
            "pkgbuild does not build the component",
        ),
        (
            "\"dist/js-agent-$VERSION-macos-arm64.pkg\"\n      - name: The unsigned",
            "\"dist/js-agent-$VERSION-aarch64-apple-darwin.pkg\"\n      - name: The unsigned",
            "the package is not named js-agent-<version>-macos-arm64.pkg",
        ),
        (
            "agent-plist > inputs/agent.plist",
            "agent-plist --server SET-THE-SERVER > inputs/agent.plist",
            "the agent's plist is not the fixed one",
        ),
        (
            "--daemon-requirement \"cdhash H\\\"$cdhash\\\"\"",
            "--daemon-requirement SET-THE-DAEMON-REQUIREMENT",
            "the helper's plist does not name the agent by its cdhash",
        ),
        (
            "codesign --display --verbose=4 target/release/js-agent 2>&1",
            "codesign --display --verbose=4 target/release/js-mac-session 2>&1",
            "the cdhash is not read from the js-agent binary that is built",
        ),
        (
            "[[ \"$cdhash\" =~ ^[0-9a-f]{40}$ ]]",
            "[ -n \"$cdhash\" ]",
            "the cdhash is not checked to be 40 hex digits",
        ),
        (
            "if [ \"$staged\" != \"$cdhash\" ]",
            "if [ -z \"$staged\" ]",
            "the cdhash of the staged js-agent is not compared",
        ),
        (
            "/usr/bin/productsign --timestamp --sign",
            "/usr/bin/productsign --sign",
            "the package is not signed by productsign --timestamp",
        ),
        (
            "--sign \"$identity\"",
            "--sign \"Developer ID Installer: Someone (TEAM)\"",
            "an identity name is written",
        ),
        (
            "find-identity -v -p basic",
            "find-identity -p basic",
            "the identity is not the one Developer ID Installer certificate",
        ),
        (
            "          cargo build --locked --release -p js-vmm --bin js-vmm\n",
            "",
            "js-vmm is not built for the package",
        ),
        (
            "js-mac-provision js-vmm; do",
            "js-mac-provision; do",
            "js-vmm is not built for the package",
        ),
        (
            "      - name: Entitle js-vmm\n        run: cargo xtask pkg entitle-macos --bin-dir target/release\n",
            "",
            "js-vmm is not given its entitlement once",
        ),
        (
            "      - name: Build the package\n",
            "      - name: Entitle js-vmm late\n        run: cargo xtask pkg entitle-macos --bin-dir target/release\n      - name: Build the package\n",
            "js-vmm is not given its entitlement once",
        ),
        (
            "      - name: Build the package\n",
            "      - name: Sign js-vmm by hand\n        run: /usr/bin/codesign --force --sign - --entitlements e.plist target/release/js-vmm\n      - name: Build the package\n",
            "the workflow signs a program itself",
        ),
        (
            "      - name: Build the package\n",
            "      - name: Sign js-vmm by hand\n        run: /usr/bin/codesign -f -s - target/release/js-vmm\n      - name: Build the package\n",
            "the workflow signs a program itself",
        ),
        (
            "      - name: The signed package is the package\n",
            "      - name: Notarize\n        run: /usr/bin/xcrun notarytool submit x\n      - name: The signed package is the package\n",
            "something is notarized",
        ),
        (
            "name: js-agent-macos-pkg-signed",
            "name: js-agent-macos-pkg",
            "the artifacts are not the unsigned one from build and the signed one from sign",
        ),
    ] {
        assert!(text.contains(from), "{from:?}");
        let problems = package_problems(&text.replacen(from, to, 1));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{from:?} -> {to:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn the_sign_condition_refuses_a_pull_request_and_a_push_that_does_not_ask() {
    // A manual run counts on main alone, and the release's call counts when it asks (`sign`),
    // on main, for a push: no part of the condition is optional.
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let line = sign
        .iter()
        .find(|l| indent(l) == 4 && l.trim().starts_with("if:"))
        .expect("the sign job's condition")
        .trim();
    assert_eq!(line, SIGN_IF);
    for part in [
        "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main'",
        "inputs.sign && github.ref == 'refs/heads/main' && github.event_name == 'push'",
    ] {
        assert!(line.contains(part), "{part}");
    }
    // The condition is a disjunction of those two parts and nothing looser.
    assert_eq!(line.matches("||").count(), 1, "{line}");
    assert!(!line.contains("pull_request"));
    // The release's call is the one that sets `sign`, and the other callers do not.
    let release = std::fs::read_to_string(
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .join(".github/workflows/release.yml"),
    )
    .expect("release.yml");
    assert!(release.contains(
        "    uses: ./.github/workflows/js-agent-macos-pkg.yml\n    with:\n      version: ${{ needs.plan.outputs.version }}\n      sign: true\n"
    ));
}

/// The one entitle step moved after the staging, or before the build of js-vmm: the count stays
/// one, and the order alone is what the lint names.
#[test]
fn an_entitle_step_out_of_order_is_named() {
    let text = read(WORKFLOW);
    let step = "      - name: Entitle js-vmm\n        run: cargo xtask pkg entitle-macos --bin-dir target/release\n";
    assert!(text.contains(step));
    let without = text.replacen(step, "", 1);
    for anchor in [
        "      - name: Build the package\n",
        "      - name: Build the programs\n",
    ] {
        assert!(without.contains(anchor), "{anchor:?}");
        let moved = without.replacen(anchor, &format!("{step}{anchor}"), 1);
        assert_eq!(
            moved.matches("run: cargo xtask pkg entitle-macos").count(),
            1
        );
        assert_eq!(
            package_problems(&moved),
            ["js-vmm is not given its entitlement once, after it is built and before it is staged"],
            "{anchor:?}"
        );
    }
}
