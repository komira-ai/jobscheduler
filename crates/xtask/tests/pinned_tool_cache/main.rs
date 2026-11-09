//! The workflow lint of pinned tool downloads, over every workflow in `.github/workflows`,
//! read as text.
//!
//! A pinned tool is downloaded only by `cargo xtask fetch`: a `run:` that names a GitHub
//! release download (`releases/download/`) and is not a fetch is refused. Every fetch is
//! one command with no `if:`, whose `--sha256` is one variable, whose `--url` names its
//! version by variable, and whose `--cache` is `"$RUNNER_TEMP/<file>"`, a path no other
//! fetch of its job uses. Right around it are
//!   - before it, `actions/cache/restore` pinned by commit, with an `id:`, no `if:`, and in
//!     its `with:` exactly the fetch's cache path (`${{ runner.temp }}/<file>`) and a key
//!     whose only expressions are `env.<V>` for each variable of the URL and for the
//!     `--sha256` variable: a new version or a new pin is a new key, and no `restore-keys`
//!     can hand back another version's archive;
//!   - after it, `actions/cache/save` pinned by commit, with `if:` exactly
//!     `steps.<id>.outputs.cache-hit != 'true'`, whose implicit `success()` lets it run only
//!     once the fetch passed, and in its `with:` the same path and
//!     `${{ steps.<id>.outputs.cache-primary-key }}`.
//!
//! Fetch itself (src/fetch.rs, tests/fetch.rs) checks the SHA-256 of whichever archive it
//! uses, downloads only when no file is at `--cache`, and keeps a download there only after
//! it matched; so this rule and those tests together make every job use the actions cache
//! when it can, and only a checked archive ever reach it.

mod lint;

use lint::{Site, jobs, lint};
use std::path::Path;

/// Every pinned tool fetch of the repository: `<file>/<job>` and its `--cache` path.
const EXPECTED: [(&str, &str, &str); 4] = [
    ("ci.yml", "deny", "$RUNNER_TEMP/pinned/cargo-deny.tar.gz"),
    (
        "ci.yml",
        "coverage",
        "$RUNNER_TEMP/pinned/cargo-llvm-cov.tar.gz",
    ),
    (
        "ci.yml",
        "coverage",
        "$RUNNER_TEMP/pinned/cargo-nextest.tar.gz",
    ),
    (
        "js-agent-deb.yml",
        "build",
        "$RUNNER_TEMP/pinned/nfpm.tar.gz",
    ),
];

fn workflows() -> Vec<(String, String)> {
    let dir = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows"
    ));
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read .github/workflows") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a file name")
            .to_owned();
        if name.ends_with(".yml") || name.ends_with(".yaml") {
            found.push((name, std::fs::read_to_string(&path).expect("read")));
        }
    }
    found.sort();
    found
}

fn read(file: &str) -> String {
    workflows()
        .into_iter()
        .find(|(n, _)| n == file)
        .map(|(_, t)| t)
        .expect("the workflow")
}

/// Asserts that every workflow of the repository keeps the rule, and that its fetches are
/// exactly the four pinned tools in `EXPECTED`.
#[test]
fn every_pinned_tool_fetch_of_the_repository_keeps_the_rule() {
    let mut sites = Vec::new();
    for (file, text) in workflows() {
        let found = lint(&file, &text).unwrap_or_else(|e| panic!("{e:#?}"));
        sites.extend(
            found
                .into_iter()
                .map(|Site { job, cache }| (file.clone(), job, cache)),
        );
    }
    let expected: Vec<(String, String, String)> = EXPECTED
        .iter()
        .map(|(f, j, c)| ((*f).to_owned(), (*j).to_owned(), (*c).to_owned()))
        .collect();
    assert_eq!(sites, expected);
}

/// A job of three steps around one fetch, in the shape the rule asks for.
const GOOD: &str = r#"name: t
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      V: 1.0
      S: 0000000000000000000000000000000000000000000000000000000000000000
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      # The tool.
      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        id: tool-cache
        with:
          path: ${{ runner.temp }}/pinned/tool.tar.gz
          key: pinned-tool-${{ env.V }}-${{ env.S }}
      - name: Fetch the tool
        run: cargo xtask fetch --url "https://github.com/o/tool/releases/download/v$V/tool.tar.gz" --sha256 "$S" --cache "$RUNNER_TEMP/pinned/tool.tar.gz" --out "$RUNNER_TEMP/tool" --extract tar.gz --member tool
      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        if: steps.tool-cache.outputs.cache-hit != 'true'
        with:
          path: ${{ runner.temp }}/pinned/tool.tar.gz
          key: ${{ steps.tool-cache.outputs.cache-primary-key }}
      - run: "$RUNNER_TEMP/tool --version"
