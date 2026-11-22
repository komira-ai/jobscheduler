//! How the update package of the first install is built, read as the files are in the repository:
//! the package workflow's job `build-update` builds, with the local action
//! `.github/actions/js-agent-macos-pkg` (the one `build` calls), the package of the next patch
//! version, which the local action `.github/actions/js-agent-next-version` names, and uploads it as
//! an artifact of its own. The version is computed in that action alone, and the first-install
//! workflow calls the same action, so the two cannot differ (the action is one `cargo xtask release
//! next-version` command, which checks the version and adds one to its patch). The update package is never signed
//! and never reaches a release. The steps of both actions are one command each that reads its
//! environment and runs under bash.

use std::path::Path;

const WORKFLOW: &str = ".github/workflows/js-agent-macos-pkg.yml";
const PACKAGE_ACTION: &str = ".github/actions/js-agent-macos-pkg/action.yml";
const NEXT_ACTION: &str = ".github/actions/js-agent-next-version/action.yml";
const RELEASE: &str = ".github/workflows/release.yml";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of `text` without comments.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The text of the job `name`: the lines below its key at two spaces.
fn job(text: &str, name: &str) -> String {
    let key = format!("  {name}:");
    let lines = code(text);
    let Some(at) = lines.iter().position(|l| *l == key) else {
        return String::new();
    };
    let mut found = lines[at + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || indent(l) > 2)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    found.push('\n');
    found
}

/// One line per way the package workflow `text` does not build the update package as described
/// above, given the texts of the two actions.
fn problems(text: &str, package: &str, next: &str, release: &str) -> Vec<String> {
    let update = job(text, "build-update");
    let build = job(text, "build");
    let sign = job(text, "sign");
    let mut found = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            found.push(what.to_owned());
        }
    };
    want(
        update.contains("    runs-on: macos-15\n"),
        "build-update does not run on macos-15",
    );
    want(
        update.contains(
            "        id: next\n        uses: ./.github/actions/js-agent-next-version\n        with:\n          version: ${{ env.VERSION }}\n",
        ),
        "build-update does not name its version with js-agent-next-version from the release's version",
    );
    want(
        update.contains(
            "        uses: ./.github/actions/js-agent-macos-pkg\n        with:\n          version: ${{ steps.next.outputs.version }}\n",
        ),
        "build-update does not build the package at the version js-agent-next-version gave",
    );
    want(
        build.contains(
            "        uses: ./.github/actions/js-agent-macos-pkg\n        with:\n          version: ${{ env.VERSION }}\n",
        ),
        "build does not build the package at the release's version with the package action",
    );
    want(
        update.contains(
            "          name: js-agent-macos-pkg-update-unsigned\n          path: dist/\n",
        ) && update.contains("          retention-days: 1\n"),
        "build-update does not upload dist/ as js-agent-macos-pkg-update-unsigned for a day",
    );
    want(
        !build.contains("update") && !sign.contains("update"),
        "the update package reaches build or sign",
    );
    want(
        !release.contains("js-agent-macos-pkg-update-unsigned")
            && !release.contains("build-update"),
        "release.yml names the update package",
    );
    want(
        !text.contains("+ 1") && !text.contains("BASH_REMATCH"),
        "the workflow computes a version of its own",
    );
    want(
        code(next).iter().any(|l| {
            l.trim()
                == r#"run: cargo xtask release next-version --version "$VERSION" --output "$GITHUB_OUTPUT""#
        }) && !next.contains("BASH_REMATCH")
            && !next.contains("+ 1"),
        "js-agent-next-version does not take the next patch version from the xtask command that checks it",
    );
    want(
        !package.contains("BASH_REMATCH"),
        "the package action computes a version",
    );
    found
}

/// One line per `${{` inside a command of `text`: a `run:` of one line, or the lines of a `run: |`
/// block (those indented more than the `run:` key).
fn expressions_in_commands(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut block: Option<usize> = None;
    for line in code(text) {
        if let Some(base) = block {
            if line.trim().is_empty() || indent(line) > base {
                if line.contains("${{") {
                    found.push(line.trim().to_owned());
                }
                continue;
            }
            block = None;
        }
        let Some(rest) = line.trim_start().strip_prefix("run: ") else {
            continue;
        };
        if rest.trim() == "|" {
            block = Some(indent(line));
        } else if rest.contains("${{") {
            found.push(line.trim().to_owned());
        }
    }
    found
}

/// The real files build the update package at the next patch version, in one place, and keep it
/// out of signing and out of the release.
#[test]
fn the_update_package_is_built_at_the_next_patch_version_named_in_one_place_and_never_released() {
    assert_eq!(
        problems(
            &read(WORKFLOW),
            &read(PACKAGE_ACTION),
            &read(NEXT_ACTION),
            &read(RELEASE)
        ),
        Vec::<String>::new()
    );
}

