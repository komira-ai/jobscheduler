//! Every program the crates of `js-agent` and `js-mac-session` start goes through the one
//! spawner, `js_mac_session::disclaim`, which disclaims the program's privacy (TCC)
//! responsibility on macOS; and the check that says so fails on each way around it.
//!
//! The failing cases build small trees in a scratch directory, laid out as the repository
//! is (the check reads the crates' manifests and the spawner path), plus one copy of the
//! real sources with one start added. Nothing in the repository is edited.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;
use xtask::spawn_sites::{Error, SPAWNER, check, crates};

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch tree with the two starting crates: an empty `js-agent` library, and a
/// `js-mac-session` library whose spawner holds a `Command::new` (the one place one may
/// be); each has a manifest that names no dependency.
struct Tree {
    dir: TestDir,
}

impl Tree {
    fn new(name: &str) -> Tree {
        let tree = Tree {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("spawn-sites-{name}")),
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

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `rel`, making its directories.
    fn put(&self, rel: &str, text: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Writes the manifest of `krate`: its package, then `extra`.
    fn manifest(&self, krate: &str, extra: &str) {
        self.put(
            &format!("crates/{krate}/Cargo.toml"),
            &format!("[package]\nname = \"{krate}\"\n{extra}"),
        );
    }

    /// Writes a library for `krate` that holds one `Command::new`, on line 1.
    fn starter(&self, krate: &str) {
        self.manifest(krate, "");
        self.put(
            &format!("crates/{krate}/src/lib.rs"),
            "fn f() { Command::new(\"x\"); }\n",
        );
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

/// `file:line: what`, for a list of `(line, what)`.
fn at(file: &str, found: &[(u32, &str)]) -> Vec<String> {
    found
        .iter()
        .map(|(line, what)| format!("{file}:{line}: {what}"))
        .collect()
}

/// The repository as it is: nothing in the crates the agent and the helper are built from
/// starts a program but the spawner.
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

/// The crates read are the two starting ones and what they depend on through a path, and
/// not what is built for Linux only or for tests: the host steps `js-mac-provision` runs
/// (`pfctl`) are in, `js-contain` and `js-testkit` are out. It catches a fixed list of
/// crates that leaves a dependency's `Command` unread.
#[test]
fn the_real_crates_read_include_the_agents_path_dependencies() {
    let read = crates(&repo()).unwrap();
    for krate in ["js-agent", "js-mac-session", "js-mac-provision", "js-types"] {
        assert!(read.contains(krate), "{krate} is not read: {read:?}");
    }
    for krate in ["js-contain", "js-testkit"] {
        assert!(!read.contains(krate), "{krate} is read: {read:?}");
    }
}

/// The real sources, copied, with one `Command::new` added to the native runtime: the
/// check reaches that module of the real tree, and names the file and line. It catches a
/// check that walks a toy tree only, or that stops short of the modules the agent has.
#[test]
fn a_start_added_to_the_real_agent_is_found() {
    let tree = Tree::new("real");
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
        // The fakes the agent's test binary reaches through `#[path]`.
        copy(
            &repo().join(&rel).join("tests/update_hook_fakes"),
            &tree.root().join(&rel).join("tests/update_hook_fakes"),
        );
        std::fs::copy(
            repo().join(&rel).join("Cargo.toml"),
            tree.root().join(&rel).join("Cargo.toml"),
        )
        .unwrap();
    }
    let native = tree.root().join("crates/js-agent/src/native/mod.rs");
    let mut text = std::fs::read_to_string(&native).unwrap();
    // The added `Command::new` is on the third line after the file's last one.
    let line = u32::try_from(text.lines().count() + 3).unwrap();
    text.push_str("\nfn stray() {\n    let _ = std::process::Command::new(\"/bin/true\");\n}\n");
    std::fs::write(&native, text).unwrap();
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/native/mod.rs",
            &[(line, "Command::new"), (line, "process::Command")]
        )
    );
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

/// The tree of [`Tree::new`] passes: the spawner's own `Command::new` is not a site. It
/// catches a check that fails every tree, so that a red result would say nothing.
#[test]
fn a_tree_that_starts_programs_only_through_the_spawner_passes() {
    assert!(Tree::new("clean").sites().is_empty());
}

/// Each way to write a start in code is a site, with its file and line, in code and in a
/// macro's tokens: a qualified `Command`, `pre_exec`, `exec`, a C library call or
/// declaration. It catches a way the check does not see.
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
    execl();
}
extern "C" {
    fn posix_spawn();
    fn system();
}
"#,
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/lib.rs",
            &[
                (1, "process::Command"),
                (2, "libc::execvp"),
                (3, "libc::vfork"),
                (4, "process::Command"),
                (5, "Command::new"),
                (6, "Command::new"),
                (6, "process::Command"),
                (7, "Command::new"),
                (7, "process::Command"),
                (8, ".pre_exec"),
                (9, ".exec"),
                (10, "libc::execv"),
                (11, "libc::posix_spawnp"),
                (12, "libc::system"),
                (13, "libc::popen"),
                (14, ".exec"),
                (14, ".pre_exec"),
                (14, "Command::new"),
                (14, "libc::execve"),
                (15, "execl"),
                (18, "libc::posix_spawn"),
                (19, "libc::system"),
            ]
        )
    );
}

