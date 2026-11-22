//! The version each job of the package workflow checks before anything else, and the one
//! command that checks it and names the next one for the update package.

use super::*;

/// One line per way the jobs of the workflow `text` that run steps do not check their version
/// first (after the presence check, in `sign`). `build` and `sign` check it in a step of their own;
/// `build-update` builds the next version, and the one command that names it checks the version
/// (`cargo xtask release next-version`), after the checkout and the toolchain it needs.
/// `sign-code` signs the programs `build` made and names no version, so it checks none.
fn version_check_problems(text: &str, next_action: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (name, job) in step_jobs(text) {
        if name == "sign-code" {
            continue;
        }
        let all = steps(&job);
        if name == "build-update" {
            let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
            let at = |what: &str| {
                all.iter()
                    .position(|(_, l)| l.iter().any(|l| l.contains(what)))
            };
            let next = at("uses: ./.github/actions/js-agent-next-version");
            let checkout = at("uses: actions/checkout@");
            let rustup = at("run: rustup toolchain install");
            let package = at("uses: ./.github/actions/js-agent-macos-pkg");
            if !matches!((checkout, rustup, next, package), (Some(c), Some(r), Some(n), Some(p)) if c < r && r < n && n < p)
            {
                found.push(format!(
                    "{name}: the next version is not named after the checkout and the toolchain and before the package is built: {names:?}"
                ));
            }
            if job.iter().any(|l| l.contains("BASH_REMATCH")) {
                found.push(format!("{name}: a bash check of the version"));
            }
            continue;
        }
        let at = usize::from(name == "sign");
        let (step, lines) = &all[at];
        if step != "Check the version" {
            found.push(format!("{name}: the first step is {step:?}"));
            continue;
        }
        let body = lines.join("\n");
        if !body
            .contains(r#"[[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]"#)
            || !body.contains("exit 1")
        {
            found.push(format!("{name}: the check is not the version pattern"));
        }
    }
    let command = code(next_action)
        .iter()
        .filter_map(|l| l.trim_start().strip_prefix("run: "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if command
        != [r#"cargo xtask release next-version --version "$VERSION" --output "$GITHUB_OUTPUT""#]
    {
        found.push(format!(
            "the next-version action does not run the one xtask command: {command:?}"
        ));
    }
    found
}

#[test]
fn the_jobs_that_run_steps_check_the_version_before_anything_else_but_the_presence_check() {
    assert_eq!(
        version_check_problems(&read(WORKFLOW), &read(NEXT_ACTION)),
        Vec::<String>::new()
    );
}

/// Each way of leaving the version checks is named: a missing or moved step, a bash check back in
/// `build-update`, and a next-version action that is not the one xtask command.
#[test]
fn each_way_of_skipping_the_version_check_is_named() {
    let text = read(WORKFLOW);
    let action = read(NEXT_ACTION);
    let update = "      - run: rustup toolchain install\n      - name: Name the next version\n";
    assert_eq!(text.matches(update).count(), 1);
    let cases = [
        (
            text.replacen(update, "      - name: Name the next version\n", 1),
            action.clone(),
            "the next version is not named after the checkout and the toolchain",
        ),
        (
            text.replacen(
                "      - name: Name the next version\n",
                "      - name: Name the next version\n        run: |\n          [[ \"$VERSION\" =~ x ]] && echo \"${BASH_REMATCH[1]}\"\n",
                1,
            ),
            action.clone(),
            "a bash check of the version",
        ),
        (
            text.replacen("      - name: Check the version\n", "      - name: Check it\n", 1),
            action.clone(),
            "the first step is",
        ),
        (
            text.clone(),
            action.replace("run: cargo xtask release next-version", "run: echo"),
            "does not run the one xtask command",
        ),
        (
            text.clone(),
            action.replace("run: cargo", "run: |\n        set -e\n        cargo"),
            "does not run the one xtask command",
        ),
    ];
    for (workflow, next, says) in cases {
        let found = version_check_problems(&workflow, &next);
        assert!(
            found.iter().any(|p| p.contains(says)),
            "must say {says:?}: {found:?}"
        );
    }
}