/// Each way the update build can leave that shape is named.
#[test]
fn each_way_of_leaving_the_update_build_shape_is_named() {
    let text = read(WORKFLOW);
    let package = read(PACKAGE_ACTION);
    let next = read(NEXT_ACTION);
    let release = read(RELEASE);
    let on_workflow: [(&str, &str, &str); 7] = [
        (
            "  build-update:\n    name: js-agent-macos-pkg-update\n    runs-on: macos-15\n",
            "  build-update:\n    name: js-agent-macos-pkg-update\n    runs-on: macos-14\n",
            "build-update does not run on macos-15",
        ),
        (
            "        id: next\n        uses: ./.github/actions/js-agent-next-version\n",
            "        id: next\n        uses: ./.github/actions/js-agent-macos-pkg\n",
            "build-update does not name its version",
        ),
        (
            "          version: ${{ steps.next.outputs.version }}\n",
            "          version: ${{ env.VERSION }}\n",
            "build-update does not build the package at the version js-agent-next-version gave",
        ),
        (
            "          name: js-agent-macos-pkg-update-unsigned\n          path: dist/\n          if-no-files-found: error\n          retention-days: 1\n",
            "          name: js-agent-macos-pkg-update-unsigned\n          path: dist/\n          if-no-files-found: error\n          retention-days: 7\n",
            "build-update does not upload dist/ as js-agent-macos-pkg-update-unsigned for a day",
        ),
        (
            "  sign:\n    needs: [build, package]\n",
            "  sign:\n    needs: [build, package, build-update]\n",
            "the update package reaches build or sign",
        ),
        (
            "          version: ${{ env.VERSION }}\n      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: js-agent-macos-pkg-unsigned\n",
            "          version: 0.0.0\n      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: js-agent-macos-pkg-unsigned\n",
            "build does not build the package at the release's version",
        ),
        (
            "      - name: Name the next version\n",
            "      - name: Name the next version\n        run: echo \"v=$((1 + 1))\"\n",
            "the workflow computes a version of its own",
        ),
    ];
    for (from, to, says) in on_workflow {
        assert_eq!(text.matches(from).count(), 1, "{from:?}");
        let found = problems(&text.replacen(from, to, 1), &package, &next, &release);
        assert!(
            found.iter().any(|p| p.contains(says)),
            "{from:?} -> {to:?} must say {says:?}: {found:?}"
        );
    }
    let on_next = [
        (
            "run: cargo xtask release next-version",
            "run: cargo xtask release stamp",
            "does not take the next patch version from the xtask command",
        ),
        (
            "--version \"$VERSION\"",
            "--version 1.0.0",
            "does not take the next patch version from the xtask command",
        ),
        (
            "      run: cargo",
            "      run: BASH_REMATCH=1 cargo",
            "does not take the next patch version from the xtask command",
        ),
    ];
    for (from, to, says) in on_next {
        assert_eq!(next.matches(from).count(), 1, "{from:?}");
        let found = problems(&text, &package, &next.replacen(from, to, 1), &release);
        assert!(
            found.iter().any(|p| p.contains(says)),
            "{from:?} -> {to:?} must say {says:?}: {found:?}"
        );
    }
    let found = problems(
        &text,
        &format!("{package}\n# BASH_REMATCH\n{next}"),
        &next,
        &release,
    );
    assert!(
        found
            .iter()
            .any(|p| p.contains("the package action computes")),
        "{found:?}"
    );
    let found = problems(
        &text,
        &package,
        &next,
        &format!("{release}\n  build-update:\n"),
    );
    assert!(
        found
            .iter()
            .any(|p| p.contains("release.yml names the update package")),
        "{found:?}"
    );
}

/// The commands of both actions read their environment: no step expands an expression in its
/// command, and every command runs under bash.
#[test]
fn the_commands_of_the_actions_read_their_environment_and_run_under_bash() {
    for path in [PACKAGE_ACTION, NEXT_ACTION] {
        let text = read(path);
        assert_eq!(
            expressions_in_commands(&text),
            Vec::<String>::new(),
            "{path}"
        );
        let commands = code(&text)
            .iter()
            .filter(|l| l.trim_start().starts_with("run: "))
            .count();
        let shells = code(&text)
            .iter()
            .filter(|l| l.trim() == "shell: bash")
            .count();
        assert!(commands > 0, "{path}");
        assert_eq!(commands, shells, "{path}: a command without shell: bash");
        assert!(text.contains("using: composite"), "{path}");
    }
}

/// A command that expands an expression, on one line or inside a block, is named; an expression
/// in an `env:` value is not.
#[test]
fn an_expression_in_a_command_is_named_and_one_in_an_env_value_is_not() {
    let env = "    - shell: bash\n      env:\n        VERSION: ${{ inputs.version }}\n      run: echo \"$VERSION\"\n";
    assert_eq!(expressions_in_commands(env), Vec::<String>::new());
    let one = "    - shell: bash\n      run: echo ${{ inputs.version }}\n";
    assert_eq!(expressions_in_commands(one).len(), 1);
    let block = "    - shell: bash\n      run: |\n        set -e\n        echo ${{ inputs.version }}\n      env:\n        A: ${{ inputs.version }}\n";
    assert_eq!(
        expressions_in_commands(block),
        ["echo ${{ inputs.version }}"]
    );
}
