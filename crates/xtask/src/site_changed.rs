//! `site-changed --event <name> --base <ref> [--group-base <commit>] --paths <file>
//! --github-output <file>`: whether the site steps of the `lint` job of
//! `.github/workflows/ci.yml` run.
//!
//! Appends `run=true` or `run=false` to `--github-output` (and prints it):
//!
//! - a pull request: `run=true` when a file changed between the merge base with
//!   `origin/<base>` and `HEAD` matches an entry of `--paths` (`site/ci-paths`), else
//!   `run=false`;
//! - a merge group of the merge queue: the same, for the files changed between the merge base
//!   with `--group-base` (the group's `base_sha`, the commit it is built on) and `HEAD`. The
//!   group base must be a full commit id (40 or 64 lowercase hex digits), so that nothing else
//!   reaches git as a revision or an option;
//! - any other event (a push to main): `run=true`.
//!
//! A rename counts as both paths. An entry of the list is a directory prefix ending in `/`
//! (every file under it) or an exact file path; lines starting with `#` and blank lines are
//! ignored, and blanks around an entry are trimmed.
//!
//! Fails closed: when the diff cannot be computed, a merge group's base is not a full commit id,
//! or the list is missing or has no entries, the answer is `run=true`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// An event whose changed files decide, read against its base branch.
const PULL_REQUEST: &str = "pull_request";

/// An event whose changed files decide, read against the group's base commit.
const MERGE_GROUP: &str = "merge_group";

/// The arguments of `site-changed`.
#[derive(clap::Args)]
pub struct Args {
    /// The workflow's event name (`github.event_name`).
    #[arg(long)]
    pub event: String,
    /// The branch a pull request targets (`github.base_ref`); its remote ref is
    /// `origin/<base>`.
    #[arg(long)]
    pub base: String,
    /// The commit a merge group is built on (`github.event.merge_group.base_sha`); read for a
    /// merge group alone, empty for every other event.
    #[arg(long, default_value = "")]
    pub group_base: String,
    /// The list of paths whose change runs the site steps.
    #[arg(long)]
    pub paths: PathBuf,
    /// The file the answer is appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub github_output: PathBuf,
    /// The checkout the diff is taken in.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
    /// The `git` program.
    #[arg(long, default_value = "git")]
    pub git: PathBuf,
}

/// Why the answer could not be written.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The output file could not be appended to.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The output file.
        path: PathBuf,
        /// Why it could not be written.
        #[source]
        source: std::io::Error,
    },
}

/// The entries of a list: its lines without comments and blank lines, trimmed.
#[must_use]
fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Whether `entry` matches the `/`-separated `file`: a directory prefix ending in `/` matches
/// every file under it, anything else matches that file only.
#[must_use]
fn matches(entry: &str, file: &str) -> bool {
    if entry.ends_with('/') {
        file.starts_with(entry)
    } else {
        file == entry
    }
}

/// Whether `text` is a full commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[must_use]
fn is_commit_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The revision whose merge base with `HEAD` the changed files of `event` are read from:
/// `origin/<base>` for a pull request, the group base for a merge group whose base is a full
/// commit id; `None` for any other event, or a merge group without a usable base.
#[must_use]
fn since(event: &str, base: &str, group_base: &str) -> Option<String> {
    match event {
        PULL_REQUEST => Some(format!("origin/{base}")),
        MERGE_GROUP => is_commit_id(group_base).then(|| group_base.to_owned()),
        _ => None,
    }
}

/// Whether the site steps run: when the list or the changed files are unavailable (`entries`
/// empty, `changed` `None`: an event whose changes are not read, or fail closed), or when a
/// changed file matches an entry.
#[must_use]
fn decide(entries: &[String], changed: Option<&[String]>) -> bool {
    if entries.is_empty() {
        return true;
    }
    changed.is_none_or(|files| {
        files
            .iter()
            .any(|file| entries.iter().any(|entry| matches(entry, file)))
    })
}

