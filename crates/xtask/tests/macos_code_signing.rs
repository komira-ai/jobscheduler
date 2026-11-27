//! The code signing of the macOS programs of js-agent in the workflow
//! (`.github/workflows/js-agent-macos-pkg.yml`) that signs the programs before the package is
//! made of them. The table, the verb and the profile are in code_signature_verb.rs.
//!
//! The workflow is read as it is in the repository. `build` builds the program and holds
//! no secret. `sign-code` runs under the signing condition (macos_pkg_workflow/signing_run.rs) in
//! the environment `pkg-signing`, signs them with the team's Developer ID Application identity,
//! and nothing built from the repository runs in it; macos_sign_code_run.rs runs its signing
//! step. `package`
//! holds no secret and makes the package of the signed programs after checking each one. `sign`
//! (macos_pkg_workflow.rs) signs that package.

use std::collections::BTreeMap;
use std::path::Path;

use xtask::release::mac_programs::{APPLICATION_IDENTITY, PROGRAMS, TEAM_ID};

const WORKFLOW: &str = ".github/workflows/js-agent-macos-pkg.yml";
const ACTION: &str = ".github/actions/js-agent-macos-pkg/action.yml";
const README: &str = "packaging/js-agent-macos/README.md";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The lines of a file without comments.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

/// The jobs of the workflow: name and the lines of the block.
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
            if indent(line) == 2 && line.trim_end().ends_with(':') {
                let name = line.trim().trim_end_matches(':').to_owned();
                found.insert(name.clone(), Vec::new());
                current = Some(name);
            } else if let Some(lines) = current.as_ref().and_then(|n| found.get_mut(n)) {
                lines.push(line);
            }
        }
    }
    found
}

/// The steps of a job: name (or the `uses:` line) and lines.
fn steps<'a>(job: &[&'a str]) -> Vec<(String, Vec<&'a str>)> {
    let mut found: Vec<(String, Vec<&str>)> = Vec::new();
    for line in job {
        let t = line.trim_start();
        if indent(line) == 6 && t.starts_with("- ") {
            found.push((
                t.trim_start_matches("- name: ").trim().to_owned(),
                vec![*line],
            ));
        } else if let Some((_, lines)) = found.last_mut() {
            lines.push(line);
        }
    }
    found
}

fn job_text(text: &str, name: &str) -> String {
    jobs(text).get(name).map_or(String::new(), |j| j.join("\n"))
}

/// The signing condition of the jobs that sign: the release's call that asks, on a push to main,
/// or a signing run on a branch of this repository (macos_pkg_workflow/signing_run.rs).
const SIGN_COND: &str = "(github.event_name == 'push' && github.ref == 'refs/heads/main' && inputs.sign) || (github.event_name == 'workflow_dispatch' && inputs.sign && github.repository == 'komira-ai/jobscheduler' && startsWith(github.ref, 'refs/heads/'))";

/// The `if:` of `sign-code`: not cancelled, the unsigned programs built by `build` or by `farm`
/// (whichever ran), and the signing condition.
fn sign_code_if() -> String {
    format!(
        "if: ${{{{ !cancelled() && (needs.build.result == 'success' || needs.farm.result == 'success') && ({SIGN_COND}) }}}}"
    )
}

/// The `if:` of `package`: not cancelled, `sign-code` succeeded, and the signing condition.
fn package_if() -> String {
    format!("if: ${{{{ !cancelled() && needs.sign-code.result == 'success' && ({SIGN_COND}) }}}}")
}

/// What `security import` answers on a hosted runner that holds Apple's intermediate already,
/// in the step's environment.
const ALREADY_EXISTS: &str = "ALREADY_EXISTS: \"security: SecKeychainItemImport: The specified item already exists in the keychain.\"";

