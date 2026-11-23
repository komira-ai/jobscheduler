//! The scratch trees the spawn-site tests check: laid out as the repository is (the check
//! reads the crates' manifests and the spawner path), either small trees written by a test or
//! a copy of the real sources. Nothing in the repository is edited. Shared by the
//! `spawn_sites` and `spawn_sites_keepers` tests.

#![allow(
    dead_code,
    reason = "each test binary that includes this module uses a part of it"
)]

use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;
use xtask::spawn_sites::{Error, SPAWNER, Site, check, check_with_keepers, crates};

/// The repository root.
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch tree with the two starting crates: an empty `js-agent` library, and a
/// `js-mac-session` library whose spawner holds a `Command::new` (the one place one may
/// be); each has a manifest that names no dependency.
pub struct Tree {
    dir: TestDir,
    /// Whether this is a copy of the real sources: only those hold the files of `KEEPERS`, so
    /// a small tree is checked with no keepers.
    real: bool,
}

impl Tree {
    /// The tree, in a scratch directory named after `name`.
    pub fn new(name: &str) -> Tree {
        let tree = Tree {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("spawn-sites-{name}")),
            real: false,
        };
        tree.manifest("js-agent", "");
        tree.put("crates/js-agent/src/lib.rs", "");
        tree.manifest("js-mac-session", "");
        tree.put("crates/js-mac-session/src/lib.rs", "pub mod disclaim;\n");
        tree.put(
            SPAWNER,
            "pub fn start() { std::process::Command::new(\"/bin/true\"); }\n",
        );
        tree
    }

    /// A copy of the real crates the check reads (their sources, manifests, and the test-only
    /// binaries' modules that a `#[path]` reaches), in a scratch directory named after `name`.
    pub fn real(name: &str) -> Tree {
        let mut tree = Tree::new(name);
        tree.real = true;
        for krate in crates(&repo()).unwrap() {
            let rel = Path::new("crates").join(krate);
            copy(
                &repo().join(&rel).join("src"),
                &tree.root().join(&rel).join("src"),
            );
            copy(
                &repo().join(&rel).join("tests/bin"),
                &tree.root().join(&rel).join("tests/bin"),
            );
            // The modules the agent's test-only binaries reach through `#[path]`: the fakes of
            // `js-update-hook-fake`, and the measurement and the runtime host of
            // `js-launch-measure`.
            for dir in [
                "tests/update_hook_fakes",
                "tests/launch_measure",
                "tests/runtime_host",
            ] {
                copy(
                    &repo().join(&rel).join(dir),
                    &tree.root().join(&rel).join(dir),
                );
            }
            std::fs::copy(
                repo().join(&rel).join("Cargo.toml"),
                tree.root().join(&rel).join("Cargo.toml"),
            )
            .unwrap();
        }
        tree
    }

    /// The tree's root.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `rel`, making its directories.
    pub fn put(&self, rel: &str, text: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Adds `text` to the end of the file `rel`; returns the number of the line the file
    /// ended on, so the added text's lines are numbered after it.
    pub fn append(&self, rel: &str, text: &str) -> u32 {
        let path = self.root().join(rel);
        let mut held = std::fs::read_to_string(&path).unwrap();
        let line = u32::try_from(held.lines().count()).unwrap();
        held.push_str(text);
        std::fs::write(&path, held).unwrap();
        line
    }

    /// Writes the manifest of `krate`: its package, then `extra`.
    pub fn manifest(&self, krate: &str, extra: &str) {
        self.put(
            &format!("crates/{krate}/Cargo.toml"),
            &format!("[package]\nname = \"{krate}\"\n{extra}"),
        );
    }

    /// What the check found, each as `file:line: what`: with the real `KEEPERS` for a copy of
    /// the real sources, with none for a small tree.
    pub fn sites(&self) -> Vec<String> {
        shown(self.check())
    }

    /// The check of this tree, as [`Tree::sites`] says.
    fn check(&self) -> Result<Vec<Site>, Error> {
        if self.real {
            check(self.root())
        } else {
            check_with_keepers(self.root(), &[])
        }
    }

    /// What the check found with `keepers` as the files that may keep the responsibility.
    pub fn sites_keeping(&self, keepers: &[&str]) -> Vec<String> {
        shown(check_with_keepers(self.root(), keepers))
    }

    /// The error the check stopped at, as it is printed (keepers as for [`Tree::sites`]).
    pub fn error(&self) -> String {
        printed(self.check())
    }

    /// The error the check stopped at with `keepers`, as it is printed.
    pub fn error_keeping(&self, keepers: &[&str]) -> String {
        printed(check_with_keepers(self.root(), keepers))
    }
}

/// The sites of a check that read the tree, each as `file:line: what`.
fn shown(found: Result<Vec<Site>, Error>) -> Vec<String> {
    found.unwrap().iter().map(ToString::to_string).collect()
}

/// The error of a check that stopped, as it is printed.
fn printed(found: Result<Vec<Site>, Error>) -> String {
    match found {
        Ok(sites) => panic!("no error; found {sites:?}"),
        Err(error) => error.to_string(),
    }
}

/// `file:line: what`, for a list of `(line, what)`.
pub fn at(file: &str, found: &[(u32, &str)]) -> Vec<String> {
    found
        .iter()
        .map(|(line, what)| format!("{file}:{line}: {what}"))
        .collect()
}

/// Copies every file under `from` to `to`; a `from` that is not a directory is not copied.
fn copy(from: &Path, to: &Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy(&path, &target);
        } else {
            std::fs::create_dir_all(to).unwrap();
            std::fs::copy(&path, &target).unwrap();
        }
    }
}