/// A type alias, imports (grouped, renamed, glob) and the trait's functions called by
/// their path are sites. It catches a `Command` hidden behind an alias, a rename, a
/// `use libc::{system, popen}` followed by bare calls, or `CommandExt::exec(&mut c)`.
#[test]
fn aliases_imports_globs_and_plain_function_calls_are_found() {
    let tree = Tree::new("imports");
    tree.put(
        "crates/js-agent/src/lib.rs",
        r#"type C = std::process::Command;
use libc::{system, popen};
use libc::execv as e;
use std::process::{Command as K, Stdio};
use std::os::unix::process::CommandExt;
use std::os::unix::process::*;
use tokio::process::*;
use libc::*;
use std::{process::*, io};
fn f(mut c: C) {
    std::os::unix::process::CommandExt::exec(&mut c);
    CommandExt::pre_exec(&mut c, g);
}
"#,
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/lib.rs",
            &[
                (1, "process::Command"),
                (2, "libc::popen"),
                (2, "libc::system"),
                (3, "libc::execv"),
                (4, "process::Command"),
                (5, "CommandExt"),
                (6, "process::*"),
                (7, "process::*"),
                (8, "libc::*"),
                (9, "process::*"),
                (11, "CommandExt"),
                (12, "CommandExt"),
            ]
        )
    );
}

/// An import or `extern crate` that renames the `libc` crate is a site, by itself: under
/// another name its `system` and `popen` would be called as `l::system`, which names no
/// `libc`. Renames of other crates, and `libc` under its own name, are not. It catches a
/// renamed C library that hides a shell start from the check.
#[test]
fn a_renamed_libc_crate_is_found() {
    let tree = Tree::new("renamed-libc");
    tree.put(
        "crates/js-agent/src/lib.rs",
        r#"use libc as l;
use libc::{self as k};
use foo as f;
use foo::{self as g};
extern crate libc as m;
extern crate libc;
extern crate alloc as a;
fn h() { l::system(c); }
"#,
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/lib.rs",
            &[
                (1, "libc renamed"),
                (2, "libc renamed"),
                (5, "libc renamed")
            ]
        )
    );
}

