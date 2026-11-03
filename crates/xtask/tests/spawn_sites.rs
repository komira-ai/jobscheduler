//! Every program `js-agent` and `js-mac-session` start goes through the one spawner,
//! `js_mac_session::disclaim`, which disclaims the program's privacy (TCC) responsibility
//! on macOS; and the check that says so fails on each way around it.
//!
//! The failing cases build small trees of the two crates in a scratch directory, laid out
//! as the repository is (the check reads its real crate list and spawner path), plus one
//! copy of the real sources with one start added. Nothing in the repository is edited.

use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;
use xtask::spawn_sites::{Error, SPAWNER, check};

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch tree with the two crates: an empty `js-agent` library, and a `js-mac-session`
/// library whose spawner holds a `Command::new` (the one place one may be).
struct Tree {
    dir: TestDir,
}

impl Tree {
    fn new(name: &str) -> Tree {
        let tree = Tree {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("spawn-sites-{name}")),
        };
        tree.put("crates/js-agent/src/lib.rs", "");
        tree.put("crates/js-mac-session/src/lib.rs", "pub mod disclaim;\n");
        tree.put(
            SPAWNER,
            "pub fn start() { std::process::Command::new(\"/bin/true\"); }\n",
        );
        tree
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `rel`, making its directories.
    fn put(&self, rel: &str, text: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// What the check found, each as `file:line: what`.
    fn sites(&self) -> Vec<String> {
        check(self.root())
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// The error the check stopped at, as it is printed.
    fn error(&self) -> String {
        match check(self.root()) {
            Ok(sites) => panic!("no error; found {sites:?}"),
            Err(error) => error.to_string(),
        }
    }
}

/// The repository as it is: nothing in either crate starts a program but the spawner.
/// It catches a job, a helper or a probe started with `Command::new` (or `pre_exec`,
/// `exec`, a C library `exec*` or `posix_spawn*`) instead of the spawner: on macOS that
/// process would answer for the helper or the agent, and hold their privacy grants.
#[test]
fn every_job_spawn_path_uses_the_disclaiming_spawner() {
    let sites = check(&repo()).unwrap();
    let listed: Vec<String> = sites.iter().map(ToString::to_string).collect();
    assert!(
        listed.is_empty(),
        "programs started outside {SPAWNER}:\n{}",
        listed.join("\n")
    );
}

/// The real sources, copied, with one `Command::new` added to the native runtime: the
/// check reaches that module of the real tree, and names the file and line. It catches a
/// check that walks a toy tree only, or that stops short of the modules the agent has.
#[test]
fn a_start_added_to_the_real_agent_is_found() {
    let tree = Tree::new("real");
    for krate in ["js-agent", "js-mac-session"] {
        let rel = Path::new("crates").join(krate).join("src");
        copy(&repo().join(&rel), &tree.root().join(&rel));
    }
    let native = tree.root().join("crates/js-agent/src/native/mod.rs");
    let mut text = std::fs::read_to_string(&native).unwrap();
    // The added `Command::new` is on the third line after the file's last one.
    let line = text.lines().count() + 3;
    text.push_str("\nfn stray() {\n    let _ = std::process::Command::new(\"/bin/true\");\n}\n");
    std::fs::write(&native, text).unwrap();
    assert_eq!(
        tree.sites(),
        [format!(
            "crates/js-agent/src/native/mod.rs:{line}: Command::new"
        )]
    );
}

/// Copies every file under `from` to `to`.
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// The tree of [`Tree::new`] passes: the spawner's own `Command::new` is not a site. It
/// catches a check that fails every tree, so that a red result would say nothing.
#[test]
fn a_tree_that_starts_programs_only_through_the_spawner_passes() {
    assert!(Tree::new("clean").sites().is_empty());
}

/// Each way to start a program outside the spawner is a site, with its file and line, in
/// code and in a macro's tokens; a name that only resembles one is not. It catches a way
/// the check does not see: a qualified or renamed `Command`, `pre_exec`, `exec`, a C
/// library call or declaration, or any of these inside a macro.
#[test]
fn every_way_to_start_a_program_outside_the_spawner_is_found() {
    let tree = Tree::new("ways");
    tree.put(
        "crates/js-agent/src/lib.rs",
        r#"use std::process::Command as Cmd;
use libc::execvp;
use libc::vfork as fork_quickly;
fn start(c: &mut std::process::Command) {
    Command::new("a");
    std::process::Command::new("b");
    tokio::process::Command::new("c");
    c.pre_exec(f);
    c.exec();
    libc::execv(p, a);
    libc::posix_spawnp(p, a);
    libc::system(s);
    libc::popen(s, m);
    vec![Command::new("d"), x.pre_exec(f), y.exec(), libc::execve(p, a, e), "Command::new"];
}
extern "C" {
    fn posix_spawn();
    fn system();
}
fn alike(c: &std::process::Command) {
    Command::news();
    Program::new("e");
    c.execute();
    crate::system::id();
    use crate::system;
    m!(c . execute, Command new_ish, a: b);
}
"#,
    );
    let file = "crates/js-agent/src/lib.rs";
    let at = |line: u32, what: &str| format!("{file}:{line}: {what}");
    assert_eq!(
        tree.sites(),
        [
            at(1, "Command imported as Cmd"),
            at(2, "execvp"),
            at(3, "vfork"),
            at(5, "Command::new"),
            at(6, "Command::new"),
            at(7, "Command::new"),
            at(8, ".pre_exec"),
            at(9, ".exec"),
            at(10, "execv"),
            at(11, "posix_spawnp"),
            at(12, "libc::system"),
            at(13, "libc::popen"),
            at(14, ".exec"),
            at(14, ".pre_exec"),
            at(14, "Command::new"),
            at(14, "execve"),
            at(17, "posix_spawn"),
            at(18, "libc::system"),
        ]
    );
}

/// What only a test build compiles is left out, and nothing else is: code built for one
/// platform, or under a cfg that does not need `test`, is read. It catches a check that
/// skips platform code (the macOS code is where the disclaim matters), one that reads test
/// code (tests start programs freely), or one that takes `not(test)` or `any(test, ..)`
/// for test-only.
#[test]
fn only_test_code_is_left_out() {
    let tree = Tree::new("cfg");
    tree.put(
        "crates/js-agent/src/lib.rs",
        r#"#[cfg(test)]
mod tests { fn t() { Command::new("t"); } }
#[cfg(test)]
mod absent_test_file;
#[cfg(test)]
fn t() { Command::new("t"); }
#[cfg(all(unix, test))]
fn t() { Command::new("t"); }
#[cfg(test)]
impl X { fn t() { Command::new("t"); } }
impl X {
    #[cfg(test)]
    fn t() { Command::new("t"); }
    fn m() { Command::new("m"); }
}
#[cfg(target_os = "linux")]
fn linux() { Command::new("l"); }
#[cfg(target_os = "macos")]
mod mac { fn m() { Command::new("m"); } }
#[cfg(not(test))]
fn n() { Command::new("n"); }
#[cfg(any(test, unix))]
fn a() { Command::new("a"); }
#[cfg(all(unix, windows))]
fn w() { Command::new("w"); }
#[cfg(all(test,,))]
fn broken() { Command::new("b"); }
#[cfg()]
fn empty() { Command::new("e"); }
#[inline]
fn i() { Command::new("i"); }
"#,
    );
    let at = |line: u32| format!("crates/js-agent/src/lib.rs:{line}: Command::new");
    assert_eq!(tree.sites(), [14, 17, 19, 21, 23, 25, 27, 29, 31].map(at));
}

/// Modules are followed to their files, `name.rs` and `name/mod.rs`, below a file module
/// and an inline one, in both crates and from `main.rs` too. It catches a module the
/// check does not reach: one in a directory, one under an inline module, or one of the
/// binary's.
#[test]
fn modules_are_followed_to_their_files() {
    let tree = Tree::new("modules");
    let start = "fn f() { Command::new(\"x\"); }\n";
    tree.put(
        "crates/js-agent/src/lib.rs",
        "mod flat;\nmod nested;\nmod inline { mod deep; }\n",
    );
    tree.put(
        "crates/js-agent/src/flat.rs",
        &format!("mod below;\n{start}"),
    );
    tree.put("crates/js-agent/src/flat/below.rs", start);
    tree.put(
        "crates/js-agent/src/nested/mod.rs",
        &format!("mod under;\n{start}"),
    );
    tree.put("crates/js-agent/src/nested/under.rs", start);
    tree.put("crates/js-agent/src/inline/deep.rs", start);
    tree.put(
        "crates/js-agent/src/main.rs",
        &format!("mod bin_only;\n{start}"),
    );
    tree.put("crates/js-agent/src/bin_only.rs", start);
    tree.put(
        "crates/js-mac-session/src/lib.rs",
        &format!("pub mod disclaim;\nmod helper;\n{start}"),
    );
    tree.put("crates/js-mac-session/src/helper.rs", start);
    let at = |file: &str, line: u32| format!("crates/{file}:{line}: Command::new");
    assert_eq!(
        tree.sites(),
        [
            at("js-agent/src/bin_only.rs", 1),
            at("js-agent/src/flat/below.rs", 1),
            at("js-agent/src/flat.rs", 2),
            at("js-agent/src/inline/deep.rs", 1),
            at("js-agent/src/main.rs", 2),
            at("js-agent/src/nested/mod.rs", 2),
            at("js-agent/src/nested/under.rs", 1),
            at("js-mac-session/src/helper.rs", 1),
            at("js-mac-session/src/lib.rs", 3),
        ]
    );
}

/// A part of the tree the check cannot read stops it with the reason, never a pass that
/// skipped it: a module with no file, a `#[path]`, a crate with no root, no spawner, a
/// file that is not Rust and one that is not text. It catches a check that passes because
/// it read less than the crates hold.
#[test]
fn what_cannot_be_read_is_an_error() {
    let tree = Tree::new("missing");
    tree.put("crates/js-agent/src/lib.rs", "mod gone;\n");
    assert_eq!(
        tree.error(),
        "crates/js-agent/src/lib.rs: `mod gone;` has no file \
         (crates/js-agent/src/gone.rs or crates/js-agent/src/gone/mod.rs)"
    );

    let tree = Tree::new("nested-missing");
    tree.put("crates/js-agent/src/lib.rs", "mod a;\n");
    tree.put("crates/js-agent/src/a.rs", "mod b;\nmod c;\n");
    assert_eq!(
        tree.error(),
        "crates/js-agent/src/a.rs: `mod b;` has no file \
         (crates/js-agent/src/a/b.rs or crates/js-agent/src/a/b/mod.rs)",
        "the first error is kept"
    );

    let tree = Tree::new("path");
    tree.put("crates/js-agent/src/lib.rs", "#[path = \"x.rs\"]\nmod y;\n");
    assert_eq!(
        tree.error(),
        "crates/js-agent/src/lib.rs: `mod y` has a #[path] attribute, \
         which the spawn-site check does not follow"
    );

    let tree = Tree::new("root");
    std::fs::remove_file(tree.root().join("crates/js-agent/src/lib.rs")).unwrap();
    assert_eq!(
        tree.error(),
        "crates/js-agent/src has neither lib.rs nor main.rs"
    );

    let tree = Tree::new("spawner");
    tree.put("crates/js-mac-session/src/lib.rs", "");
    assert_eq!(
        tree.error(),
        "no module of js-mac-session is crates/js-mac-session/src/disclaim.rs, \
         the one module that may start programs"
    );
    assert!(matches!(check(tree.root()), Err(Error::NoSpawner)));

    let tree = Tree::new("parse");
    tree.put("crates/js-agent/src/lib.rs", "fn (");
    let error = tree.error();
    assert!(error.starts_with("crates/js-agent/src/lib.rs: "), "{error}");
    assert!(matches!(check(tree.root()), Err(Error::Parse { .. })));

    let tree = Tree::new("bytes");
    let lib = tree.root().join("crates/js-agent/src/lib.rs");
    std::fs::write(&lib, [0xff, 0xfe]).unwrap();
    assert_eq!(
        tree.error(),
        "crates/js-agent/src/lib.rs: stream did not contain valid UTF-8"
    );
}
