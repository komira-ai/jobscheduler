//! `site-changed` against real git repositories (no network): each test is a pull request
//! with a base on `origin/main` and a head, and the answer the site steps get.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_testkit::dir::TestDir;
use xtask::site_changed::{Args, Error, changed_files, run};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// A repository on `main` with `origin/main` at its first commit, and a branch `pr`.
struct Repo {
    dir: TestDir,
}

impl Repo {
    fn new(name: &str, list: Option<&str>) -> Repo {
        let repo = Repo {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), name),
        };
        repo.git(&["init", "--quiet", "-b", "main"]);
        repo.write("README.md", "x\n");
        repo.write("crates/js-sched/src/lib.rs", "fn a() {}\n");
        if let Some(text) = list {
            repo.write("site/ci-paths", text);
        }
        repo.commit("base");
        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(&["checkout", "--quiet", "-b", "pr"]);
        repo
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "user.name=t"])
            .args(["-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(&*self.dir)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
    }

    /// Commits the head of the pull request: `changed` files written.
    fn change(&self, changed: &[&str]) {
        for file in changed {
            self.write(file, &format!("changed {file}\n"));
        }
        self.commit("head");
    }

    fn args(&self, event: &str, base: &str) -> Args {
        Args {
            event: event.to_owned(),
            base: base.to_owned(),
            paths: self.dir.join("site/ci-paths"),
            github_output: self.dir.join("github_output"),
            repo_dir: self.dir.to_path_buf(),
            git: PathBuf::from("git"),
        }
    }

    /// The answer for a pull request against `main`.
    fn decide(&self) -> String {
        self.decide_with(&self.args("pull_request", "main"))
    }

    /// The answer for `args`, which must also be the one line written to the output file.
    fn decide_with(&self, args: &Args) -> String {
        let said = run(args).expect("run");
        let written = std::fs::read_to_string(&args.github_output).expect("output");
        assert_eq!(written, format!("{said}\n"));
        said
    }
}

const REAL: &str = "# paths\n\nsite/\ndocs/\nREADME.md\nCargo.toml\ncrates/js-sched/src/\n.github/workflows/ci.yml\nLICENSE\n";

/// Asserts that a push runs the site steps, with or without changes.
#[test]
fn a_push_always_runs_the_site_steps() {
    let repo = Repo::new("sc-push", Some(REAL));
    repo.change(&["crates/js-core/src/lib.rs"]);
    assert_eq!(repo.decide_with(&repo.args("push", "")), "run=true");
    let none = Repo::new("sc-push-none", Some(REAL));
    none.change(&[]);
    assert_eq!(none.decide_with(&none.args("push", "main")), "run=true");
}

/// Asserts that a file under the site or any listed directory runs the steps.
#[test]
fn a_pull_request_changing_a_listed_directory_runs_the_site_steps() {
    for (i, file) in [
        "site/content/index.html",
        "docs/design/placement-by-load.md",
        "crates/js-sched/src/lib.rs",
    ]
    .iter()
    .enumerate()
    {
        let repo = Repo::new(&format!("sc-dir-{i}"), Some(REAL));
        repo.change(&[file]);
        assert_eq!(repo.decide(), "run=true", "{file}");
    }
}

/// Asserts that a listed file runs the steps, also among unrelated changes.
#[test]
fn a_pull_request_changing_a_listed_file_runs_the_site_steps() {
    let repo = Repo::new("sc-file", Some(REAL));
    repo.change(&["README.md"]);
    assert_eq!(repo.decide(), "run=true");
    let mixed = Repo::new("sc-mixed", Some(REAL));
    mixed.change(&["crates/js-registry/tests/a.rs", "LICENSE"]);
    assert_eq!(mixed.decide(), "run=true");
}

/// Asserts that a rename is both paths, so moving a listed file away runs the steps, and
/// that a deletion counts too.
#[test]
fn a_rename_counts_as_both_paths() {
    let repo = Repo::new("sc-rename", Some(REAL));
    repo.git(&["mv", "README.md", "crates/js-core-README.md"]);
    repo.commit("move");
    assert_eq!(repo.decide(), "run=true");
    let gone = Repo::new("sc-delete", Some(REAL));
    gone.git(&["rm", "--quiet", "README.md"]);
    gone.commit("delete");
    assert_eq!(gone.decide(), "run=true");
}

/// Asserts that unrelated changes, and no change, skip the steps.
#[test]
fn a_pull_request_changing_only_unrelated_files_skips_the_site_steps() {
    let repo = Repo::new("sc-skip", Some(REAL));
    repo.change(&["crates/js-sched/tests/a.rs", "crates/js-core/src/lib.rs"]);
    assert_eq!(repo.decide(), "run=false");
    let lock = Repo::new("sc-lock", Some(REAL));
    lock.change(&["Cargo.lock"]);
    assert_eq!(lock.decide(), "run=false");
    let empty = Repo::new("sc-empty", Some(REAL));
    empty.change(&[]);
    assert_eq!(empty.decide(), "run=false");
}