/// The same forms inside a macro's tokens are sites, including an import list in braces
/// (nested too). It catches a macro that hides an alias, an import or a call from the
/// check, or an import list that is read without the path before it.
#[test]
fn the_same_forms_inside_a_macro_are_found() {
    let tree = Tree::new("macros");
    tree.put(
        "crates/js-agent/src/lib.rs",
        r#"m!(type C = std::process::Command;);
m!(use libc::{system, popen};);
m!(use libc::execv as e;);
m!(use std::process::{Command as K, Stdio};);
m!(std::os::unix::process::CommandExt::exec(&mut c););
m!(use std::os::unix::process::*; use tokio::process::*; use libc::*;);
m!(use std::{process::{Command, Child}, io};);
m!(a(b), libc :: (c), { Command new });
"#,
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/lib.rs",
            &[
                (1, "process::Command"),
                (2, "libc::popen"),
                (2, "libc::system"),
                (3, "libc::execv"),
                (4, "process::Command"),
                (5, "CommandExt"),
                (6, "libc::*"),
                (6, "process::*"),
                (6, "process::*"),
                (7, "process::Command"),
                (8, "Command::new"),
            ]
        )
    );
}

/// Names that only resemble a start are not sites. It catches a check that flags a
/// lookalike: `Command::news`, `Program::new`, `.execute`, `crate::system`, `Child`,
/// `Stdio`, `process::exit`, or another `libc` function.
#[test]
fn names_that_only_resemble_a_start_are_not_sites() {
    let tree = Tree::new("alike");
    tree.put(
        "crates/js-agent/src/lib.rs",
        r#"use std::process::{Child, Stdio};
fn alike(c: &std::process::Child, s: std::process::Stdio) {
    Command::news();
    Program::new("e");
    c.execute();
    crate::system::id();
    use crate::system;
    std::process::exit(1);
    libc::getpid();
    m!(c . execute, Command new_ish, a: b, Program new, libc :: getpid, libc);
}
"#,
    );
    assert!(tree.sites().is_empty());
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
#[cfg(test)]
use std::process::Command;
use std::process::Command as Late;
"#,
    );
    let mut expected: Vec<(u32, &str)> = [14, 17, 19, 21, 23, 25, 27, 29, 31]
        .map(|line| (line, "Command::new"))
        .to_vec();
    expected.push((34, "process::Command"));
    assert_eq!(tree.sites(), at("crates/js-agent/src/lib.rs", &expected));
}

/// Modules are followed to their files, `name.rs` and `name/mod.rs`, below a file module
/// and an inline one, in both crates and from `main.rs` too; a module two roots declare is
/// read once. It catches a module the check does not reach: one in a directory, one under
/// an inline module, or one of the binary's, or one it reports twice.
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
        &format!("mod bin_only;\nmod flat;\n{start}"),
    );
    tree.put("crates/js-agent/src/bin_only.rs", start);
    tree.put(
        "crates/js-mac-session/src/lib.rs",
        &format!("pub mod disclaim;\nmod helper;\n{start}"),
    );
    tree.put("crates/js-mac-session/src/helper.rs", start);
    let site = |file: &str, line: u32| format!("crates/{file}:{line}: Command::new");
    assert_eq!(
        tree.sites(),
        [
            site("js-agent/src/bin_only.rs", 1),
            site("js-agent/src/flat/below.rs", 1),
            site("js-agent/src/flat.rs", 2),
            site("js-agent/src/inline/deep.rs", 1),
            site("js-agent/src/main.rs", 3),
            site("js-agent/src/nested/mod.rs", 2),
            site("js-agent/src/nested/under.rs", 1),
            site("js-mac-session/src/helper.rs", 1),
            site("js-mac-session/src/lib.rs", 3),
        ]
    );
}

