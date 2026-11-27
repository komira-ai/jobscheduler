//! The black-box tests of the released agent and server (`.github/workflows/js-blackbox.yml`)
//! and their call from the release (`release.yml`'s job `blackbox-linux`), read as they are in the
//! repository.
//!
//! The workflow is called by the release and run by hand, never started by another event; it has
//! one job on a hosted Linux x86_64 runner with `contents: read` and no secret; it builds the test
//! as the runner's user and runs its wrapper (`released rig`, given the release as its
//! `--version`) as root through cargo's target runner, so nothing is built as root. The release calls it once `check-apt` passed, for a push to main
//! that released, and no job of the release waits for it.

use std::path::Path;

const WORKFLOW: &str = ".github/workflows/js-blackbox.yml";
const RELEASE: &str = ".github/workflows/release.yml";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of a workflow without comments.
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

/// The command of a step's `run:` line.
fn run_of<'a>(step: &[&'a str]) -> Option<&'a str> {
    step.iter().find_map(|l| {
        l.trim_start()
            .trim_start_matches("- ")
            .strip_prefix("run: ")
    })
}

/// One line per way the workflow `text` is not the called or manual, secret-free, one-job run
/// of the black-box test on a Linux x86_64 runner, as root only at the test's run.
fn problems(text: &str) -> Vec<String> {
    let body = code(text);
    let joined = body.join("\n");
    let mut found = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            found.push(what.to_owned());
        }
    };
    let on: Vec<&str> = body
        .iter()
        .skip_while(|l| **l != "on:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter(|l| indent(l) == 2)
        .map(|l| l.trim())
        .collect();
    want(
        on == ["workflow_call:", "workflow_dispatch:"],
        "the workflow is started by an event besides workflow_call and workflow_dispatch",
    );
    want(
        body.contains(&"permissions: {}"),
        "the top level grants permissions",
    );
    want(
        body.contains(&"  VERSION: ${{ inputs.version }}"),
        "the version is not the input's",
    );
    let job_at = body.iter().position(|l| *l == "jobs:");
    let job_lines: Vec<&str> = job_at.map_or(Vec::new(), |at| body[at + 1..].to_vec());
    let job_names: Vec<&str> = job_lines
        .iter()
        .filter(|l| indent(l) == 2)
        .map(|l| l.trim())
        .collect();
    want(
        job_names == ["blackbox:"],
        "the workflow does not have exactly the job blackbox",
    );
    want(
        job_lines
            .iter()
            .any(|l| l.trim() == "runs-on: ubuntu-24.04"),
        "the job does not run on ubuntu-24.04",
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
    want(
        !body
            .iter()
            .any(|l| l.trim_end().ends_with("run: |") || l.trim_end().ends_with("run: >")),
        "a run step holds more than one command line",
    );
    let all = steps(&job_lines);
    let position = |needle: &str| all.iter().position(|s| s.join("\n").contains(needle));
    let step_text = |at: usize| all.get(at).map_or(String::new(), |s| s.join("\n"));
    let build = position("--no-run");
    let run = position("-- rig ");
    want(
        matches!((build, run), (Some(b), Some(r)) if b < r),
        "the test is not built before its wrapper runs",
    );
    want(
        build.is_some_and(|b| {
            step_text(b).contains("cargo test --locked -p js-blackbox --test released --no-run")
        }),
        "the test built is not js-blackbox's released",
    );
    want(
        run.is_some_and(|r| {
            step_text(r).contains(
                "cargo test --locked -p js-blackbox --test released -- rig --version \"$VERSION\"",
            )
        }),
        "the run is not the released test's wrapper given the release's version",
    );
    want(
        joined.matches("sudo").count() == 1
            && joined.contains("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER: sudo\n"),
        "sudo is used other than as the cargo target runner of the test's run",
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
    found
}

/// The lines of the job `name` of a workflow's `jobs:`, comments included.
fn job_of<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let head = format!("  {name}:");
    text.lines()
        .skip_while(|l| *l != head)
        .skip(1)
        .take_while(|l| l.trim().is_empty() || indent(l) > 2)
        .collect()
}

/// One line per way the release's call is not the call after `check-apt` that nothing waits
/// for.
fn call_problems(release: &str) -> Vec<String> {
    let job = code_lines(&job_of(release, "blackbox-linux"));
    let mut found = Vec::new();
    let mut want = |ok: bool, what: &str| {
        if !ok {
            found.push(what.to_owned());
        }
    };
    want(
        job.contains(&"    uses: ./.github/workflows/js-blackbox.yml"),
        "blackbox-linux does not call js-blackbox.yml",
    );
    want(
        job.contains(&"    needs: [plan, check-apt]"),
        "blackbox-linux does not need exactly plan and check-apt",
    );
    want(
        job.contains(
            &"    if: ${{ !cancelled() && !failure() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && needs.check-apt.result == 'success' }}",
        ),
        "blackbox-linux does not run only for a push to main that released, after check-apt passed",
    );
    want(
        block(&job, "    permissions:") == ["contents: read"],
        "blackbox-linux's permissions are not contents: read",
    );
    want(
        block(&job, "    with:") == ["version: ${{ needs.plan.outputs.version }}"],
        "blackbox-linux is not given the plan's version alone",
    );
    want(
        !job.iter().any(|l| l.trim_start().starts_with("secrets:")),
        "blackbox-linux passes secrets",
    );
    let waiting: Vec<&str> = release
        .lines()
        .filter(|l| {
            l.trim_start().starts_with("needs:")
                && l.split([' ', '[', ']', ','])
                    .any(|name| name == "blackbox-linux")
        })
        .collect();
    want(
        waiting.is_empty(),
        "a job of the release needs blackbox-linux",
    );
    found
}

fn code_lines<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .copied()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

/// The text with `old`, which it holds exactly once, replaced by `new`.
fn edit(text: &str, old: &str, new: &str) -> String {
    assert_eq!(text.matches(old).count(), 1, "{old}");
    text.replacen(old, new, 1)
}

/// The real workflow is the called and manual, secret-free run of the black-box test.
#[test]
fn the_workflow_is_the_secret_free_root_run_of_the_released_test() {
    assert_eq!(problems(&read(WORKFLOW)), Vec::<String>::new());
}

/// The release calls it after `check-apt`, for a push to main that released, and nothing waits
/// for it.
#[test]
fn the_release_calls_it_after_check_apt_and_waits_for_it_nowhere() {
    assert_eq!(call_problems(&read(RELEASE)), Vec::<String>::new());
}

/// The version is the one input of each event, and required.
#[test]
fn the_workflow_takes_the_version_as_its_one_required_input() {
    let text = read(WORKFLOW);
    for event in ["workflow_call", "workflow_dispatch"] {
        let head = format!("  {event}:\n    inputs:\n      version:\n");
        let at = text.find(&head).unwrap_or_else(|| panic!("{event}"));
        let rest = &text[at + head.len()..];
        let input: Vec<&str> = rest.lines().take_while(|l| indent(l) >= 8).collect();
        assert!(input.contains(&"        required: true"), "{event}");
        assert!(input.contains(&"        type: string"), "{event}");
        assert_eq!(input.len(), 3, "{event}: {input:?}");
    }
}

/// Each way the workflow can leave the shape above is named.
#[test]
fn each_way_of_leaving_the_workflows_shape_is_named() {
    let text = read(WORKFLOW);
    let table: [(&str, &str, &str); 16] = [
        (
            "on:\n  workflow_call:\n",
            "on:\n  pull_request:\n  workflow_call:\n",
            "an event besides workflow_call and workflow_dispatch",
        ),
        (
            "on:\n  workflow_call:\n",
            "on:\n  push:\n  workflow_call:\n",
            "an event besides workflow_call and workflow_dispatch",
        ),
        (
            "\npermissions: {}\n",
            "\npermissions:\n  contents: read\n",
            "the top level grants permissions",
        ),
        (
            "  VERSION: ${{ inputs.version }}\n",
            "  VERSION: 0.1.0\n",
            "the version is not the input's",
        ),
        (
            "  blackbox:\n    name: js-blackbox\n",
            "  other:\n    name: js-blackbox\n",
            "not have exactly the job blackbox",
        ),
        (
            "    runs-on: ubuntu-24.04\n",
            "    runs-on: ubuntu-24.04-arm\n",
            "does not run on ubuntu-24.04",
        ),
        (
            "    permissions:\n      contents: read\n",
            "    permissions:\n      contents: read\n      packages: read\n",
            "permissions are not contents: read",
        ),
        (
            "    timeout-minutes: 60\n",
            "    timeout-minutes: 60\n    environment: apt-publish\n",
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
            "persist-credentials: false",
            "persist-credentials: true",
            "keeps the credentials",
        ),
        (
            "        run: cargo test --locked -p js-blackbox --test released --no-run\n",
            "        run: |\n          cargo test --locked -p js-blackbox --test released --no-run\n",
            "more than one command line",
        ),
        (
            "      - name: Build the test as the runner's user\n        run: cargo test --locked -p js-blackbox --test released --no-run\n",
            "",
            "the test is not built before its wrapper runs",
        ),
        (
            "--test released --no-run",
            "--test judge --no-run",
            "the test built is not js-blackbox's released",
        ),
        (
            "-- rig --version \"$VERSION\"",
            "-- rig --version 0.1.0",
            "given the release's version",
        ),
        (
            "          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER: sudo\n        run: cargo test",
            "          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER: env\n        run: sudo cargo test",
            "a run command starts with sudo",
        ),
    ];
    for (old, new, said) in table {
        let found = problems(&edit(&text, old, new));
        assert!(
            found.iter().any(|p| p.contains(said)),
            "{said:?} not in {found:?} for {new:?}"
        );
    }
    let expands = edit(
        &text,
        "-- rig --version \"$VERSION\"",
        "-- rig --version \"${{ inputs.version }}\"",
    );
    assert!(
        problems(&expands)
            .iter()
            .any(|p| p.contains("a run expands an expression"))
    );
}

/// Each way the release's call can leave its shape is named.
#[test]
fn each_way_of_leaving_the_calls_shape_is_named() {
    let text = read(RELEASE);
    let table: [(&str, &str, &str); 7] = [
        (
            "    uses: ./.github/workflows/js-blackbox.yml\n",
            "    uses: ./.github/workflows/beta.yml\n",
            "does not call js-blackbox.yml",
        ),
        (
            "  blackbox-linux:\n    needs: [plan, check-apt]\n",
            "  blackbox-linux:\n    needs: [plan, publish]\n",
            "does not need exactly plan and check-apt",
        ),
        (
            " && needs.check-apt.result == 'success' }}\n    permissions:\n      contents: read\n    uses: ./.github/workflows/js-blackbox.yml",
            " }}\n    permissions:\n      contents: read\n    uses: ./.github/workflows/js-blackbox.yml",
            "after check-apt passed",
        ),
        (
            "    permissions:\n      contents: read\n    uses: ./.github/workflows/js-blackbox.yml",
            "    permissions:\n      contents: read\n      id-token: write\n    uses: ./.github/workflows/js-blackbox.yml",
            "permissions are not contents: read",
        ),
        (
            "    uses: ./.github/workflows/js-blackbox.yml\n    with:\n      version: ${{ needs.plan.outputs.version }}\n",
            "    uses: ./.github/workflows/js-blackbox.yml\n    with:\n      version: 0.1.0\n",
            "not given the plan's version alone",
        ),
        (
            "    uses: ./.github/workflows/js-blackbox.yml\n",
            "    uses: ./.github/workflows/js-blackbox.yml\n    secrets: inherit\n",
            "passes secrets",
        ),
        (
            "  deployed:\n    needs: [turn, ",
            "  deployed:\n    needs: [blackbox-linux, turn, ",
            "a job of the release needs blackbox-linux",
        ),
    ];
    for (old, new, said) in table {
        let found = call_problems(&edit(&text, old, new));
        assert!(
            found.iter().any(|p| p.contains(said)),
            "{said:?} not in {found:?} for {new:?}"
        );
    }
}
