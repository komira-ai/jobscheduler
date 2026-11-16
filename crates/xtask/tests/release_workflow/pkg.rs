//! The macOS installer package in the release: `beta-macos-pkg` builds it at the release's
//! version and has it signed, and `promote-macos` attaches the signed file, never the unsigned
//! one, under hostkeeper's name for a versioned `macos-pkg` file.

use super::{job, jobs, workflow};

/// What is wrong with the way `text` (release.yml) builds, signs and attaches the package.
fn pkg_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let mut problems = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            problems.push(what.to_owned());
        }
    };
    let call = job(&all, "beta-macos-pkg").join("\n");
    want(
        call.contains("      version: ${{ needs.plan.outputs.version }}\n"),
        "beta-macos-pkg does not build the release's version",
    );
    want(
        call.contains("      sign: true\n"),
        "beta-macos-pkg does not ask for the sign job",
    );
    // The called workflow's jobs ask for `contents: read` alone, which this covers.
    want(
        call.contains("    permissions:\n      contents: read\n    uses:"),
        "beta-macos-pkg does not grant exactly contents: read",
    );
    let promote = job(&all, "promote-macos").join("\n");
    want(
        promote.contains("          pattern: js-agent-macos-pkg\n"),
        "promote-macos does not download the signed package's artifact",
    );
    want(
        !promote.contains("js-agent-macos-pkg-unsigned"),
        "promote-macos downloads the unsigned package",
    );
    want(
        promote.contains(r#"name="js-agent-$VERSION-macos-arm64.pkg""#),
        "promote-macos does not name the package as hostkeeper's versioned file",
    );
    want(
        promote.contains(r#"attach "$name.sha256" "$name.sha256""#),
        "promote-macos does not attach the package's SHA-256 file",
    );
    // Attaching is one function, with one comparison, for the archives and the package.
    want(
        promote.matches("cmp -s").count() == 1
            && promote.matches("attach \"").count() == 3
            && promote.contains(r#"attach "$signed" "$name""#),
        "the package is not attached by the rule the archives are",
    );
    // The package is attached only inside the test for the signed file.
    let guard = promote.find(r#"if [ -f "$signed" ]; then"#);
    let attach = promote.find(r#"attach "$signed" "$name""#);
    want(
        guard.zip(attach).is_some_and(|(g, a)| g < a),
        "promote-macos attaches the package without testing that it is signed",
    );
    want(
        promote.contains("::notice::no signed macOS package"),
        "promote-macos does not say when there is no signed package",
    );
    want(
        !job(&all, "publish").join("\n").contains(".pkg"),
        "publish touches a package",
    );
    problems
}

/// Asserts the release builds the package at its own version, asks for it to be signed, and
/// attaches the signed file and its SHA-256 line the way it attaches the archives: an identical
/// asset is kept, a different one refused. Catches a package built as 0.0.0, a promotion that
/// downloads the unsigned artifact or attaches without the signed file, and a package attached
/// by a rule of its own.
#[test]
fn the_release_builds_the_package_at_its_version_and_attaches_only_the_signed_file() {
    assert_eq!(pkg_problems(&workflow()), Vec::<String>::new());
}

/// Asserts each rule of `pkg_problems` fires on a release.yml that breaks it.
#[test]
fn a_release_that_breaks_the_package_rules_is_refused() {
    let text = workflow();
    let broken = |from: &str, to: &str| {
        assert!(text.contains(from), "{from}");
        pkg_problems(&text.replacen(from, to, 1))
    };
    assert_eq!(
        broken(
            "      version: ${{ needs.plan.outputs.version }}\n      sign: true\n",
            "      sign: true\n"
        ),
        ["beta-macos-pkg does not build the release's version"]
    );
    assert_eq!(
        broken("      sign: true\n", ""),
        ["beta-macos-pkg does not ask for the sign job"]
    );
    assert_eq!(
        broken(
            "    permissions:\n      contents: read\n    uses: ./.github/workflows/js-agent-macos-pkg.yml",
            "    permissions:\n      contents: read\n      id-token: write\n    uses: ./.github/workflows/js-agent-macos-pkg.yml"
        ),
        ["beta-macos-pkg does not grant exactly contents: read"]
    );
    assert_eq!(
        broken(
            "          pattern: js-agent-macos-pkg\n",
            "          pattern: js-agent-macos-pkg-unsigned\n"
        ),
        [
            "promote-macos does not download the signed package's artifact",
            "promote-macos downloads the unsigned package"
        ]
    );
    assert_eq!(
        broken(
            r#"name="js-agent-$VERSION-macos-arm64.pkg""#,
            r#"name="js-agent-$VERSION-aarch64-apple-darwin.pkg""#
        ),
        ["promote-macos does not name the package as hostkeeper's versioned file"]
    );
    assert_eq!(
        broken(
            r#"            attach "$name.sha256" "$name.sha256"
"#,
            ""
        ),
        [
            "promote-macos does not attach the package's SHA-256 file",
            "the package is not attached by the rule the archives are"
        ]
    );
    assert_eq!(
        broken(r#"if [ -f "$signed" ]; then"#, "if true; then"),
        ["promote-macos attaches the package without testing that it is signed"]
    );
    assert_eq!(
        broken(
            "::notice::no signed macOS package",
            "::notice::nothing to attach"
        ),
        ["promote-macos does not say when there is no signed package"]
    );
}