/// The roots of a crate are the manifest's `[lib]` and `[[bin]]` paths and the default
/// layout's files (`src/main.rs`, `src/bin/x.rs`, `src/bin/y/main.rs`); a file in
/// `src/bin` that is not a root, and one that is not Rust, are not read. It catches a
/// binary the check does not start from, such as a custom path or a `src/bin` program.
#[test]
fn the_roots_of_a_crate_come_from_its_manifest_and_layout() {
    let tree = Tree::new("roots");
    tree.manifest(
        "js-agent",
        "[lib]\npath = \"src/core.rs\"\n[[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n[[bin]]\nname = \"implied\"\n",
    );
    std::fs::remove_file(tree.root().join("crates/js-agent/src/lib.rs")).unwrap();
    let start = "fn f() { Command::new(\"x\"); }\n";
    tree.put("crates/js-agent/src/core.rs", start);
    tree.put("crates/js-agent/src/main.rs", start);
    tree.put("crates/js-agent/src/bin/x.rs", start);
    tree.put("crates/js-agent/src/bin/y/main.rs", start);
    tree.put("crates/js-agent/src/bin/y/other.rs", start);
    tree.put("crates/js-agent/src/bin/z/extra.rs", start);
    tree.put("crates/js-agent/src/bin/notes.txt", "Command::new");
    tree.put(
        "crates/js-agent/tools/tool.rs",
        &format!("mod helper;\n{start}"),
    );
    tree.put("crates/js-agent/tools/helper.rs", start);
    let site = |file: &str, line: u32| format!("crates/js-agent/{file}:{line}: Command::new");
    assert_eq!(
        tree.sites(),
        [
            site("src/bin/x.rs", 1),
            site("src/bin/y/main.rs", 1),
            site("src/core.rs", 1),
            site("src/main.rs", 1),
            site("tools/helper.rs", 1),
            site("tools/tool.rs", 2),
        ]
    );
}

/// The crates read are the path dependencies of the two, transitively, through
/// `[dependencies]` and every target table but the Linux-only one; dev- and
/// build-dependencies, the Linux-only table and dependencies that name no path are not
/// followed. It catches a dependency whose start goes unseen, and a crate read that no
/// macOS build holds.
#[test]
fn the_crates_read_are_the_path_dependencies_built_for_macos() {
    let tree = Tree::new("closure");
    tree.manifest(
        "js-agent",
        r#"[dependencies]
dep-a = { path = "../dep-a" }
serde.workspace = true
plain = "1"
[target.'cfg(target_os = "macos")'.dependencies]
mac-only = { path = "../mac-only" }
[target.'cfg(target_os = "linux")'.dependencies]
linux-only = { path = "../linux-only" }
[dev-dependencies]
dev-only = { path = "../dev-only" }
[build-dependencies]
build-only = { path = "../build-only" }
"#,
    );
    tree.manifest(
        "js-mac-session",
        "[dependencies]\ndep-b = { path = \"../dep-b\" }\n",
    );
    tree.manifest("dep-a", "[dependencies]\ndep-b = { path = \"../dep-b\" }\n");
    for krate in ["dep-b", "mac-only", "linux-only", "dev-only", "build-only"] {
        tree.starter(krate);
    }
    tree.put("crates/js-mac-session/src/lib.rs", "pub mod disclaim;\n");
    tree.put("crates/dep-a/src/lib.rs", "");
    let read: BTreeSet<String> = ["dep-a", "dep-b", "js-agent", "js-mac-session", "mac-only"]
        .map(String::from)
        .into();
    assert_eq!(crates(tree.root()).unwrap(), read);
    assert_eq!(
        tree.sites(),
        [
            "crates/dep-b/src/lib.rs:1: Command::new",
            "crates/mac-only/src/lib.rs:1: Command::new",
        ]
    );
}

/// A `#[path = "literal"] mod name;` is followed to the file at that path from the
/// declaring file's directory (here with `../`), and the child modules of that file are in
/// its own directory. It catches a module behind a `#[path]` that goes unread, and a
/// child of it looked for next to the declaring file.
#[test]
fn a_path_attribute_with_a_literal_is_followed() {
    let tree = Tree::new("path-followed");
    tree.put(
        "crates/js-agent/src/lib.rs",
        "#[path = \"../shared/m.rs\"]\nmod m;\n",
    );
    tree.put(
        "crates/js-agent/shared/m.rs",
        "mod child;\nfn f() { Command::new(\"x\"); }\n",
    );
    tree.put(
        "crates/js-agent/shared/child.rs",
        "fn g() { Command::new(\"y\"); }\n",
    );
    assert_eq!(
        tree.sites(),
        at(
            "crates/js-agent/src/../shared/child.rs",
            &[(1, "Command::new")]
        )
        .into_iter()
        .chain(at(
            "crates/js-agent/src/../shared/m.rs",
            &[(2, "Command::new")]
        ))
        .collect::<Vec<_>>()
    );
}