/// The import of the intermediate: its answer kept and printed, and a failure passed only when
/// the answer is exactly [`ALREADY_EXISTS`] (macos_sign_code_run.rs runs it).
const IMPORT: &str = r#"          imported=true
          /usr/bin/security import "$RUNNER_TEMP/DeveloperIDG2CA.cer" -t cert -f x509 -k "$keychain" > "$RUNNER_TEMP/import-intermediate.txt" 2>&1 || imported=false
          /bin/cat "$RUNNER_TEMP/import-intermediate.txt"
          if [ "$imported" = false ]; then
            if ! printf '%s\n' "$ALREADY_EXISTS" | /usr/bin/cmp -s - "$RUNNER_TEMP/import-intermediate.txt"; then
              echo "::error::the import of Apple's intermediate failed with the answer above"
              exit 1
            fi
            echo "the runner holds Apple's intermediate already"
          fi
          identity="$(/usr/bin/security find-identity -v -p codesigning "$keychain" "#;

// ---- the workflow ----

/// One line per way the workflow `text` and the package action `action` are not the signing of
/// the programs before the package: the signing job holds the only application secrets, signs
/// the program under the table's identifiers with the hardened runtime and a timestamp,
/// runs nothing built from the repository, and the package job holds no secret and checks each
/// staged program.
fn signing_problems(text: &str, action: &str) -> Vec<String> {
    let all = jobs(text);
    let sign_code = all.get("sign-code").cloned().unwrap_or_default();
    let sign_text = sign_code.join("\n");
    let package = job_text(text, "package");
    let build = job_text(text, "build");
    let action_code = code(action).join("\n");
    let mut problems = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            problems.push(what.to_owned());
        }
    };
    let at_four = |job: &[&str]| -> Vec<String> {
        job.iter()
            .filter(|l| indent(l) == 4)
            .map(|l| l.trim().to_owned())
            .collect()
    };
    let signing_four = at_four(&sign_code);
    let package_four = at_four(all.get("package").map_or(&[][..], Vec::as_slice));
    want(
        signing_four.iter().any(|l| *l == sign_code_if())
            && package_four.iter().any(|l| *l == package_if()),
        "sign-code and package do not run under the signing condition alone",
    );
    want(
        signing_four.iter().any(|l| l == "environment: pkg-signing"),
        "sign-code does not run in the environment pkg-signing",
    );
    want(
        !package.contains("environment:") && !build.contains("environment:"),
        "package or build runs in an environment",
    );
    want(
        signing_four.iter().any(|l| l == "needs: [build, farm]")
            && package_four
                .iter()
                .any(|l| l == "needs: [build, farm, sign-code]"),
        "sign-code does not wait for build and farm, or package for build, farm and sign-code",
    );
    for word in [
        "actions/checkout",
        "cargo",
        "rustup",
        "uses: ./",
        "xtask",
        "chmod",
    ] {
        want(
            !sign_text.contains(word),
            &format!("sign-code runs something built from the repository: {word}"),
        );
    }
    // Secrets: named in the environment of the two steps that use them, nowhere else.
    let mut named = Vec::new();
    for (step, lines) in steps(&sign_code) {
        for line in lines.iter().filter(|l| l.contains("secrets.")) {
            let ok = indent(line) == 10
                && line.trim().starts_with("MACOS_APPLICATION_P12")
                && (step == "Check the signing secrets" || step == "Sign the programs");
            want(
                ok,
                &format!("a secret is named outside the env of the step that uses it: {line}"),
            );
            named.push(line.trim().to_owned());
        }
    }
    want(
        named
            == [
                "MACOS_APPLICATION_P12: ${{ secrets.MACOS_APPLICATION_P12 }}",
                "MACOS_APPLICATION_P12_PASSWORD: ${{ secrets.MACOS_APPLICATION_P12_PASSWORD }}",
                "MACOS_APPLICATION_P12: ${{ secrets.MACOS_APPLICATION_P12 }}",
                "MACOS_APPLICATION_P12_PASSWORD: ${{ secrets.MACOS_APPLICATION_P12_PASSWORD }}",
            ],
        "sign-code does not name MACOS_APPLICATION_P12 and its password in two steps",
    );
    want(
        !package.contains("secrets")
            && !build.contains("secrets")
            && !action_code.contains("secret"),
        "package, build or the action names a secret",
    );
    want(
        sign_text.contains("is not set in the pkg-signing environment")
            && sign_text.contains("exit 1"),
        "sign-code does not fail by name when a secret is not set",
    );
    // The system tools are called by absolute path.
    let mut absolute = 0;
    for line in sign_code
        .iter()
        .filter(|l| !l.trim_start().starts_with("name:"))
    {
        for token in line.split(|c: char| c.is_whitespace() || "();|&\"'".contains(c)) {
            if [
                "security", "codesign", "curl", "shasum", "openssl", "base64", "uuidgen", "awk",
                "cat", "cmp",
            ]
            .contains(&token)
            {
                want(false, &format!("sign-code calls {token} by name"));
            }
            if token.starts_with("/usr/bin/")
                || token.starts_with("/usr/sbin/")
                || token.starts_with("/bin/")
            {
                absolute += 1;
            }
        }
    }
    want(
        absolute >= 12,
        "sign-code calls too few system tools by absolute path",
    );
    want(
        sign_text.contains("-T /usr/bin/codesign") && !sign_text.contains(" -A"),
        "the key is not imported for /usr/bin/codesign alone",
    );
    want(
        sign_text.contains("trap cleanup EXIT")
            && sign_text.contains("/usr/bin/security delete-keychain \"$keychain\"")
            && sign_text.contains("rm -f \"$RUNNER_TEMP/application.p12\""),
        "the keychain and the key file are not deleted whatever happens",
    );
    want(
        sign_text.contains("INTERMEDIATE_SHA256: f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a")
            && sign_text.contains("--proto '=https'")
            && sign_text.contains("echo \"$INTERMEDIATE_SHA256  $RUNNER_TEMP/DeveloperIDG2CA.cer\" | /usr/bin/shasum -a 256 --check"),
        "Apple's intermediate is not fetched over HTTPS and held to its SHA-256",
    );
    want(
        sign_text.contains(
            "/usr/bin/security list-keychains -d user -s \"$keychain\"\n          /usr/bin/curl ",
        ),
        "the job's keychain is not made the user's only keychain before the lookups by name",
    );
    want(
        sign_text.contains(&format!("          {ALREADY_EXISTS}\n"))
            && sign_text.contains(IMPORT)
            && sign_text.matches("security import").count() == 2,
        "the import of Apple's intermediate does not pass the runner's duplicate answer alone",
    );
    want(
        sign_text.contains(&format!("IDENTITY: \"{APPLICATION_IDENTITY}\""))
            && sign_text.matches("Developer ID").count() == 1,
        "the identity is not the table's, written once",
    );
    want(
        sign_text.contains("/usr/bin/openssl x509 -noout -checkend 2592000"),
        "the job does not fail 30 days before the identity expires",
    );
    want(
        sign_text.contains("[[ \"$identity\" =~ ^[0-9A-F]{40}$ ]]")
            && sign_text.contains("find-identity -v -p codesigning \"$keychain\""),
        "the identity is not the one valid codesigning identity of the keychain, by its SHA-1",
    );
    want(
        sign_text.contains("/usr/bin/codesign --force --sign \"$identity\" --keychain \"$keychain\" --identifier \"$identifier\" --options runtime --timestamp \"programs/$program\""),
        "the programs are not signed with the identity, the identifier, the hardened runtime and a timestamp",
    );
    let table: Vec<String> = PROGRAMS
        .iter()
        .map(|p| format!("          {} {}", p.name, p.identifier))
        .collect();
    want(
        sign_text.contains(&format!("<<'TABLE'\n{}\n          TABLE", table.join("\n"))),
        "the programs and identifiers signed are not the table's",
    );
    want(
        sign_text.contains("/usr/bin/shasum -a 256 --check PROGRAMS.SHA256SUMS")
            && sign_text
                .contains("/usr/bin/codesign --verify --strict --verbose=2 \"programs/$program\""),
        "the programs are not checked against their sums before and verified after signing",
    );
    want(
        sign_text.contains("name: js-agent-macos-programs-unsigned")
            && sign_text.contains("name: js-agent-macos-programs-signed")
            && build.contains("name: js-agent-macos-programs-unsigned")
            && build.contains("target/release/PROGRAMS.SHA256SUMS")
            && build.contains("shasum -a 256 js-agent > PROGRAMS.SHA256SUMS"),
        "the programs do not go build -> sign-code -> package as the two artifacts",
    );
    want(
        package.contains("name: js-agent-macos-programs-signed")
            && package.contains("path: signed-programs")
            && package.contains("          programs: signed-programs\n")
            && !package.contains("js-agent-macos-programs-unsigned"),
        "package does not make the package of the signed programs",
    );
    // The action.
    let step_at = |name: &str| action_code.find(&format!("    - name: {name}\n"));
    let check = step_at("The staged programs are signed as the release signs them");
    let stage = step_at("Stage the payload");
    let make = step_at("Build the package");
    want(
        matches!((stage, check, make), (Some(s), Some(c), Some(m)) if s < c && c < m),
        "the action does not check the staged programs after staging and before the package is built",
    );
    want(
        action_code.contains("for bin in js-agent; do\n          cargo xtask release check-code-signature \"stage/root/Library/Application Support/js-agent/bin/$bin\" --program \"$bin\"\n        done"),
        "the action does not check each staged program against the table",
    );
    want(
        action_code
            .matches("if: ${{ inputs.programs != '' }}")
            .count()
            == 2
            && action_code
                .matches("if: ${{ inputs.programs == '' }}")
                .count()
                == 1,
        "the action builds when it is given programs, or checks when it is not",
    );
    want(
        action_code.contains("cp \"$PROGRAMS/$bin\" \"target/release/$bin\""),
        "the action does not take the programs from its input",
    );
    problems
}

