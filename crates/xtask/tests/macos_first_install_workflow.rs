//! The workflow that runs the first install of the macOS package
//! (`.github/workflows/js-agent-macos-first-install.yml`) read as it is in the repository. It is
//! called (by the release's beta stage and by the package workflow's manual run), never started by
//! an event of its own; it has one job on a hosted Apple silicon runner with `contents: read` and
//! no secret; it installs the unsigned package its caller's run built after checking the package
//! against its sums; it builds the test as the runner's user and runs it as root through cargo's
//! target runner, so nothing is built as root.

use std::path::Path;

const WORKFLOW: &str = ".github/workflows/js-agent-macos-first-install.yml";

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

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The trimmed lines of the block under the line `key` (the lines indented more than it).
fn block<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let Some(at) = lines.iter().position(|l| l.trim_end() == key) else {
        return Vec::new();
    };
    let base = indent(lines[at]);
    lines[at + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || indent(l) > base)
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The steps of the job: each is the lines from one `- ` at six spaces to the next.
fn steps<'a>(lines: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut found: Vec<Vec<&str>> = Vec::new();
    for line in lines {
        if indent(line) == 6 && line.trim_start().starts_with("- ") {
            found.push(vec![*line]);
        } else if let Some(last) = found.last_mut() {
            last.push(line);
        }
    }
    found
}

/// The commands of the `run:` lines of one step (a one-line `run:` is one command).
fn run_of<'a>(step: &[&'a str]) -> Option<&'a str> {
    step.iter().find_map(|l| {
        l.trim_start()
            .trim_start_matches("- ")
            .strip_prefix("run: ")
    })
}

/// One line per way the workflow `text` is not the called, secret-free, one-job run of the first
/// install of the unsigned package on a macOS runner, as root only at the test's run.
fn problems(text: &str) -> Vec<String> {
    let body = code(text);
    let joined = body.join("\n");
    let mut found = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            found.push(what.to_owned());
        }
    };
    let on: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "on:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter(|l| indent(l) == 2)
        .map(|l| l.trim())
        .collect();
    want(
        on == ["workflow_call:"],
        "the workflow is started by an event besides workflow_call",
    );
    want(
        !joined.contains("pull_request")
            && !joined.contains("workflow_dispatch")
            && !joined.contains("push:"),
        "the workflow names an event that starts it",
    );
    want(
        body.contains(&"permissions: {}"),
        "the top level grants permissions",
    );
    let job_at = body.iter().position(|l| *l == "jobs:");
    let job_lines: Vec<&str> = job_at.map_or(Vec::new(), |at| body[at + 1..].to_vec());
    let job_names: Vec<&str> = job_lines
        .iter()
        .filter(|l| indent(l) == 2)
        .map(|l| l.trim())
        .collect();
    want(
        job_names == ["first-install:"],
        "the workflow does not have exactly the job first-install",
    );
    want(
        job_lines.iter().any(|l| l.trim() == "runs-on: macos-15"),
        "the job does not run on macos-15",
    );
    want(
        block(&job_lines, "    permissions:") == ["contents: read"],
        "the job's permissions are not contents: read",
    );
    want(!joined.contains("secret"), "the workflow names a secret");
    want(
        !joined.contains("environment:"),
        "the job runs in an environment",
    );
    want(
        !joined.contains("id-token"),
        "the workflow asks for id-token",
    );
    want(
        !joined.contains("write"),
        "the workflow names a write scope",
    );
    for line in body.iter().filter(|l| l.contains("uses:")) {
        let pin = line.split('@').nth(1).unwrap_or("");
        want(
            pin.len() >= 40 && pin[..40].chars().all(|c| c.is_ascii_hexdigit()),
            &format!("an action is not pinned to a commit: {}", line.trim()),
        );
    }
    want(
        joined.contains("persist-credentials: false"),
        "the checkout keeps the credentials",
    );
    let all = steps(&job_lines);
    let step_text = |at: usize| all.get(at).map_or(String::new(), |s| s.join("\n"));
    let position = |needle: &str| all.iter().position(|s| s.join("\n").contains(needle));
    want(
        !body
            .iter()
            .any(|l| l.trim_end().ends_with("run: |") || l.trim_end().ends_with("run: >")),
        "a run step holds more than one command line",
    );
    let download = position("actions/download-artifact");
    want(
        download.is_some_and(|d| {
            let t = step_text(d);
            t.contains("name: js-agent-macos-pkg-unsigned") && t.contains("path: dist")
        }),
        "the artifact downloaded is not js-agent-macos-pkg-unsigned into dist",
    );
    let sums = position("shasum -a 256 --check SHA256SUMS");
    let first_cargo = position("cargo ");
    want(
        sums.is_some_and(|s| {
            download.is_some_and(|d| d < s)
                && first_cargo.is_some_and(|c| s < c)
                && step_text(s).contains("working-directory: dist")
                && step_text(s).contains("/usr/bin/shasum -a 256 --check SHA256SUMS")
        }),
        "the download is not checked against its sums before anything is built",
    );
    let build = position("--no-run");
    let run = position("--ignored");
    want(
        build.is_some_and(|b| {
            run.is_some_and(|r| b < r)
                && step_text(b)
                    .contains("cargo test --locked -p js-agent --test mac_first_install --no-run")
        }),
        "the test is not built with --no-run before it is run",
    );
    want(
        run.is_some_and(|r| {
            let t = step_text(r);
            t.contains("--test mac_first_install -- --ignored")
        }),
        "the run does not pass --ignored to the mac_first_install test target",
    );
    want(
        joined.matches("sudo").count() == 1
            && joined.contains(
                "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER: sudo --preserve-env=JS_FIRST_INSTALL_PKG,JS_FIRST_INSTALL_VERSION",
            ),
        "sudo is used other than as the cargo target runner that keeps the package path and version",
    );
    for step in &all {
        if let Some(command) = run_of(step) {
            want(
                !command.trim_start().starts_with("sudo"),
                "a run command starts with sudo",
            );
            want(
                !command.contains("${{"),
                &format!("a run expands an expression: {command}"),
            );
        }
    }
    for line in &body {
        if indent(line) >= 10 && line.contains("${{") {
            want(
                !line.contains("cargo")
                    && !line.trim_start().starts_with("run:")
                    && !line.trim_start().starts_with('-'),
                &format!("a script line expands an expression: {}", line.trim()),
            );
        }
    }
    found
}