"#;

/// The errors of `text` (none when it passes).
fn errors(text: &str) -> Vec<String> {
    lint("t.yml", text).err().unwrap_or_default()
}

/// `GOOD` with `from` replaced by `to`, which must occur in it.
fn with(from: &str, to: &str) -> String {
    assert!(GOOD.contains(from), "{from}");
    GOOD.replacen(from, to, 1)
}

/// The line of the fetch step's `-` in `text`.
fn fetch_line(text: &str) -> usize {
    1 + text
        .lines()
        .position(|l| l == "      - name: Fetch the tool")
        .expect("the fetch")
}

/// Asserts that `text` fails with one error, on the fetch's line, that contains `says`.
#[track_caller]
fn refused(text: &str, says: &str) {
    refused_at(text, fetch_line(text), says);
}

/// Asserts that `text` fails with one error on line `line` that contains `says`.
#[track_caller]
fn refused_at(text: &str, line: usize, says: &str) {
    let errors = errors(text);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].starts_with(&format!("t.yml:{line}: build: ")) && errors[0].contains(says),
        "{says}: {errors:#?}"
    );
}

/// Asserts that the shape passes, with the pin as `${S}` or a version as `${V}` too, with
/// the save's condition wrapped in `${{ }}`, and with the job's steps parsed in order.
#[test]
fn the_three_steps_pass() {
    assert_eq!(
        lint("t.yml", GOOD),
        Ok(vec![Site {
            job: "build".to_owned(),
            cache: "$RUNNER_TEMP/pinned/tool.tar.gz".to_owned(),
        }])
    );
    assert_eq!(
        errors(&with("--sha256 \"$S\"", "--sha256 \"${S}\"")),
        Vec::<String>::new()
    );
    assert_eq!(errors(&with("/v$V/", "/v${V}/")), Vec::<String>::new());
    let wrapped = with(
        "if: steps.tool-cache.outputs.cache-hit != 'true'",
        "if: ${{ steps.tool-cache.outputs.cache-hit != 'true' }}",
    );
    assert_eq!(errors(&wrapped), Vec::<String>::new());
    let steps = &jobs(GOOD)[0].steps;
    let lines: Vec<usize> = steps.iter().map(|s| s.line).collect();
    assert_eq!(lines, [10, 12, 17, 19, 24]);
    assert_eq!(steps[1].with.len(), 2);
}

/// Asserts that a fetch that may be skipped, chains another command, or lacks a flag the
/// rule reads is refused, naming what is wrong.
#[test]
fn a_fetch_of_another_shape_is_refused() {
    let fetch = "        run: cargo xtask fetch";
    for (from, to, says) in [
        (
            "      - name: Fetch the tool\n",
            "      - name: Fetch the tool\n        if: github.event_name == 'push'\n",
            "has an `if:`",
        ),
        (
            " --member tool",
            " --member tool && echo done",
            "not one command",
        ),
        (" --member tool", " --member tool; true", "not one command"),
        (" --member tool", " --member tool > log", "not one command"),
        (
            "--sha256 \"$S\"",
            "--sha256 \"0000\"",
            "is not one variable",
        ),
        (
            "--sha256 \"$S\"",
            "--sha256 \"$S$V\"",
            "is not one variable",
        ),
        ("--sha256 \"$S\"", "", "no --sha256"),
        (
            "--url \"https://github.com/o/tool/releases/download/v$V/tool.tar.gz\"",
            "",
            "no --url",
        ),
        ("/v$V/", "/v1.0/", "names no version variable"),
        (
            " --cache \"$RUNNER_TEMP/pinned/tool.tar.gz\"",
            "",
            "no --cache",
        ),
        (
            "--cache \"$RUNNER_TEMP/pinned/tool.tar.gz\"",
            "--cache \"pinned/tool.tar.gz\"",
            "is not \"$RUNNER_TEMP/<file>\"",
        ),
        (
            "--cache \"$RUNNER_TEMP/pinned/tool.tar.gz\"",
            "--cache \"$RUNNER_TEMP/\"",
            "is not \"$RUNNER_TEMP/<file>\"",
        ),
        (
            "--cache \"$RUNNER_TEMP/pinned/tool.tar.gz\"",
            "--cache \"$RUNNER_TEMP/$V.tar.gz\"",
            "is not \"$RUNNER_TEMP/<file>\"",
        ),
    ] {
        refused(&with(from, to), says);
    }
    // A fetch alone in a block is the same one command; with another line after it, it is not.
    let block = with(
        &format!("{fetch} --url"),
        "        run: |\n          cargo xtask fetch --url",
    );
    assert_eq!(errors(&block), Vec::<String>::new());
    let two = block.replacen(
        " --member tool\n",
        " --member tool\n          echo done\n",
        1,
    );
    refused(&two, "not one command");
}

