//! The workflows that build a release binary name the binary with `--bin`, so that the test-only
//! binaries of a package (the fake update hook of js-agent) are never built, and never shipped,
//! with it. Read from the files as they are in the repository.
//!
//! The rule, for every `cargo build` or `cargo install` that builds for release (`--release`,
//! `-r`, `--profile release`), with or without `-p`: it carries `--bin <name>` (or
//! `--bin=<name>`), or it is the build of the one package `xtask`, alone. `--workspace`, another
//! package beside `xtask`, or no package at all do not escape it.

use std::path::Path;

const WORKFLOWS: &str = ".github/workflows";

/// The commands of `text` as lists of words: comment lines dropped, `\` continuations joined,
/// and a line split at `&&`, `||`, `;` and `|`.
fn commands(text: &str) -> Vec<Vec<String>> {
    let mut joined = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if current.is_empty() && line.trim_start().starts_with('#') {
            continue;
        }
        match line.trim_end().strip_suffix('\\') {
            Some(rest) => {
                current.push_str(rest);
                current.push(' ');
            }
            None => {
                current.push_str(line);
                joined.push(std::mem::take(&mut current));
            }
        }
    }
    let mut commands = Vec::new();
    for line in joined {
        let mut words: Vec<String> = Vec::new();
        for word in line.split_whitespace() {
            if matches!(word, "&&" | "||" | ";" | "|") {
                commands.push(std::mem::take(&mut words));
            } else {
                words.push(word.to_owned());
            }
        }
        commands.push(words);
    }
    commands
}

/// The `cargo build` / `cargo install` commands among `commands`, from the verb on.
fn cargo_builds(commands: Vec<Vec<String>>) -> Vec<Vec<String>> {
    commands
        .into_iter()
        .filter_map(|words| {
            let at = words.windows(2).position(|pair| {
                pair[0] == "cargo" && matches!(pair[1].as_str(), "build" | "install")
            })?;
            Some(words[at..].to_vec())
        })
        .collect()
}

fn is_release(words: &[String]) -> bool {
    words.iter().enumerate().any(|(at, word)| {
        matches!(word.as_str(), "--release" | "-r" | "--profile=release")
            || (word == "--profile" && words.get(at + 1).is_some_and(|next| next == "release"))
    })
}

fn names_a_bin(words: &[String]) -> bool {
    words.iter().enumerate().any(|(at, word)| {
        (word == "--bin" && words.get(at + 1).is_some())
            || word
                .strip_prefix("--bin=")
                .is_some_and(|name| !name.is_empty())
    })
}

/// The packages named with `-p X`, `-pX`, `--package X` and `--package=X`.
fn packages(words: &[String]) -> Vec<&str> {
    let mut found = Vec::new();
    for (at, word) in words.iter().enumerate() {
        if matches!(word.as_str(), "-p" | "--package") {
            found.extend(words.get(at + 1).map(String::as_str));
        } else if let Some(name) = word.strip_prefix("--package=") {
            found.push(name);
        } else if let Some(name) = word
            .strip_prefix("-p")
            .filter(|name| !name.is_empty() && !name.starts_with('-'))
        {
            found.push(name);
        }
    }
    found
}

fn whole_workspace(words: &[String]) -> bool {
    words
        .iter()
        .any(|word| matches!(word.as_str(), "--workspace" | "--all"))
}

/// The release builds in `text`, and those of them that break the rule: the first reading of
/// the rule, a package named with `-p` and no `--bin`, kept for the red run.
fn release_builds(text: &str) -> (Vec<String>, Vec<String>) {
    let _ = (names_a_bin, packages, whole_workspace);
    let all: Vec<String> = cargo_builds(commands(text))
        .into_iter()
        .filter(|words| is_release(words))
        .map(|words| words.join(" "))
        .collect();
    let broken = all
        .iter()
        .filter(|b| b.contains("-p ") && !b.contains("-p xtask") && !b.contains("--bin "))
        .cloned()
        .collect();
    (all, broken)
}

#[test]
fn every_release_build_in_the_workflows_names_its_binary() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(WORKFLOWS);
    let (mut all, mut broken) = (Vec::new(), Vec::new());
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml")
        {
            let (found, bad) = release_builds(&std::fs::read_to_string(&path).unwrap());
            all.extend(found);
            broken.extend(
                bad.into_iter()
                    .map(|line| format!("{}: {line}", path.display())),
            );
        }
    }
    assert!(
        broken.is_empty(),
        "release builds without --bin: {broken:#?}"
    );
    // The agent package and the release workflow at least.
    assert!(all.len() >= 2, "{all:?}");
    assert!(
        all.iter()
            .any(|b| b.contains("js-agent") && b.contains("--bin js-agent"))
    );
}

fn broken(text: &str) -> usize {
    release_builds(text).1.len()
}

#[test]
fn a_build_that_names_its_binary_passes_in_either_spelling() {
    for text in [
        "cargo build --release -p js-agent --bin js-agent",
        "cargo build --release -p js-agent --bin=js-agent",
        "run: cargo build --locked --release \\\n  --target x \\\n  -p js-agent --bin js-agent",
        "cargo build -p xtask --locked --release",
        "cargo build --release -p \"$package\" --bin \"$bin\"",
        "cargo build -p js-agent\ncargo test --release -p js-agent",
    ] {
        assert_eq!(broken(text), 0, "{text}");
    }
}

#[test]
fn a_release_build_without_a_binary_name_is_found_however_it_is_spelt() {
    for text in [
        "cargo build --release -p js-agent",
        "cargo build --release",
        "cargo build --release --workspace",
        "cargo build --release --all",
        "cargo build --release --package js-agent",
        "cargo build --release --package=js-agent",
        "cargo build --release -pjs-agent",
        "cargo build -r -p js-agent",
        "cargo build --profile release -p js-agent",
        "cargo build --profile=release -p js-agent",
        "cargo build --release -p js-agent -p xtask",
        "cargo build --release -p xtask --workspace",
        "cargo install --release --path crates/js-agent",
        "cargo build --locked \\\n  --release \\\n  -p js-agent",
        "run: make && cargo build --release -p js-agent",
        "cargo build --release -p js-agent --bin",
    ] {
        assert_eq!(broken(text), 1, "{text}");
    }
}

#[test]
fn comments_and_builds_that_are_not_for_release_are_not_selected() {
    let text =
        "# cargo build --release -p js-agent\ncargo build -p js-agent\ncargo test --release\n";
    assert_eq!(release_builds(text).0.len(), 0);
}
