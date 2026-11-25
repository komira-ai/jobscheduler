//! The beta workflow's step that runs the native runtime's root tests as root
//! (`.github/workflows/beta.yml`) read as it is in the repository: it runs on every outcome of
//! the steps before it, runs the ignored tests of `native_privileged` one at a time, and skips
//! exactly the VM test, whose name is a test of `crates/js-agent/tests/native_privileged/vm.rs`.

use std::path::Path;

const STEP: &str = "Run the native runtime's root tests as root";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The trimmed lines of the step named `STEP`: from its `- name:` line to the next step.
fn step_lines(beta_yml: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = beta_yml
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    let at = lines
        .iter()
        .position(|l| l.trim() == format!("- name: {STEP}"))?;
    let base = lines[at].len() - lines[at].trim_start().len();
    Some(
        lines[at + 1..]
            .iter()
            .take_while(|l| {
                !l.trim_start().starts_with("- ") || l.len() - l.trim_start().len() > base
            })
            .map(|l| l.trim())
            .collect(),
    )
}

/// One line per way the step of `beta_yml` is not the as-root run of the native runtime's tests
/// without the VM one, given the source `vm_src` of the VM tests.
fn check(beta_yml: &str, vm_src: &str) -> Result<(), Vec<String>> {
    let Some(step) = step_lines(beta_yml) else {
        return Err(vec![format!("the step \"{STEP}\" is missing")]);
    };
    let mut found = Vec::new();
    if !step.contains(&"if: ${{ !cancelled() }}") {
        found.push("the step does not run when an earlier step failed".to_owned());
    }
    let run = step
        .iter()
        .find_map(|l| l.strip_prefix("run: "))
        .unwrap_or_default();
    for flag in ["--test native_privileged", "--ignored", "--test-threads=1"] {
        if !run.contains(flag) {
            found.push(format!("the run does not pass {flag}"));
        }
    }
    let words: Vec<&str> = run.split_whitespace().collect();
    match words.iter().position(|w| *w == "--skip") {
        None => found.push("the run does not skip the VM test".to_owned()),
        Some(at) => {
            let name = words.get(at + 1).copied().unwrap_or_default();
            let is_a_test = vm_src.lines().any(|l| {
                l.trim()
                    .strip_prefix("fn ")
                    .is_some_and(|rest| rest.contains(name) && !name.is_empty())
            });
            if !is_a_test {
                found.push(format!("the skipped name {name} is not a VM test"));
            }
        }
    }
    if found.is_empty() { Ok(()) } else { Err(found) }
}

const VM: &str = "crates/js-agent/tests/native_privileged/vm.rs";

/// Asserts that the repository's beta workflow runs the native runtime's root tests as root,
/// every test but the VM one.
///
/// Catches a beta stage that stops running the native root tests, or skips a test that no
/// longer exists.
#[test]
fn the_beta_stage_runs_the_native_root_tests_as_root() {
    assert_eq!(
        check(&read(".github/workflows/beta.yml"), &read(VM)),
        Ok(())
    );
}

/// Asserts that a step missing, missing a flag, not skipping the VM test, skipping a name no
/// VM test has, or not running after a failure is refused, each with its own message.
///
/// Catches a check that passes a workflow whose native root run is wrong.
#[test]
fn a_beta_step_that_breaks_the_native_root_run_is_refused() {
    let beta = read(".github/workflows/beta.yml");
    let vm = read(VM);
    let skip = " --skip a_command_runs_in_a_fresh_vm";
    assert!(beta.contains(skip));
    let refused = |beta: &str, vm: &str| check(beta, vm).unwrap_err();

    assert_eq!(
        refused(&beta.replace(STEP, "Another step"), &vm),
        [format!("the step \"{STEP}\" is missing")]
    );
    assert_eq!(
        refused(
            &beta.replace(
                " --ignored --nocapture --test-threads=1 --skip",
                " --nocapture --test-threads=1 --skip"
            ),
            &vm
        ),
        ["the run does not pass --ignored"]
    );
    assert_eq!(
        refused(&beta.replace(skip, ""), &vm),
        ["the run does not skip the VM test"]
    );
    assert_eq!(
        refused(&beta.replace(skip, " --skip no_such_test"), &vm),
        ["the skipped name no_such_test is not a VM test"]
    );
    assert_eq!(
        refused(
            &beta,
            &vm.replace("fn a_command_runs_in_a_fresh_vm", "fn renamed")
        ),
        ["the skipped name a_command_runs_in_a_fresh_vm is not a VM test"]
    );
    let after_name = format!("- name: {STEP}\n        if: ${{{{ !cancelled() }}}}\n");
    assert!(beta.contains(&after_name));
    assert_eq!(
        refused(
            &beta.replace(&after_name, &format!("- name: {STEP}\n")),
            &vm
        ),
        ["the step does not run when an earlier step failed"]
    );
}