/// Asserts that a restore of another shape is refused: absent (the fetch first in its job,
/// or after another step), the whole cache action (which saves at the end of the job,
/// checked or not), unpinned, skippable, without an id, with restore-keys, at another path,
/// or keyed without the version or the pin, or with anything else that varies.
#[test]
fn a_restore_of_another_shape_is_refused() {
    let restore = "      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        id: tool-cache\n        with:\n          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: pinned-tool-${{ env.V }}-${{ env.S }}\n";
    let gone = with(restore, "");
    refused(&gone, "is not actions/cache/restore");
    let first = gone.replacen(
        "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n",
        "",
        1,
    );
    refused(&first, "no actions/cache/restore step right before");
    for (from, to, says) in [
        (
            "actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "is not actions/cache/restore",
        ),
        (
            "actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache/restore@v6",
            "is not actions/cache/restore",
        ),
        (
            "        id: tool-cache\n        with:\n          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: pinned",
            "        id: tool-cache\n        if: github.event_name == 'push'\n        with:\n          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: pinned",
            "the restore has an `if:`",
        ),
        (
            "        id: tool-cache\n        with:\n          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: pinned",
            "        with:\n          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: pinned",
            "no `id:`",
        ),
        (
            "-${{ env.S }}\n",
            "-${{ env.S }}\n          restore-keys: pinned-tool-\n",
            "not exactly path and key",
        ),
        (
            "-${{ env.S }}\n",
            "-${{ env.S }}\n          fail-on-cache-miss: true\n",
            "not exactly path and key",
        ),
        (
            "          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: pinned",
            "          path: ${{ runner.temp }}/pinned/other.tar.gz\n          key: pinned",
            "the restore's path is",
        ),
        (
            "pinned-tool-${{ env.V }}-${{ env.S }}",
            "pinned-tool-${{ env.V }}",
            "the restore's key",
        ),
        (
            "pinned-tool-${{ env.V }}-${{ env.S }}",
            "pinned-tool-${{ env.S }}",
            "the restore's key",
        ),
        (
            "pinned-tool-${{ env.V }}-${{ env.S }}",
            "pinned-tool-${{ env.V }}-${{ env.S }}-${{ github.run_id }}",
            "the restore's key",
        ),
        (
            "pinned-tool-${{ env.V }}-${{ env.S }}",
            "pinned-tool-${{ env.V }}-${{ env.S }}-$V",
            "the restore's key",
        ),
        (
            "pinned-tool-${{ env.V }}-${{ env.S }}",
            "pinned-tool-${{ env.V }}-${{ env.OTHER }}",
            "the restore's key",
        ),
    ] {
        refused(&with(from, to), says);
    }
}