/// The workflow and the action sign the programs before the package is made.
#[test]
fn the_programs_are_signed_in_a_keyed_job_and_packaged_in_a_keyless_one() {
    assert_eq!(
        signing_problems(&read(WORKFLOW), &read(ACTION)),
        Vec::<String>::new()
    );
}

/// Each way of leaving that shape is named.
///
/// Catches a lint that no longer sees a signing job that runs on a pull request, names a secret
/// in the wrong step, imports the key for every tool, skips the intermediate's digest, writes
/// another identity, signs without the hardened runtime or the timestamp, or packages unchecked
/// programs.
#[test]
fn each_way_of_leaving_the_signing_shape_is_named() {
    let text = read(WORKFLOW);
    let action = read(ACTION);
    let sign_code_head = format!(
        "  sign-code:\n    needs: [build, farm]\n    {}\n",
        sign_code_if()
    );
    let sign_code_other =
        "  sign-code:\n    needs: [build, farm]\n    if: ${{ github.event_name != 'x' }}\n";
    let package_head = format!(
        "  package:\n    needs: [build, farm, sign-code]\n    {}\n    runs-on: macos-15\n",
        package_if()
    );
    let package_environment = format!(
        "  package:\n    needs: [build, farm, sign-code]\n    {}\n    environment: pkg-signing\n    runs-on: macos-15\n",
        package_if()
    );
    let cases: [(&str, &str, &str); 29] = [
        (
            "          /usr/bin/security list-keychains -d user -s \"$keychain\"\n",
            "",
            "not made the user's only keychain",
        ),
        (
            // The import as it was on the first signing run on main: the duplicate fails the job.
            "          imported=true\n          /usr/bin/security import \"$RUNNER_TEMP/DeveloperIDG2CA.cer\" -t cert -f x509 -k \"$keychain\" > \"$RUNNER_TEMP/import-intermediate.txt\" 2>&1 || imported=false\n",
            "          /usr/bin/security import \"$RUNNER_TEMP/DeveloperIDG2CA.cer\" -t cert -f x509 -k \"$keychain\"\n          imported=true\n",
            "does not pass the runner's duplicate answer alone",
        ),
        (
            "The specified item already exists in the keychain.\"\n",
            "already exists\"\n",
            "does not pass the runner's duplicate answer alone",
        ),
        (
            "| /usr/bin/cmp -s - \"$RUNNER_TEMP/import-intermediate.txt\"",
            "| /usr/bin/grep -q -F -f - \"$RUNNER_TEMP/import-intermediate.txt\"",
            "does not pass the runner's duplicate answer alone",
        ),
        (
            "              echo \"::error::the import of Apple's intermediate failed with the answer above\"\n              exit 1\n",
            "              echo \"::error::the import of Apple's intermediate failed with the answer above\"\n",
            "does not pass the runner's duplicate answer alone",
        ),
        (
            "          /bin/cat \"$RUNNER_TEMP/import-intermediate.txt\"\n",
            "          cat \"$RUNNER_TEMP/import-intermediate.txt\"\n",
            "calls cat by name",
        ),
        (
            &sign_code_head,
            sign_code_other,
            "do not run under the signing condition alone",
        ),
        (
            "    environment: pkg-signing\n    runs-on: macos-15\n    timeout-minutes: 60\n    permissions:\n      contents: read\n    steps:\n      - name: Check the signing secrets\n        env:\n          MACOS_APPLICATION_P12:",
            "    runs-on: macos-15\n    timeout-minutes: 60\n    permissions:\n      contents: read\n    steps:\n      - name: Check the signing secrets\n        env:\n          MACOS_APPLICATION_P12:",
            "does not run in the environment pkg-signing",
        ),
        (
            "  package:\n    needs: [build, farm, sign-code]\n",
            "  package:\n    needs: [build, farm]\n",
            "does not wait for build",
        ),
        (
            "      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: js-agent-macos-programs-unsigned\n",
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: js-agent-macos-programs-unsigned\n",
            "runs something built from the repository: actions/checkout",
        ),
        (
            "          /usr/bin/shasum -a 256 --check PROGRAMS.SHA256SUMS\n",
            "          shasum -a 256 --check PROGRAMS.SHA256SUMS\n",
            "calls shasum by name",
        ),
        (
            "      - name: The signed programs verify\n        run: |\n          set -euo pipefail\n",
            "      - name: The signed programs verify\n        env:\n          MACOS_APPLICATION_P12: ${{ secrets.MACOS_APPLICATION_P12 }}\n        run: |\n          set -euo pipefail\n",
            "a secret is named outside the env of the step that uses it",
        ),
        (
            &package_head,
            &package_environment,
            "package or build runs in an environment",
        ),
        (
            "          version: ${{ env.VERSION }}\n          programs: signed-programs\n",
            "          version: ${{ env.VERSION }}\n          programs: signed-programs\n        env:\n          X: ${{ secrets.MACOS_APPLICATION_P12 }}\n",
            "package, build or the action names a secret",
        ),
        (
            "is not set in the pkg-signing environment",
            "is empty",
            "does not fail by name",
        ),
        (
            "-T /usr/bin/codesign",
            "-A",
            "not imported for /usr/bin/codesign alone",
        ),
        (
            "rm -f \"$RUNNER_TEMP/application.p12\" \"$RUNNER_TEMP/DeveloperIDG2CA.cer\"",
            "true",
            "not deleted whatever happens",
        ),
        (
            "f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "not fetched over HTTPS and held to its SHA-256",
        ),
        (
            "--proto '=https' ",
            "",
            "not fetched over HTTPS and held to its SHA-256",
        ),
        (
            "IDENTITY: \"Developer ID Application: Komira AI, Inc. (U7RTX4XD3X)\"",
            "IDENTITY: \"Developer ID Application: Someone Else (ABCDE12345)\"",
            "the identity is not the table's",
        ),
        (
            "-checkend 2592000",
            "-checkend 0",
            "30 days before the identity expires",
        ),
        (
            "find-identity -v -p codesigning",
            "find-identity -p codesigning",
            "not the one valid codesigning identity",
        ),
        (
            "--options runtime --timestamp \"programs/$program\"",
            "--timestamp \"programs/$program\"",
            "not signed with the identity, the identifier, the hardened runtime and a timestamp",
        ),
        (
            "--options runtime --timestamp \"programs/$program\"",
            "--options runtime \"programs/$program\"",
            "not signed with the identity, the identifier, the hardened runtime and a timestamp",
        ),
        (
            "          js-agent com.komira.js-agent\n",
            "          js-agent com.komira.js-agent.session\n",
            "the programs and identifiers signed are not the table's",
        ),
        (
            "name: js-agent-macos-programs-signed\n          path: |",
            "name: js-agent-macos-programs-other\n          path: |",
            "do not go build -> sign-code -> package",
        ),
        (
            "          programs: signed-programs\n",
            "",
            "package does not make the package of the signed programs",
        ),
        (
            "            /usr/bin/codesign --verify --strict --verbose=2 \"programs/$program\"\n",
            "            echo \"$program\"\n",
            "not checked against their sums before and verified after signing",
        ),
        (
            "          done <<'TABLE'\n",
            "          done <<'TABLE'\n          js-vmm com.komira.js-vmm\n",
            "the programs and identifiers signed are not the table's",
        ),
    ];
    for (from, to, says) in cases {
        assert_eq!(text.matches(from).count(), 1, "{from:?}");
        let problems = signing_problems(&text.replacen(from, to, 1), &action);
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{from:?} -> {to:?} must say {says:?}: {problems:?}"
        );
    }
    let on_action = [
        (
            "    - name: The staged programs are signed as the release signs them\n",
            "    - name: The staged programs are looked at\n",
            "does not check the staged programs after staging",
        ),
        (
            "--program \"$bin\"\n        done",
            "--program js-agent\n        done",
            "does not check each staged program",
        ),
        (
            "      if: ${{ inputs.programs == '' }}\n",
            "",
            "builds when it is given programs",
        ),
        (
            "cp \"$PROGRAMS/$bin\" \"target/release/$bin\"",
            "cp \"$bin\" \"target/release/$bin\"",
            "does not take the programs from its input",
        ),
    ];
    for (from, to, says) in on_action {
        assert_eq!(action.matches(from).count(), 1, "{from:?}");
        let problems = signing_problems(&text, &action.replacen(from, to, 1));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{from:?} -> {to:?} must say {says:?}: {problems:?}"
        );
    }
}

