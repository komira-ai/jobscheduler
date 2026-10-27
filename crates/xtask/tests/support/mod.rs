//! A git repository holding a small Cargo workspace, for the release tests.
//!
//! The workspace: `app` (a shipped crate) depends on `lib` and has `fake` as a
//! dev-dependency; `tools` depends on nothing shipped and nothing depends on it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::release::plan::PlanArgs;

/// A repository of its own in a scratch directory.
pub struct Repo {
    /// Its working tree.
    pub dir: PathBuf,
}

const MANIFEST: &str = "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2021\"\n";

impl Repo {
    /// A fresh repository named `name` with the workspace written and committed.
    pub fn new(name: &str) -> Repo {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("xtask-release")
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear scratch");
        }
        std::fs::create_dir_all(&dir).expect("scratch");
        let repo = Repo { dir };
        repo.git(&["init", "--quiet", "-b", "main"]);
        repo.write("Cargo.toml", MANIFEST);
        repo.write("Cargo.lock", "version = 4\n");
        repo.write("README.md", "docs\n");
        repo.member("app", "[dependencies]\nlib = { path = \"../lib\" }\n[dev-dependencies]\nfake = { path = \"../fake\" }\n");
        repo.member("lib", "");
        repo.member("fake", "");
        repo.member("tools", "");
        repo.commit("workspace");
        repo
    }

    /// Writes the crate `name` with `extra` after its package table.
    fn member(&self, name: &str, extra: &str) {
        self.write(
            &format!("crates/{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion.workspace = true\nedition.workspace = true\n\n{extra}"
            ),
        );
        self.write(&format!("crates/{name}/src/lib.rs"), "");
    }

    /// Writes `text` to `rel`, creating its directory.
    pub fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    /// Runs git here and returns its trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
            ])
            .args(args)
            .current_dir(&self.dir)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    /// Commits everything and returns the commit.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
        self.head()
    }

    /// The commit checked out.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// An annotated tag at the commit checked out.
    pub fn tag(&self, name: &str) {
        self.git(&["tag", "-a", name, "-m", name]);
    }

    /// The arguments of a plan for the commit checked out, with main still there.
    pub fn plan(&self) -> PlanArgs {
        let head = self.head();
        PlanArgs {
            tip: head.clone(),
            head,
            repo_dir: self.dir.clone(),
            ship: vec!["app".to_owned()],
            dry_run: false,
            output: None,
            git: PathBuf::from("git"),
            cargo: PathBuf::from(env!("CARGO")),
        }
    }
}

/// The value of `key` in the `key=value` lines `plan` printed.
pub fn field<'a>(lines: &'a str, key: &str) -> &'a str {
    lines
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {key} in {lines:?}"))
}