/// Asserts that a save of another shape is refused: absent (the fetch last in its job, or
/// before another step), unpinned, unconditional, run whatever came before (`always()`) or
/// on a failure, of another restore's hit, at another path or under another key.
#[test]
fn a_save_of_another_shape_is_refused() {
    let save = "      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        if: steps.tool-cache.outputs.cache-hit != 'true'\n        with:\n          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: ${{ steps.tool-cache.outputs.cache-primary-key }}\n";
    let gone = with(save, "");
    refused(&gone, "is not actions/cache/save");
    let last = gone.replacen("      - run: \"$RUNNER_TEMP/tool --version\"\n", "", 1);
    refused(&last, "no actions/cache/save step right after");
    let hit = "if: steps.tool-cache.outputs.cache-hit != 'true'";
    for (from, to, says) in [
        (
            "actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache/save@v6",
            "is not actions/cache/save",
        ),
        (
            "actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9",
            "is not actions/cache/save",
        ),
        (&format!("        {hit}\n"), "", "the save's `if:` is \"\""),
        (
            hit,
            "if: always() && steps.tool-cache.outputs.cache-hit != 'true'",
            "the save's `if:`",
        ),
        (hit, "if: failure()", "the save's `if:`"),
        (
            hit,
            "if: steps.other.outputs.cache-hit != 'true'",
            "the save's `if:`",
        ),
        (
            "          path: ${{ runner.temp }}/pinned/tool.tar.gz\n          key: ${{ steps",
            "          path: ${{ runner.temp }}/pinned/other.tar.gz\n          key: ${{ steps",
            "the save's path is",
        ),
        (
            "key: ${{ steps.tool-cache.outputs.cache-primary-key }}",
            "key: pinned-tool-${{ env.V }}-${{ env.S }}",
            "the save's key is",
        ),
        (
            "key: ${{ steps.tool-cache.outputs.cache-primary-key }}",
            "key: ${{ steps.other.outputs.cache-primary-key }}",
            "the save's key is",
        ),
        (
            "key: ${{ steps.tool-cache.outputs.cache-primary-key }}\n",
            "key: ${{ steps.tool-cache.outputs.cache-primary-key }}\n          upload-chunk-size: 1\n",
            "not exactly path and key",
        ),
    ] {
        refused(&with(from, to), says);
    }
}

/// Asserts that a release download by another command is refused, and that two fetches of
/// one job may not share a cache path (a miss on the second would find the first's archive).
#[test]
fn a_download_outside_fetch_and_a_shared_cache_path_are_refused() {
    let curl = with(
        "      - run: \"$RUNNER_TEMP/tool --version\"\n",
        "      - run: curl -fsSL -o t.tgz https://github.com/o/tool/releases/download/v1/t.tgz\n",
    );
    refused_at(&curl, 24, "a release download outside `cargo xtask fetch`");
    // The three steps twice, the second copy under another id: only the path is shared.
    let three = &GOOD[GOOD
        .find("      - uses: actions/cache/restore")
        .expect("restore")
        ..GOOD.find("      - run: \"$RUNNER_TEMP").expect("last")];
    let again = three
        .replace("tool-cache", "tool2-cache")
        .replace("Fetch the tool", "Fetch the tool again");
    let twice = with(three, &format!("{three}{again}"));
    refused_at(
        &twice,
        29,
        "a second fetch with the cache path $RUNNER_TEMP/pinned/tool.tar.gz",
    );
}

/// Asserts that the repository's own workflows are refused with one line changed: ci.yml's
/// coverage job without the restore before cargo-nextest, with its llvm-cov key missing the
/// pin, or saving cargo-deny's archive on every outcome; and js-agent-deb.yml fetching nfpm
/// without the cache.
#[test]
fn the_repository_workflows_with_one_line_changed_are_refused() {
    let ci = read("ci.yml");
    let one = |text: &str, from: &str, to: &str| {
        assert_eq!(text.matches(from).count(), 1, "{from}");
        let changed = text.replacen(from, to, 1);
        assert!(lint("ci.yml", &changed).is_err(), "{from} -> {to}");
    };
    one(
        &ci,
        "        id: nextest-cache\n",
        "        id: nextest-cache\n        if: false\n",
    );
    one(
        &ci,
        "key: pinned-cargo-llvm-cov-${{ env.LLVM_COV_VERSION }}-${{ env.LLVM_COV_SHA256 }}",
        "key: pinned-cargo-llvm-cov-${{ env.LLVM_COV_VERSION }}",
    );
    one(
        &ci,
        "if: steps.deny-cache.outputs.cache-hit != 'true'",
        "if: always()",
    );
    let deb = read("js-agent-deb.yml");
    one(&deb, " --cache \"$RUNNER_TEMP/pinned/nfpm.tar.gz\"", "");
}