/// The package that `sign` signs is the one made of the signed programs, and `sign` takes the
/// checker from `build`.
///
/// Catches a `sign` that signs the unsigned package of `build`, which would release programs
/// with no signature of their own.
#[test]
fn the_installer_signature_is_put_on_the_package_made_of_the_signed_programs() {
    let sign = job_text(&read(WORKFLOW), "sign");
    assert!(
        sign.contains("name: js-agent-macos-pkg-codesigned"),
        "{sign}"
    );
    assert!(!sign.contains("js-agent-macos-pkg-unsigned"), "{sign}");
    assert!(sign.contains("name: js-agent-macos-checker"), "{sign}");
    assert!(sign.contains("needs: [build, farm, package]"), "{sign}");
}

/// The README names the secrets the signing job reads and the profile the owner installs.
///
/// Catches documentation that leaves the owner without the names of the secrets or the profile.
#[test]
fn the_readme_names_the_application_secrets_the_profile_and_the_verb() {
    let readme = read(README);
    for needle in [
        "MACOS_APPLICATION_P12",
        "MACOS_APPLICATION_P12_PASSWORD",
        "js-agent-full-disk-access.mobileconfig",
        "check-code-signature",
        TEAM_ID,
    ] {
        assert!(readme.contains(needle), "{needle}");
    }
}