/// An `include!` is an error naming the file, in any path form; `include_str!` and
/// `include_bytes!` are not. It catches a file the check cannot read being pulled in.
#[test]
fn an_include_is_an_error_but_include_str_is_not() {
    let tree = Tree::new("include");
    tree.put(
        "crates/js-agent/src/lib.rs",
        "include!(\"gen.rs\");\nfn f() { std::include!(\"x.rs\"); }\n",
    );
    assert_eq!(
        tree.error(),
        "crates/js-agent/src/lib.rs: `include!` is not followed by the spawn-site check"
    );
    assert!(matches!(check(tree.root()), Err(Error::Include { .. })));

    let tree = Tree::new("include-str");
    tree.put(
        "crates/js-agent/src/lib.rs",
        "const S: &str = include_str!(\"x\");\nconst B: &[u8] = include_bytes!(\"y\");\n",
    );
    assert!(tree.sites().is_empty());
}

/// A part of the tree the check cannot read stops it with the reason, never a pass that
/// skipped it: a module with no file, a `#[path]`, a crate with no root, a dependency
/// without a manifest, a manifest that is not TOML, no spawner, a file that is not Rust
/// and one that is not text. It catches a check that passes because it read less than the
/// crates hold.
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

    let refused = |name: &str, lib: &str| {
        let tree = Tree::new(name);
        tree.put("crates/js-agent/src/lib.rs", lib);
        tree.error()
    };
    let message = |name: &str| {
        format!(
            "crates/js-agent/src/lib.rs: `mod {name}` has a #[path] attribute that the \
             spawn-site check does not follow (it follows a string literal on a \
             `mod name;` outside an inline module)"
        )
    };
    assert_eq!(
        refused("path-inline", "#[path = \"x.rs\"]\nmod y { }\n"),
        message("y")
    );
    assert_eq!(
        refused(
            "path-inside",
            "mod outer {\n#[path = \"x.rs\"]\nmod z;\n}\n"
        ),
        message("z")
    );
    assert_eq!(refused("path-int", "#[path = 1]\nmod w;\n"), message("w"));
    assert_eq!(refused("path-bare", "#[path]\nmod v;\n"), message("v"));

    let tree = Tree::new("path-missing");
    tree.put(
        "crates/js-agent/src/lib.rs",
        "#[path = \"gone.rs\"]\nmod u;\n",
    );
    let error = tree.error();
    assert!(
        error.starts_with("crates/js-agent/src/gone.rs: "),
        "{error}"
    );
    assert!(matches!(check(tree.root()), Err(Error::Read { .. })));

    let tree = Tree::new("root");
    std::fs::remove_file(tree.root().join("crates/js-agent/src/lib.rs")).unwrap();
    assert_eq!(
        tree.error(),
        "crates/js-agent: no crate root \
         (no [lib] or [[bin]] path, src/lib.rs, src/main.rs or src/bin source)"
    );

    let tree = Tree::new("dependency");
    tree.manifest(
        "js-agent",
        "[dependencies]\nghost = { path = \"../ghost\" }\n",
    );
    let error = tree.error();
    assert!(error.starts_with("crates/ghost/Cargo.toml: "), "{error}");
    assert!(matches!(check(tree.root()), Err(Error::Read { .. })));
    assert!(crates(tree.root()).is_err());

    let tree = Tree::new("manifest");
    tree.put("crates/js-agent/Cargo.toml", "[package\n");
    let error = tree.error();
    assert!(error.starts_with("crates/js-agent/Cargo.toml: "), "{error}");
    assert!(matches!(check(tree.root()), Err(Error::Manifest { .. })));

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
