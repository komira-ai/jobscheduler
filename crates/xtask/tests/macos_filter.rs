//! The macos job's filter in `.github/workflows/ci.yml` (the `changes` job). The script of
//! its `filter` step is taken from the workflow text and run by `bash` against small git
//! repositories, so each trigger is pinned by what it does: a push to main always runs the
//! job; a pull request runs it only when it carries the label `macos`, changes a path that
//! names darwin, apple, macos or seatbelt (any case), a `js-mac-*` crate or ci.yml, or
//! adds or removes a `target_os = "macos"` or `target_vendor = "apple"` line; and a diff that cannot be computed runs it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml");
    std::fs::read_to_string(path).expect("read ci.yml")
}

/// The `run: |` script of the step `id: filter`, with its indentation removed.
fn script() -> String {
    let text = workflow();
    let mut lines = text.lines().skip_while(|l| l.trim() != "- id: filter");
    lines.next().expect("no filter step in ci.yml");
    let mut lines = lines.skip_while(|l| l.trim() != "run: |");
    let header = lines.next().expect("filter step has no run block");
    let indent = header.len() - header.trim_start().len() + 2;
    let pad = " ".repeat(indent);
    let body: Vec<&str> = lines
        .take_while(|l| l.is_empty() || l.starts_with(&pad))
        .map(|l| l.get(indent..).unwrap_or(""))
        .collect();
    assert!(body.len() > 10, "filter script too short: {body:?}");
    body.join("\n") + "\n"
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

/// A repository with a base commit (also `origin/main`, the remote ref the script diffs
/// against) and a head commit that writes `changes`, as (path, content) pairs.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str, changes: &[(&str, &str)]) -> Fixture {
        let dir = std::env::temp_dir().join(format!("macos-filter-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture");
        git(&dir, &["init", "-q", "-b", "main"]);
        write(
            &dir,
            "crates/js-agent/src/lib.rs",
            "fn a() {}\n// removed\n#[cfg(target_os = \"macos\")]\nfn old() {}\n",
        );
        write(&dir, "README.md", "x\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "base"]);
        git(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&dir, &["checkout", "-q", "-b", "pr"]);
        for (path, content) in changes {
            write(&dir, path, content);
        }
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "head"]);
        Fixture { dir }
    }

    /// Runs the script and returns the value of `macos=` it wrote.
    fn run(&self, event: &str, base: &str, labeled: &str) -> String {
        let out = self.dir.join("github_output");
        let _ = std::fs::remove_file(&out);
        let status = Command::new("bash")
            .arg("-c")
            .arg(script())
            .current_dir(&self.dir)
            .env("EVENT", event)
            .env("BASE", base)
            .env("LABELED", labeled)
            .env("GITHUB_OUTPUT", &out)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("run bash");
        assert!(status.success(), "the script failed");
        let text = std::fs::read_to_string(&out).expect("script wrote no output");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines[0]
            .strip_prefix("macos=")
            .expect("macos= line")
            .to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

/// Runs a pull request that writes `changes` and returns the filter's answer.
fn pr(name: &str, changes: &[(&str, &str)]) -> String {
    Fixture::new(name, changes).run("pull_request", "main", "false")
}

#[test]
fn a_push_to_main_always_runs_the_job() {
    let f = Fixture::new("push", &[("README.md", "y\n")]);
    assert_eq!(f.run("push", "", "false"), "true");
}

#[test]
fn a_pull_request_that_touches_no_macos_code_skips_the_job() {
    let changes: [(&str, &str); 5] = [
        ("README.md", "y\n"),
        (
            "crates/js-agent/src/other.rs",
            "fn b() {}\n#[cfg(target_os = \"linux\")]\nfn c() {}\n",
        ),
        ("crates/js-cli/src/main.rs", "fn main() {}\n"),
        ("Cargo.lock", "# lock\n"),
        ("Cargo.toml", "# manifest\n"),
    ];
    assert_eq!(pr("skip", &changes), "false");
}

#[test]
fn a_changed_path_naming_darwin_apple_macos_or_seatbelt_runs_the_job() {
    for (i, path) in [
        "docs/darwin-notes.md",
        "crates/js-agent/src/Darwin.rs",
        "crates/x/aarch64-apple-thing.txt",
        "profiles/seatbelt.sb",
        "profiles/SEATBELT.sb",
        "docs/MacOS.md",
        ".github/workflows/macos-probe.yml",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(pr(&format!("path{i}"), &[(path, "y\n")]), "true", "{path}");
    }
}

#[test]
fn a_change_under_a_js_mac_crate_runs_the_job() {
    assert_eq!(
        pr("mac", &[("crates/js-mac-new/src/lib.rs", "fn a() {}\n")]),
        "true"
    );
    assert_eq!(
        pr("mac2", &[("crates/js-mac-session/Cargo.toml", "# c\n")]),
        "true"
    );
    // A name that only starts like it does not count.
    assert_eq!(
        pr("notmac", &[("crates/js-machine/src/lib.rs", "fn a() {}\n")]),
        "false"
    );
}

#[test]
fn a_change_to_the_toolchain_pin_runs_the_job_and_one_to_the_lockfile_does_not() {
    assert_eq!(pr("pin", &[("rust-toolchain.toml", "# pin\n")]), "true");
    assert_eq!(pr("lock", &[("Cargo.lock", "# lock\n")]), "false");
}

#[test]
fn a_change_to_the_workflow_that_defines_the_job_runs_it() {
    assert_eq!(pr("ci", &[(".github/workflows/ci.yml", "# c\n")]), "true");
    assert_eq!(
        pr("rel", &[(".github/workflows/release.yml", "# c\n")]),
        "false"
    );
}

#[test]
fn an_added_or_removed_target_os_macos_line_runs_the_job() {
    let spellings = [
        "#[cfg(target_os = \"macos\")]\n",
        "#[cfg(target_os=\"macos\")]\n",
        "#[cfg(any(unix, target_os = \"macos\"))]\n",
        "if cfg!(target_os = \"macos\") {}\n",
        "#[cfg(not(target_os = \"macos\"))]\n",
        "#[cfg(target_vendor = \"apple\")]\n",
        "#[cfg(target_vendor=\"apple\")]\n",
        "#[cfg(any(unix, target_vendor  =  \"apple\"))]\n",
    ];
    for (i, line) in spellings.iter().enumerate() {
        let content = format!("fn a() {{}}\n{line}");
        assert_eq!(
            pr(
                &format!("add{i}"),
                &[("crates/js-agent/src/other.rs", &content)]
            ),
            "true",
            "{line}"
        );
    }
    // Other platform words stay out: unix, another vendor, linux.
    for (i, line) in [
        "#[cfg(unix)]\n",
        "#[cfg(target_vendor = \"unknown\")]\n",
        "#[cfg(target_os = \"linux\")]\n",
    ]
    .iter()
    .enumerate()
    {
        let content = format!("fn a() {{}}\n{line}");
        assert_eq!(
            pr(
                &format!("neg{i}"),
                &[("crates/js-agent/src/other.rs", &content)]
            ),
            "false",
            "{line}"
        );
    }
    // Removing the line the base has counts; a change next to an unchanged cfg line does
    // not.
    let removed = "fn a() {}\n// removed\n";
    assert_eq!(pr("rm", &[("crates/js-agent/src/lib.rs", removed)]), "true");
    let edited = "fn a() {}\n// removed\n#[cfg(target_os = \"macos\")]\nfn renamed() {}\n";
    assert_eq!(
        pr("edit", &[("crates/js-agent/src/lib.rs", edited)]),
        "false"
    );
}

#[test]
fn the_label_macos_runs_the_job_whatever_the_diff() {
    let f = Fixture::new("label", &[("README.md", "y\n")]);
    assert_eq!(f.run("pull_request", "main", "false"), "false");
    assert_eq!(f.run("pull_request", "main", "true"), "true");
}

#[test]
fn a_diff_that_cannot_be_computed_runs_the_job() {
    let f = Fixture::new("closed", &[("README.md", "y\n")]);
    assert_eq!(f.run("pull_request", "no-such-branch", "false"), "true");
}

#[test]
fn a_base_with_no_merge_base_runs_the_job() {
    let f = Fixture::new("orphan", &[("README.md", "y\n")]);
    git(&f.dir, &["checkout", "-q", "--orphan", "unrelated"]);
    write(&f.dir, "other.txt", "o\n");
    git(&f.dir, &["add", "other.txt"]);
    git(&f.dir, &["commit", "-q", "-m", "unrelated"]);
    git(
        &f.dir,
        &["update-ref", "refs/remotes/origin/unrelated", "HEAD"],
    );
    git(&f.dir, &["checkout", "-q", "pr"]);
    assert_eq!(f.run("pull_request", "unrelated", "false"), "true");
}

#[test]
fn a_label_alone_starts_no_run_and_is_read_at_push_and_reopen() {
    let text = workflow();
    // No `labeled` event: a label-only run would put skipped checks under the real run's
    // names. The label is read from the pull request at the events that remain.
    assert!(
        text.contains("types: [opened, synchronize, reopened]\n"),
        "pull_request types"
    );
    assert!(!text.contains("labeled]") && !text.contains(", labeled"));
    // And no per-job skip for label runs, nor a special concurrency group for them.
    assert!(
        !text.contains("github.event.action"),
        "no event-action test"
    );
    assert!(!text.contains("github.event.label"), "no label-event test");
}

#[test]
fn the_workflow_wires_the_label_the_event_and_the_job_condition() {
    let text = workflow();
    assert!(
        text.contains("types: [opened, synchronize, reopened]"),
        "the events that start a pull request run"
    );
    assert!(
        text.contains("LABELED: ${{ contains(github.event.pull_request.labels.*.name, 'macos') }}"),
        "the label must reach the filter"
    );
    assert!(
        text.contains("if: ${{ !cancelled() && (github.event_name != 'pull_request' || needs.changes.result != 'success' || needs.changes.outputs.macos == 'true') }}"),
        "the macos job must run on main, on a failed filter and on the filter's yes"
    );
}