/// The files changed between the merge base of the revision `since` and `HEAD` and `HEAD`,
/// renames as both paths and names unquoted, taken with `git` in `repo_dir`; `None` when git
/// does not run or fails.
#[must_use]
pub fn changed_files(git: &Path, repo_dir: &Path, since: &str) -> Option<Vec<String>> {
    let out = Command::new(git)
        .current_dir(repo_dir)
        .args(["diff", "--no-renames", "--name-only", "-z"])
        .arg(format!("{since}...HEAD"))
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// Takes the decision for `args`, appends `run=<bool>` to `--github-output` and returns that
/// line.
///
/// # Errors
/// [`Error::Write`] when the output file cannot be appended to.
pub fn run(args: &Args) -> Result<String, Error> {
    let entries = std::fs::read_to_string(&args.paths)
        .map(|text| parse_list(&text))
        .unwrap_or_default();
    // The diff is not taken when the event or the list already decides.
    let changed = match since(&args.event, &args.base, &args.group_base) {
        Some(since) if !entries.is_empty() => changed_files(&args.git, &args.repo_dir, &since),
        _ => None,
    };
    let line = format!("run={}", decide(&entries, changed.as_deref()));
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.github_output)
        .and_then(|mut file| writeln!(file, "{line}"));
    write.map_err(|source| Error::Write {
        path: args.github_output.clone(),
        source,
    })?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Asserts that comments, blank and blank-only lines are dropped and blanks around an
    /// entry are trimmed.
    #[test]
    fn a_list_ignores_comments_and_blank_lines_and_trims_entries() {
        assert_eq!(
            parse_list("# c\n\n  \nCargo.toml   \n\t docs/\n  # indented\n"),
            list(&["Cargo.toml", "docs/"])
        );
        assert_eq!(parse_list("# nothing\n\n"), Vec::<String>::new());
        assert_eq!(parse_list(""), Vec::<String>::new());
    }

    /// Asserts that a directory entry matches only paths under it, and a file entry only
    /// that file.
    #[test]
    fn an_entry_matches_a_directory_prefix_or_an_exact_file() {
        assert!(matches("docs/", "docs/a/b.md"));
        assert!(!matches("docs/", "docs-old/a.md"));
        assert!(!matches("docs/", "docs"));
        assert!(matches("README.md", "README.md"));
        assert!(!matches("README.md", "README.mdx"));
        assert!(!matches("README.md", "docs/README.md"));
    }

    /// Asserts the decision for each list and set of changed files.
    #[test]
    fn the_decision_follows_the_list_and_the_changed_files() {
        let entries = list(&["site/", "README.md"]);
        let some = list(&["crates/js-core/src/lib.rs"]);
        // A match runs, no match skips, also with no files at all.
        assert!(decide(&entries, Some(&list(&["a.rs", "site/a.html"]))));
        assert!(decide(&entries, Some(&list(&["README.md"]))));
        assert!(!decide(&entries, Some(&some)));
        assert!(!decide(&entries, Some(&[])));
        // No changed files read (a push, or fail closed), or no list: always.
        assert!(decide(&entries, None));
        assert!(decide(&[], Some(&some)));
        assert!(decide(&[], None));
    }

    /// Asserts which revision each event's changes are read from: a pull request's base
    /// branch, a merge group's base commit when it is a full commit id, and none otherwise.
    #[test]
    fn each_event_reads_its_changes_from_its_own_base() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            since(PULL_REQUEST, "main", sha1).as_deref(),
            Some("origin/main")
        );
        assert_eq!(since(MERGE_GROUP, "", sha1).as_deref(), Some(sha1));
        assert_eq!(since(MERGE_GROUP, "main", sha256).as_deref(), Some(sha256));
        for event in [
            "push",
            "workflow_dispatch",
            "pull_request_target",
            "Merge_Group",
            "",
        ] {
            assert_eq!(since(event, "main", sha1), None, "{event}");
        }
        // A group base that is not a full commit id is never handed to git.
        for base in [
            "",
            "main",
            "HEAD^1",
            "--output=x",
            &sha1[..39],
            &format!("{sha1}0"),
            &sha1.to_uppercase(),
            &sha1.replace('a', "g"),
            &format!("{}~1", &sha1[..38]),
            &format!("{sha256}0"),
        ] {
            assert_eq!(since(MERGE_GROUP, "main", base), None, "{base}");
        }
    }
}
