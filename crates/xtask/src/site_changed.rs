//! `site-changed --event <name> --base <ref> --paths <file> --github-output <file>`: whether the
//! site steps of the `lint` job of `.github/workflows/ci.yml` run.
//!
//! Appends `run=true` or `run=false` to `--github-output` (and prints it):
//!
//! - any event other than `pull_request` (a push to main): `run=true`;
//! - a pull request: `run=true` when a file changed between the merge base with
//!   `origin/<base>` and `HEAD` matches an entry of `--paths` (`site/ci-paths`), else
//!   `run=false`. A rename counts as both paths.
//!
//! An entry of the list is a directory prefix ending in `/` (every file under it) or an exact
//! file path; lines starting with `#` and blank lines are ignored, and blanks around an entry
//! are trimmed.
//!
//! Fails closed: when the diff cannot be computed, or the list is missing or has no entries,
//! the answer is `run=true`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The event whose changed files decide; every other event runs the site steps.
const PULL_REQUEST: &str = "pull_request";

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

/// Whether the site steps run: always off a pull request; on one, when the list or the diff
/// is unavailable (`entries` empty, `changed` `None`: fail closed) or a changed file matches
/// an entry.
#[must_use]
fn decide(event: &str, entries: &[String], changed: Option<&[String]>) -> bool {
    if event != PULL_REQUEST || entries.is_empty() {
        return true;
    }
    changed.is_none_or(|files| {
        files
            .iter()
            .any(|file| entries.iter().any(|entry| matches(entry, file)))
    })
}

/// The files changed between the merge base of `origin/<base>` and `HEAD` and `HEAD`, renames
/// as both paths and names unquoted, taken with `git` in `repo_dir`; `None` when git does not
/// run or fails.
#[must_use]
pub fn changed_files(git: &Path, repo_dir: &Path, base: &str) -> Option<Vec<String>> {
    let out = Command::new(git)
        .current_dir(repo_dir)
        .args(["diff", "--no-renames", "--name-only", "-z"])
        .arg(format!("origin/{base}...HEAD"))
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
    let changed = if args.event == PULL_REQUEST && !entries.is_empty() {
        changed_files(&args.git, &args.repo_dir, &args.base)
    } else {
        None
    };
    let line = format!("run={}", decide(&args.event, &entries, changed.as_deref()));
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

    /// Asserts the decision for each event, list and diff.
    #[test]
    fn the_decision_follows_the_event_the_list_and_the_changed_files() {
        let entries = list(&["site/", "README.md"]);
        let some = list(&["crates/js-core/src/lib.rs"]);
        // Not a pull request: always.
        assert!(decide("push", &entries, Some(&some)));
        assert!(decide("push", &[], None));
        // A pull request: a match runs, no match skips, also with no files at all.
        assert!(decide(
            PULL_REQUEST,
            &entries,
            Some(&list(&["a.rs", "site/a.html"]))
        ));
        assert!(decide(PULL_REQUEST, &entries, Some(&list(&["README.md"]))));
        assert!(!decide(PULL_REQUEST, &entries, Some(&some)));
        assert!(!decide(PULL_REQUEST, &entries, Some(&[])));
        // Fail closed.
        assert!(decide(PULL_REQUEST, &entries, None));
        assert!(decide(PULL_REQUEST, &[], Some(&some)));
    }
}
