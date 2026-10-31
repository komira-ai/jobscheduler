//! The workflow that builds the macOS package (`.github/workflows/js-agent-macos-pkg.yml`)
//! read as it is in the repository: it starts only on a pull request and by hand, holds
//! no write scope, publishes nothing, writes SHA256SUMS, and signs and notarizes only
//! when the secrets are present, naming each secret once at the job and never in a command.

use std::collections::BTreeSet;
use std::path::Path;

const WORKFLOW: &str = ".github/workflows/js-agent-macos-pkg.yml";
const README: &str = "packaging/js-agent-macos/README.md";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The steps of the workflow: the lines from one `- name:` to the next.
fn steps(text: &str) -> Vec<(String, Vec<&str>)> {
    let mut found: Vec<(String, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.trim_start().strip_prefix("- name: ") {
            found.push((name.trim().to_owned(), vec![line]));
        } else if let Some((_, lines)) = found.last_mut() {
            lines.push(line);
        }
    }
    found
}

fn secrets(text: &str) -> BTreeSet<String> {
    text.lines()
        .flat_map(|l| l.split("secrets.").skip(1))
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_uppercase() || *c == '_' || c.is_ascii_digit())
                .collect()
        })
        .collect()
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
    assert!(!text.contains("write"), "a write scope");
    assert!(!text.contains("id-token"));
    for word in [
        "gh release",
        "releases",
        "upload-release",
        "aws ",
        "gsutil",
        "gh api",
    ] {
        assert!(!text.contains(word), "{word}");
    }
}

#[test]
fn every_action_is_pinned_to_a_commit_and_the_job_runs_on_macos_15() {
    let text = read(WORKFLOW);
    for line in text.lines().filter(|l| l.contains("uses:")) {
        let pin = line.split('@').nth(1).unwrap_or("");
        assert!(
            pin.len() >= 40 && pin[..40].chars().all(|c| c.is_ascii_hexdigit()),
            "{line}"
        );
    }
    assert!(text.lines().any(|l| l.trim() == "runs-on: macos-15"));
    assert!(
        text.lines()
            .any(|l| l.contains("persist-credentials: false"))
    );
}

#[test]
fn the_package_is_built_checked_before_and_after_signing_and_summed() {
    let text = read(WORKFLOW);
    for needle in [
        "cargo xtask pkg stage-macos",
        "pkgbuild ",
        "productbuild ",
        "SHA256SUMS",
        "shasum -a 256 --check",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    let checks = text.matches("cargo xtask pkg check-macos").count();
    assert_eq!(
        checks, 2,
        "the unsigned and the final package are each checked"
    );
}

#[test]
fn signing_and_notarizing_run_only_when_their_secrets_are_present() {
    let text = read(WORKFLOW);
    let all = steps(&text);
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
        assert!(
            cond.contains("env.MACOS_INSTALLER_CERT_P12 != ''"),
            "{name}: {cond}"
        );
        if name.starts_with("Notarize") {
            assert!(
                cond.contains("env.MACOS_NOTARY_KEY_P8 != ''"),
                "{name}: {cond}"
            );
        }
    }
}

#[test]
fn a_secret_is_named_only_in_the_jobs_environment_and_in_the_readme() {
    let text = read(WORKFLOW);
    for line in text.lines().filter(|l| l.contains("secrets.")) {
        let t = line.trim();
        let (name, value) = t.split_once(": ").unwrap_or_else(|| panic!("{line}"));
        assert!(
            name.chars().all(|c| c.is_ascii_uppercase() || c == '_'),
            "{line}"
        );
        assert!(
            value.starts_with("${{ secrets.") && value.ends_with("}}"),
            "{line}"
        );
        assert!(
            line.starts_with("      "),
            "a secret outside the job's env: {line}"
        );
    }
    let named = secrets(&text);
    assert_eq!(
        named,
        [
            "MACOS_INSTALLER_CERT_P12",
            "MACOS_INSTALLER_CERT_PASSWORD",
            "MACOS_INSTALLER_IDENTITY",
            "MACOS_NOTARY_ISSUER_ID",
            "MACOS_NOTARY_KEY_ID",
            "MACOS_NOTARY_KEY_P8",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    let readme = read(README);
    for name in &named {
        assert!(
            readme.contains(name.as_str()),
            "{name} is not in the README"
        );
    }
}

#[test]
fn no_command_expands_an_expression() {
    // Every `${{ }}` is in an `env:` or `with:` value, never inside a script.
    let text = read(WORKFLOW);
    let mut in_script = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("run: |") {
            in_script = true;
            continue;
        }
        if in_script && !line.starts_with("          ") {
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
