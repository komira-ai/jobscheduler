//! The workflow that builds the macOS package (`.github/workflows/js-agent-macos-pkg.yml`)
//! read as it is in the repository. Two jobs: `build` runs on pull requests and by hand,
//! holds no secret and signs nothing; `sign` runs only by hand from main, in the protected
//! environment `macos-signing`, takes the checked unsigned package from `build` without
//! rebuilding, and is the only place a secret is named, on the steps that sign and
//! notarize and nowhere else. Neither job publishes anything.

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

#[test]
fn the_workflow_starts_on_a_pull_request_and_by_hand_only() {
    let text = read(WORKFLOW);
    let on: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "on:")
        .skip(1)
        .take_while(|l| l.starts_with("  ") || l.starts_with('#') || l.is_empty())
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .map(str::trim)
        .collect();
    assert_eq!(on, ["pull_request:", "workflow_dispatch:"]);
    assert!(!text.contains("pull_request_target"));
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

#[test]
fn the_pull_request_job_names_no_secret_and_signs_nothing() {
    let text = read(WORKFLOW);
    let build = job_text(&text, "build");
    assert!(!build.contains("secret"), "the build job mentions a secret");
    for word in [
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
                step == "Check the signing secrets"
                    || step.starts_with("Sign")
                    || step.starts_with("Notarize"),
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
    let want: BTreeSet<String> = [
        "MACOS_INSTALLER_CERT_P12",
        "MACOS_INSTALLER_CERT_PASSWORD",
        "MACOS_INSTALLER_IDENTITY",
        "MACOS_NOTARY_ISSUER_ID",
        "MACOS_NOTARY_KEY_ID",
        "MACOS_NOTARY_KEY_P8",
    ]
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
fn the_sign_job_runs_only_by_hand_from_main_in_the_protected_environment() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let at_four: Vec<&str> = sign
        .iter()
        .filter(|l| indent(l) == 4)
        .map(|l| l.trim())
        .collect();
    assert!(
        at_four.contains(
            &"if: ${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}"
        ),
        "{at_four:?}"
    );
    assert!(
        at_four.contains(&"environment: macos-signing"),
        "{at_four:?}"
    );
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

#[test]
fn signing_and_notarizing_are_steps_gated_on_the_presence_check() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let all = steps(&sign);
    let gated: Vec<_> = all
        .iter()
        .filter(|(name, _)| name.starts_with("Sign") || name.starts_with("Notarize"))
        .collect();
    assert_eq!(
        gated.len(),
        2,
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
    assert!(
        steps(&build)
            .iter()
            .all(|(n, _)| !n.starts_with("Sign") && !n.starts_with("Notarize"))
    );
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
    assert_eq!(body.matches("secrets.").count(), 6, "{body}");
    assert!(body.contains("state=present"), "{body}");
    assert!(body.contains("state=absent"), "{body}");
    assert!(body.contains("::notice::"), "{body}");
    assert!(body.contains("macos-signing"), "{body}");
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
        "once, after signing and notarizing"
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
fn the_checker_runs_in_the_sign_job_only_after_signing_and_notarizing() {
    let text = read(WORKFLOW);
    let sign = jobs(&text)["sign"].clone();
    let all = steps(&sign);
    let at = |prefix: &str| all.iter().position(|(n, _)| n.starts_with(prefix)).unwrap();
    let notarize = at("Notarize");
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
