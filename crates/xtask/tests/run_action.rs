//! The rules of the GitHub Action `actions/run` (the comment at the top of
//! `actions/run/action.yml`), read from the file as text: every action it uses is pinned to a
//! commit, it names no tailnet secret (the join is federated), every `run:` is one line run by
//! bash, and every flag it passes to `js` is one `js` has. The workflow lints read
//! `.github/workflows` only, so the action has this lint of its own.

use std::path::Path;

/// The action's text.
fn action() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../actions/run/action.yml");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read the action: {e}"))
}

/// Words that would put a tailnet secret into the action: the join takes a client id and an
/// audience, never a secret or an auth key.
const SECRET_WORDS: [&str; 3] = ["oauth-secret", "authkey", "secrets."];

/// The program the action runs, as its run step names it.
const JS: &str = "\"$JS_CI_DIR/js\" --server";

/// What breaks the action's rules, one line each; empty when it keeps them all.
fn problems(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if !text.lines().any(|line| line == "  using: composite") {
        found.push("the action is not a composite action".to_owned());
    }
    let mut runs = 0;
    let mut shells = 0;
    for (number, line) in text.lines().enumerate().map(|(at, line)| (at + 1, line)) {
        let item = line.trim_start().trim_start_matches("- ");
        if let Some(used) = item.strip_prefix("uses: ") {
            let pinned = used
                .split_once('@')
                .and_then(|(_, rest)| rest.split_once(" # v"));
            let sha = pinned.map_or("", |(sha, _)| sha);
            let full =
                sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !full {
                found.push(format!(
                    "line {number}: {used} is not pinned to a commit with its version in a comment"
                ));
            }
        }
        if let Some(run) = item.strip_prefix("run: ") {
            runs += 1;
            if run.starts_with('|') {
                found.push(format!("line {number}: a run: block of several lines"));
            }
        }
        if item == "shell: bash" {
            shells += 1;
        }
        for word in SECRET_WORDS {
            if !line.trim_start().starts_with('#') && line.contains(word) {
                found.push(format!("line {number}: names {word}"));
            }
        }
    }
    if runs != shells {
        found.push(format!("{runs} run: steps and {shells} shell: bash lines"));
    }
    found
}

/// The flags the action passes to `js` before and after `ci run`: those of the step that runs
/// `js`, from the program to the next step.
fn js_flags(text: &str) -> Vec<String> {
    let start = text.find(JS).expect("the action runs js");
    let step = &text[start..];
    let step = step.split("\n    - name:").next().unwrap_or_default();
    let words = step.split_whitespace();
    let flags = words.filter(|word| word.starts_with("--") && word.len() > 2);
    flags.map(str::to_owned).collect()
}

/// The flags of `flags` that `js ci run` (with the global flags) does not have.
fn unknown(flags: &[String]) -> Vec<String> {
    let tree = js_cli::command();
    let run = tree
        .find_subcommand("ci")
        .and_then(|ci| ci.find_subcommand("run"));
    let run = run.expect("js has a ci run command");
    let known: Vec<&str> = run
        .get_arguments()
        .chain(tree.get_arguments())
        .filter_map(clap::Arg::get_long)
        .collect();
    let unknown = flags.iter().filter(|flag| !known.contains(&&flag[2..]));
    unknown.cloned().collect()
}

/// The action with `old` replaced by `new` once, which must be in it.
fn changed(old: &str, new: &str) -> String {
    let text = action();
    assert!(text.contains(old), "the action has no {old:?}");
    text.replacen(old, new, 1)
}

/// Asserts that the action keeps every rule: composite, every `uses:` pinned to a commit, no
/// tailnet secret named, every `run:` one line with `shell: bash`. It catches an action moved
/// to a tag, a secret added for the join, and multi-line logic in a step.
#[test]
fn the_action_keeps_its_rules() {
    assert_eq!(problems(&action()), Vec::<String>::new());
}

/// Asserts that each breach is refused, by its whole message: an action pinned to a tag, a
/// tailnet auth key, a run: block, a run: step with no shell line, and a non-composite action.
/// It catches a rule that refuses nothing.
#[test]
fn each_breach_of_the_rules_is_refused() {
    let tag = changed(
        "tailscale/github-action@d1b6cd204f8dceda5b3eaad7f1f767be390056cd # v4.2.0",
        "tailscale/github-action@v4",
    );
    let line = tag.lines().position(|l| l.contains("@v4")).unwrap() + 1;
    assert_eq!(
        problems(&tag),
        [format!(
            "line {line}: tailscale/github-action@v4 is not pinned to a commit with its version in a comment"
        )]
    );
    let key = changed("        use-cache: \"false\"\n", "        authkey: x\n");
    let line = key.lines().position(|l| l.contains("authkey")).unwrap() + 1;
    assert_eq!(problems(&key), [format!("line {line}: names authkey")]);
    let block = changed("      run: tar -xzf", "      run: |\n        tar -xzf");
    let line = block.lines().position(|l| l == "      run: |").unwrap() + 1;
    assert_eq!(
        problems(&block),
        [format!("line {line}: a run: block of several lines")]
    );
    let shells = action().matches("shell: bash").count();
    let no_shell = changed(
        "    - name: Unpack js\n      shell: bash\n",
        "    - name: Unpack js\n",
    );
    assert_eq!(
        problems(&no_shell),
        [format!(
            "{shells} run: steps and {} shell: bash lines",
            shells - 1
        )]
    );
    let node = changed("  using: composite", "  using: node24");
    assert_eq!(problems(&node), ["the action is not a composite action"]);
}

/// Asserts that every flag the action passes to `js` is a flag of `js ci run` or a global flag
/// of `js`, and that the action passes the ones a step needs. It catches a flag renamed in js
/// and not in the action, which would fail every step at once.
#[test]
fn every_flag_the_action_passes_is_one_js_ci_run_has() {
    let flags = js_flags(&action());
    assert_eq!(unknown(&flags), Vec::<String>::new());
    for needed in [
        "--server",
        "--token-file",
        "--os",
        "--run",
        "--source-commit",
        "--github-output",
    ] {
        assert!(flags.iter().any(|flag| flag == needed), "{needed}");
    }
    let renamed = js_flags(&changed("--source-commit", "--source-sha"));
    assert_eq!(unknown(&renamed), ["--source-sha"]);
}