/// Asserts that a path sharing only a prefix with a directory entry or a file entry skips.
#[test]
fn a_path_sharing_only_a_prefix_does_not_run_the_site_steps() {
    let repo = Repo::new("sc-prefix-dir", Some(REAL));
    repo.change(&[
        "docs-old/a.md",
        "site-extra/a.md",
        "crates/js-sched/srcs/a.rs",
    ]);
    assert_eq!(repo.decide(), "run=false");
    let file = Repo::new("sc-prefix-file", Some(REAL));
    file.change(&["README.mdx", "LICENSE-APACHE"]);
    assert_eq!(file.decide(), "run=false");
}

/// Asserts that comments, blank lines and trailing blanks of the list do not change what
/// matches: a comment is not an entry, an entry with trailing blanks still matches, and the
/// list read is the one given.
#[test]
fn the_list_ignores_comments_and_blank_lines_and_trailing_blanks() {
    let repo = Repo::new("sc-comment", Some("# nope\n\n  \nCargo.toml   \n"));
    repo.change(&["# nope"]);
    assert_eq!(repo.decide(), "run=false");
    let trailing = Repo::new("sc-trailing", Some("# c\n\nCargo.toml   \n"));
    trailing.change(&["Cargo.toml"]);
    assert_eq!(trailing.decide(), "run=true");
    let own = Repo::new("sc-own-list", Some("docs/\n"));
    own.change(&["site/a.html"]);
    assert_eq!(own.decide(), "run=false");
}

/// Asserts the fail-closed answer when the base ref does not exist or git does not run.
#[test]
fn a_diff_that_cannot_be_computed_runs_the_site_steps() {
    let repo = Repo::new("sc-no-base", Some(REAL));
    repo.change(&["crates/js-core/src/lib.rs"]);
    assert_eq!(
        repo.decide_with(&repo.args("pull_request", "gone")),
        "run=true"
    );
    let mut missing_git = repo.args("pull_request", "main");
    missing_git.git = repo.dir.join("no-such-git");
    missing_git.github_output = repo.dir.join("second_output");
    assert_eq!(repo.decide_with(&missing_git), "run=true");
}

/// Asserts the fail-closed answer when the list is missing or has no entries, even for
/// changes that would otherwise skip.
#[test]
fn a_list_that_is_missing_or_empty_runs_the_site_steps() {
    let missing = Repo::new("sc-no-list", None);
    missing.change(&["crates/js-core/src/lib.rs"]);
    assert_eq!(missing.decide(), "run=true");
    let empty = Repo::new("sc-empty-list", Some("# nothing\n\n"));
    empty.change(&["crates/js-core/src/lib.rs"]);
    assert_eq!(empty.decide(), "run=true");
}

/// Asserts the git seam: names come back unquoted, one per path, from a merge-base diff that
/// leaves out what the base gained since, and a missing ref or program gives none.
#[test]
fn changed_files_lists_unquoted_names_since_the_merge_base() {
    let repo = Repo::new("sc-seam", Some(REAL));
    repo.change(&["caf\u{e9}/\u{4e2d}.md", "a b.txt"]);
    let git = Path::new("git");
    let mut names = changed_files(git, &repo.dir, "main").expect("diff");
    names.sort();
    assert_eq!(names, ["a b.txt", "caf\u{e9}/\u{4e2d}.md"]);
    // The base moves on after the branch was made: its own new file is not a change.
    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("later.txt", "later\n");
    repo.commit("later");
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    repo.git(&["checkout", "--quiet", "pr"]);
    let again = changed_files(git, &repo.dir, "main").expect("diff");
    assert!(!again.contains(&"later.txt".to_owned()), "{again:?}");
    assert_eq!(changed_files(git, &repo.dir, "gone"), None);
    assert_eq!(
        changed_files(Path::new("no-such-git-program"), &repo.dir, "main"),
        None
    );
}

/// Asserts that an output file that cannot be appended to is an error naming it.
#[test]
fn an_output_file_that_cannot_be_written_is_an_error() {
    let repo = Repo::new("sc-bad-output", Some(REAL));
    let mut args = repo.args("push", "main");
    args.github_output = repo.dir.to_path_buf();
    match run(&args) {
        Err(Error::Write { path, why }) => {
            assert_eq!(path, repo.dir.display().to_string());
            assert!(!why.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

fn xtask(repo: &Repo, args: &[&str]) -> Output {
    Command::new(XTASK)
        .arg("site-changed")
        .args(args)
        .current_dir(&*repo.dir)
        .output()
        .expect("xtask")
}

/// Asserts the command line: the answer is printed and appended to the output file, and a
/// failed write exits 1 with the reason on stderr.
#[test]
fn the_command_line_writes_the_answer_and_fails_on_an_unwritable_output() {
    let repo = Repo::new("sc-cli", Some(REAL));
    repo.change(&["crates/js-core/src/lib.rs"]);
    let out = xtask(
        &repo,
        &[
            "--event",
            "pull_request",
            "--base",
            "main",
            "--paths",
            "site/ci-paths",
            "--github-output",
            "out.txt",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "run=false\n");
    assert_eq!(
        std::fs::read_to_string(repo.dir.join("out.txt")).expect("out"),
        "run=false\n"
    );
    let bad = xtask(
        &repo,
        &[
            "--event",
            "push",
            "--base",
            "",
            "--paths",
            "site/ci-paths",
            "--github-output",
            ".",
        ],
    );
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).starts_with("xtask: cannot write ."));
}