/// The real workflow is the called, secret-free first install and nothing else.
#[test]
fn the_workflow_is_the_called_secret_free_first_install_of_the_unsigned_package() {
    assert_eq!(problems(&read(WORKFLOW)), Vec::<String>::new());
}

/// The version is the one input, with the same default as the package workflow, and the
/// environment fixes the deployment target the package's programs are built for.
#[test]
fn the_workflow_takes_the_version_as_its_one_input_and_defaults_it() {
    let text = read(WORKFLOW);
    assert!(text.contains(
        "  workflow_call:\n    inputs:\n      version:\n        description: The package version, MAJOR.MINOR.PATCH\n        required: false\n        type: string\n        default: 0.0.0\n"
    ));
    assert!(text.contains("  VERSION: ${{ inputs.version || '0.0.0' }}\n"));
    assert!(text.contains("  MACOSX_DEPLOYMENT_TARGET: \"11.0\"\n"));
}

/// Each way the workflow can leave the shape above is named.
#[test]
fn each_way_of_leaving_the_first_install_shape_is_named() {
    let text = read(WORKFLOW);
    let table: [(&str, &str, &str); 19] = [
        (
            "on:\n  workflow_call:\n",
            "on:\n  pull_request:\n  workflow_call:\n",
            "an event besides workflow_call",
        ),
        (
            "on:\n  workflow_call:\n",
            "on:\n  workflow_dispatch:\n  workflow_call:\n",
            "an event besides workflow_call",
        ),
        (
            "on:\n  workflow_call:\n",
            "on:\n  push:\n  workflow_call:\n",
            "an event besides workflow_call",
        ),
        (
            "\npermissions: {}\n",
            "\npermissions:\n  contents: read\n",
            "the top level grants permissions",
        ),
        (
            "    runs-on: macos-15\n",
            "    runs-on: ubuntu-24.04\n",
            "does not run on macos-15",
        ),
        (
            "    permissions:\n      contents: read\n",
            "    permissions:\n      contents: read\n      packages: read\n",
            "permissions are not contents: read",
        ),
        (
            "    permissions:\n      contents: read\n",
            "    permissions:\n      contents: write\n",
            "permissions are not contents: read",
        ),
        (
            "    permissions:\n      contents: read\n",
            "    permissions:\n      contents: read\n      id-token: none\n",
            "asks for id-token",
        ),
        (
            "    timeout-minutes: 60\n",
            "    timeout-minutes: 60\n    environment: pkg-signing\n",
            "runs in an environment",
        ),
        (
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n        env:\n          TOKEN: ${{ secrets.TOKEN }}\n",
            "names a secret",
        ),
        (
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
            "actions/checkout@v7",
            "not pinned to a commit",
        ),
        (
            "        run: /usr/bin/shasum -a 256 --check SHA256SUMS\n",
            "        run: |\n          set -e\n          /usr/bin/shasum -a 256 --check SHA256SUMS\n",
            "a run step holds more than one command line",
        ),
        (
            "name: js-agent-macos-pkg-unsigned",
            "name: js-agent-macos-pkg-signed",
            "the artifact downloaded is not js-agent-macos-pkg-unsigned",
        ),
        (
            "run: /usr/bin/shasum -a 256 --check SHA256SUMS",
            "run: /usr/bin/shasum -a 256 SHA256SUMS",
            "not checked against its sums",
        ),
        (
            "cargo test --locked -p js-agent --test mac_first_install --no-run",
            "cargo build --locked -p js-agent --test mac_first_install",
            "not built with --no-run",
        ),
        (
            "mac_first_install -- --ignored --nocapture",
            "mac_first_install -- --nocapture",
            "does not pass --ignored",
        ),
        (
            "        run: JS_FIRST_INSTALL_PKG=",
            "        run: sudo JS_FIRST_INSTALL_PKG=",
            "a run command starts with sudo",
        ),
        (
            "      - run: rustup toolchain install\n",
            "      - run: sudo cargo build --release\n      - run: rustup toolchain install\n",
            "a run command starts with sudo",
        ),
        (
            "dist/js-agent-$VERSION-macos-arm64.pkg\" JS_FIRST",
            "dist/js-agent-${{ inputs.version }}-macos-arm64.pkg\" JS_FIRST",
            "a run expands an expression",
        ),
    ];
    for (from, to, says) in table {
        assert_eq!(text.matches(from).count(), 1, "{from:?}");
        let found = problems(&text.replacen(from, to, 1));
        assert!(
            found.iter().any(|p| p.contains(says)),
            "{from:?} -> {to:?} must say {says:?}: {found:?}"
        );
    }
}
