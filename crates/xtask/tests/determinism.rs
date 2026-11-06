//! No clock or random source is read in the crates the simulation cell links but at the
//! sites the allow-list names; and the lint that says so fails on each way around it.
//!
//! The failing cases build small trees in a scratch directory, laid out as the repository
//! is (`crates/<name>/src/`), plus one copy of the real sources with one clock read added
//! and one allowed read taken away. Nothing in the repository is edited.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::dir::TestDir;
use xtask::determinism::{ALLOWED, Allowed, CRATES, Error, Token, check, run, sites};

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch tree laid out as the repository.
struct Tree {
    dir: TestDir,
}

impl Tree {
    fn new(name: &str) -> Tree {
        Tree {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("determinism-{name}")),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `rel`, making its directories.
    fn put(&self, rel: &str, text: impl AsRef<[u8]>) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        std::fs::write(path, text).expect("write");
    }

    /// The sites of crate `fix`, each as `line token item`.
    fn found(&self) -> Vec<String> {
        sites(self.root(), &["fix"])
            .expect("the tree reads")
            .iter()
            .map(|site| format!("{} {} {}", site.line, site.token, site.item))
            .collect()
    }

    /// The error [`check`] of crate `fix` with `allowed` stopped at, as it is printed.
    fn error(&self, allowed: &[Allowed]) -> String {
        match check(self.root(), &["fix"], allowed) {
            Ok(count) => panic!("no error; {count} sites allowed"),
            Err(error) => error.to_string(),
        }
    }
}

/// An allow-list entry for crate `fix`'s `lib.rs`.
fn entry(item: &'static str, token: Token, reason: &'static str) -> Allowed {
    Allowed {
        file: "crates/fix/src/lib.rs",
        item,
        token,
        sites: 1,
        reason,
    }
}

/// Asserts that an `Instant::now` no entry allows fails the lint with one line naming its
/// file, its line, the token and the function it is in. Catches a lint that reads no file,
/// or that passes a clock read nothing allows.
#[test]
fn an_instant_now_outside_the_allow_list_fails_naming_its_file_line_and_token() {
    let tree = Tree::new("instant");
    tree.put(
        "crates/fix/src/lib.rs",
        "//! A clock.\npub fn stamp() -> std::time::Instant {\n    std::time::Instant::now()\n}\n",
    );
    assert_eq!(
        tree.error(&[]),
        "crates/fix/src/lib.rs:3: Instant::now in stamp is not on the allow-list"
    );
}

/// Asserts that each of the five tokens is a site, at its own line and in its own
/// function, and that the lint fails listing all five in line order. Catches a token
/// left off the list, or a second site hidden behind the first.
#[test]
fn each_banned_token_is_found_where_no_entry_allows_it() {
    let tree = Tree::new("tokens");
    tree.put(
        "crates/fix/src/lib.rs",
        "fn wall() { let _ = SystemTime::now(); }\n\
         fn mono() { let _ = std::time::Instant::now(); }\n\
         fn os(b: &mut [u8]) { let _ = getrandom::fill(b); }\n\
         fn rng() { let _ = rand::thread_rng(); }\n\
         fn id() { let _ = uuid::Uuid::new_v4(); }\n",
    );
    assert_eq!(
        tree.found(),
        [
            "1 SystemTime::now wall",
            "2 Instant::now mono",
            "3 getrandom os",
            "4 thread_rng rng",
            "5 Uuid::new_v4 id",
        ]
    );
    let names: Vec<&str> = Token::ALL.iter().map(|token| token.as_str()).collect();
    assert_eq!(
        names,
        [
            "SystemTime::now",
            "Instant::now",
            "getrandom",
            "thread_rng",
            "Uuid::new_v4"
        ]
    );
    let file = "crates/fix/src/lib.rs";
    assert_eq!(
        tree.error(&[]),
        format!(
            "{file}:1: SystemTime::now in wall is not on the allow-list\n\
             {file}:2: Instant::now in mono is not on the allow-list\n\
             {file}:3: getrandom in os is not on the allow-list\n\
             {file}:4: thread_rng in rng is not on the allow-list\n\
             {file}:5: Uuid::new_v4 in id is not on the allow-list"
        )
    );
}

/// Asserts which ways of writing a token are sites: in a macro call, as a function value,
/// in a type, in an import (alone, in a braced list, or renamed), in a qualified path, in an
/// `impl` block (named by its type under any `&`, or `impl` for a tuple), a trait's default
/// method, an inline module, a constant and a static; and which are not:
/// a comment, a doc comment, a string, the type or `now` alone, and the two words with no
/// `::` between them. Catches a lint that reads only calls written in full, or that reads
/// text rather than code.
#[test]
fn where_a_token_is_written_decides_whether_it_is_a_site() {
    let tree = Tree::new("forms");
    tree.put(
        "crates/fix/src/lib.rs",
        "use getrandom::fill;\n\
         use rand::{Rng, thread_rng};\n\
         use std::time::Instant::now as stamp;\n\
         fn shown() { println!(\"{:?}\", Instant::now()); }\n\
         static START: LazyLock<Instant> = LazyLock::new(Instant::now);\n\
         fn typed() -> Result<(), getrandom::Error> { Ok(()) }\n\
         impl Clock for Wall { fn now(&self) -> SystemTime { SystemTime::now() } }\n\
         impl Clock for &mut Wall { fn now(&self) -> SystemTime { SystemTime::now() } }\n\
         trait Fresh { fn fresh() -> Uuid { Uuid::new_v4() } }\n\
         mod inner { fn deep() { thread_rng(); } }\n\
         const NOW: fn() -> Instant = Instant::now;\n\
         impl Clock for (Wall,) { fn now(&self) -> SystemTime { SystemTime::now() } }\n\
         fn qualified() { <Instant>::now(); <Instant as Clock>::now(); }\n\
         // Instant::now() in a comment\n\
         /// SystemTime::now() in a doc comment\n\
         fn quiet(t: Instant) -> &'static str { now(); m!(Instant now); \"getrandom\" }\n",
    );
    assert_eq!(
        tree.found(),
        [
            "1 getrandom ",
            "2 thread_rng ",
            "3 Instant::now ",
            "4 Instant::now shown",
            "5 Instant::now START",
            "6 getrandom typed",
            "7 SystemTime::now Wall::now",
            "8 SystemTime::now Wall::now",
            "9 Uuid::new_v4 Fresh::fresh",
            "10 thread_rng inner::deep",
            "11 Instant::now NOW",
            "12 SystemTime::now impl::now",
            "13 Instant::now qualified",
        ]
    );
    assert!(
        tree.error(&[])
            .starts_with("crates/fix/src/lib.rs:1: getrandom at the top level of the file is not on the allow-list\n"),
        "a site outside any item is named so"
    );
}

/// Asserts that code only a test build compiles is not read: items under `#[cfg(test)]` or
/// a `#[cfg(all(..))]` that holds `test` (a module, a function, an `impl`, a method, a
/// trait and its method, an import, a constant, a static), and the file of a module
/// declared so, wherever it is (beside `lib.rs`, as `name/mod.rs`, under a `#[path]`, in the
/// directory of a `mod.rs`, of a file module and of an inline module, and declared inside a
/// test-only inline module at any depth), with its own child modules; while code under any other `cfg`, and a file beside a test module's, is read.
/// Catches a lint that fails on tests,
/// or that leaves out more than tests.
#[test]
fn code_only_a_test_build_compiles_is_not_read() {
    let tree = Tree::new("tests");
    let clock = "fn f() { let _ = Instant::now(); }\n";
    tree.put(
        "crates/fix/src/lib.rs",
        "#[cfg(test)]\nmod tests { fn t() { Instant::now(); } }\n\
         #[cfg(test)]\nfn helper() { Instant::now(); }\n\
         #[cfg(all(unix, test))]\nimpl Wall { fn now() { Instant::now(); } }\n\
         impl Wall { #[cfg(test)] fn later() { Instant::now(); } }\n\
         #[cfg(test)]\ntrait Clocked { fn now() { Instant::now(); } }\n\
         trait Timed { #[cfg(test)] fn now() { Instant::now(); } }\n\
         #[cfg(test)]\nuse getrandom::fill;\n\
         #[cfg(test)]\nconst A: fn() -> Instant = Instant::now;\n\
         #[cfg(test)]\nstatic B: fn() -> Instant = Instant::now;\n\
         /// The file tests.\n#[cfg(test)]\nmod file_tests;\n\
         #[cfg(test)]\nmod nested_tests;\n\
         #[cfg(test)]\n#[path = \"elsewhere.rs\"]\nmod moved;\n\
         mod a;\nmod b;\n\
         mod c { #[cfg(test)] mod t; }\n\
         #[cfg(not(test))]\nfn built() { Instant::now(); }\n\
         #[cfg(unix)]\nfn unix() { Instant::now(); }\n\
         #[cfg(test)]\nmod inline_tests { mod child; mod deeper { mod leaf; } }\n",
    );
    for file in [
        "file_tests.rs",
        "file_tests/child.rs",
        "nested_tests/mod.rs",
        "elsewhere.rs",
        "a/t.rs",
        "b/t.rs",
        "c/t.rs",
        "inline_tests/child.rs",
        "inline_tests/deeper/leaf/mod.rs",
    ] {
        tree.put(&format!("crates/fix/src/{file}"), clock);
    }
    tree.put("crates/fix/src/a/mod.rs", "#[cfg(test)]\nmod t;\n");
    tree.put("crates/fix/src/b.rs", "#[cfg(test)]\nmod t;\n");
    tree.put("crates/fix/src/c/kept.rs", clock);
    assert_eq!(
        tree.found(),
        [
            "1 Instant::now f",
            "29 Instant::now built",
            "31 Instant::now unix"
        ],
        "only the code a build without tests compiles is read"
    );
    let kept = sites(tree.root(), &["fix"]).expect("the tree reads");
    let files: BTreeSet<PathBuf> = kept.into_iter().map(|site| site.file).collect();
    let read = ["crates/fix/src/c/kept.rs", "crates/fix/src/lib.rs"];
    assert_eq!(files, read.into_iter().map(PathBuf::from).collect());
}

/// Asserts that a site an entry names (file, item and token) passes, and counts; that an
/// entry for another item, or for another token, allows nothing and is reported stale
/// beside the site; that an entry with no reason fails; and that a second read added to
/// an allowed function fails until the entry counts it. Catches an allow-list matched
/// by file alone, and one that keeps entries for calls that went.
#[test]
fn an_allowed_site_passes_and_an_entry_that_matches_nothing_is_stale() {
    let tree = Tree::new("allowed");
    tree.put(
        "crates/fix/src/lib.rs",
        "impl Clock for Wall {\n    fn now(&self) -> SystemTime { SystemTime::now() }\n}\n",
    );
    let wall = entry("Wall::now", Token::SystemTimeNow, "the production clock");
    assert_eq!(check(tree.root(), &["fix"], &[wall]).expect("allowed"), 1);
    let file = "crates/fix/src/lib.rs";
    let not_allowed = format!("{file}:2: SystemTime::now in Wall::now is not on the allow-list");
    assert_eq!(
        tree.error(&[entry("Wall::then", Token::SystemTimeNow, "moved")]),
        format!(
            "{not_allowed}\n\
             {file}: the allow-list entry for SystemTime::now in Wall::then matches no site: remove it"
        )
    );
    assert_eq!(
        tree.error(&[entry("Wall::now", Token::InstantNow, "another clock")]),
        format!(
            "{not_allowed}\n\
             {file}: the allow-list entry for Instant::now in Wall::now matches no site: remove it"
        )
    );
    assert_eq!(
        tree.error(&[entry("Wall::now", Token::SystemTimeNow, " ")]),
        format!("{file}: the allow-list entry for SystemTime::now in Wall::now gives no reason")
    );
    tree.put(
        "crates/fix/src/lib.rs",
        "impl Clock for Wall {\n    fn now(&self) -> SystemTime {\n        \
         let _ = SystemTime::now();\n        SystemTime::now()\n    }\n}\n",
    );
    assert_eq!(
        tree.error(&[wall]),
        format!(
            "{file}: the allow-list entry for SystemTime::now in Wall::now allows 1 sites, and \
             there are 2: each site needs its reason"
        ),
        "a second read in an allowed function is not allowed with the first"
    );
    let both = Allowed { sites: 2, ..wall };
    assert_eq!(check(tree.root(), &["fix"], &[both]).expect("allowed"), 2);
}

/// Asserts that a listed crate with no `src/`, a file that is not UTF-8, a file that is
/// not Rust and a directory that cannot be listed each fail the lint naming the crate or the file, rather than reading less of
/// the tree in silence.
#[test]
fn a_missing_crate_and_a_file_that_cannot_be_read_or_parsed_fail_naming_it() {
    let tree = Tree::new("errors");
    let missing = sites(tree.root(), &["gone"]).expect_err("no such crate");
    assert!(matches!(missing, Error::NoCrate { .. }), "{missing:?}");
    assert_eq!(
        missing.to_string(),
        "crates/gone/src: no such directory (a crate of the determinism lint's list)"
    );
    tree.put("crates/fix/src/lib.rs", [0xff, 0xfe]);
    let unread = sites(tree.root(), &["fix"]).expect_err("not UTF-8");
    assert!(matches!(unread, Error::Read { .. }), "{unread:?}");
    assert!(
        unread.to_string().starts_with("crates/fix/src/lib.rs: "),
        "{unread}"
    );
    tree.put("crates/fix/src/lib.rs", "fn {");
    let unparsed = sites(tree.root(), &["fix"]).expect_err("not Rust");
    assert!(matches!(unparsed, Error::Parse { .. }), "{unparsed:?}");
    assert!(
        unparsed.to_string().starts_with("crates/fix/src/lib.rs: "),
        "{unparsed}"
    );
    tree.put("crates/fix/src/lib.rs", "");
    let locked = tree.root().join("crates/fix/src/locked");
    std::fs::create_dir_all(&locked).expect("directory");
    let mode = |bits| std::fs::Permissions::from_mode(bits);
    std::fs::set_permissions(&locked, mode(0o000)).expect("lock");
    let unlisted = sites(tree.root(), &["fix"]);
    std::fs::set_permissions(&locked, mode(0o755)).expect("unlock");
    let unlisted = unlisted.expect_err("a directory that cannot be listed");
    assert!(matches!(unlisted, Error::Read { .. }), "{unlisted:?}");
    assert!(
        unlisted.to_string().starts_with("crates/fix/src/locked: "),
        "{unlisted}"
    );
}

/// The repository as it is: every clock and random-source site in the crates the cell
/// links is on the allow-list, and every entry matches a site. It catches a clock read or
/// a random draw added to code a cell run may reach, which would make a reported seed
/// replay differently.
#[test]
fn the_tree_passes_with_the_committed_allow_list() {
    let passed = run(&repo()).expect("the tree passes");
    let tail = format!("sites in {} crates, each on the allow-list", CRATES.len());
    assert!(passed.ends_with(&tail), "{passed}");
}

/// Copies every `.rs` file under `from` to `to`.
fn copy(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read") {
        let path = entry.expect("entry").path();
        let target = to.join(path.file_name().expect("name"));
        if path.is_dir() {
            copy(&path, &target);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            std::fs::create_dir_all(to).expect("directories");
            std::fs::copy(&path, &target).expect("copy");
        }
    }
}

/// The real sources, copied, with an `Instant::now` added to the cell's replay module and
/// the allowed wall-clock read of the state store taken out: the lint fails naming the new
/// site by file and line, and the entry that no longer matches. It catches a lint that
/// passes on a toy tree only, or that reads less of the cell than the cell is.
#[test]
fn a_clock_read_added_to_the_real_cell_is_found() {
    let tree = Tree::new("real");
    for krate in CRATES {
        let src = Path::new("crates").join(krate).join("src");
        copy(&repo().join(&src), &tree.root().join(&src));
    }
    let replay = "crates/js-sim-cell/src/replay.rs";
    let text = std::fs::read_to_string(repo().join(replay)).expect("replay");
    let line = text.lines().count() + 1;
    tree.put(
        replay,
        format!("{text}fn stray() -> std::time::Instant {{ std::time::Instant::now() }}\n"),
    );
    let clock = "crates/js-state/src/clock.rs";
    let text = std::fs::read_to_string(repo().join(clock)).expect("clock");
    assert_eq!(text.matches("SystemTime::now()").count(), 1, "{clock}");
    tree.put(
        clock,
        text.replace("SystemTime::now()", "SystemTime::UNIX_EPOCH"),
    );
    let failed = check(tree.root(), CRATES, ALLOWED).expect_err("a stray clock");
    assert_eq!(
        failed.to_string(),
        format!(
            "{replay}:{line}: Instant::now in stray is not on the allow-list\n\
             {clock}: the allow-list entry for SystemTime::now in SystemClock::now matches no site: remove it"
        )
    );
}

/// Asserts that the lint's crates are exactly `js-sim-cell` and the workspace crates it
/// links, as `cargo metadata` resolves the cell's normal dependencies (on every target),
/// transitively. Catches a dependency added to the cell, or to a crate it links, whose code
/// the lint would not read.
#[test]
fn the_crate_list_is_every_crate_the_sim_cell_links() {
    let metadata = xtask::layering::cargo_metadata(&repo()).expect("cargo metadata");
    let packages = metadata["packages"].as_array().expect("packages");
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .expect("members")
        .iter()
        .filter_map(|id| id.as_str())
        .collect();
    let dir_of = |id: &str| -> Option<String> {
        let package = packages.iter().find(|p| p["id"] == id)?;
        let manifest = Path::new(package["manifest_path"].as_str()?);
        let dir = manifest.parent()?.file_name()?.to_str()?;
        Some(dir.to_owned())
    };
    let cell = packages
        .iter()
        .find(|p| p["name"] == "js-sim-cell")
        .and_then(|p| p["id"].as_str())
        .expect("js-sim-cell");
    let nodes = metadata["resolve"]["nodes"].as_array().expect("nodes");
    let mut seen = BTreeSet::from([cell.to_owned()]);
    let mut queue = vec![cell.to_owned()];
    while let Some(id) = queue.pop() {
        let node = nodes.iter().find(|n| n["id"] == id.as_str()).expect("node");
        for dep in node["deps"].as_array().expect("deps") {
            let normal = dep["dep_kinds"]
                .as_array()
                .expect("kinds")
                .iter()
                .any(|kind| kind["kind"].is_null());
            let pkg = dep["pkg"].as_str().expect("pkg");
            if normal && members.contains(pkg) && seen.insert(pkg.to_owned()) {
                queue.push(pkg.to_owned());
            }
        }
    }
    let linked: BTreeSet<String> = seen.iter().filter_map(|id| dir_of(id)).collect();
    let listed: BTreeSet<String> = CRATES.iter().map(|c| (*c).to_owned()).collect();
    assert_eq!(listed, linked);
}

/// Asserts that every allow-list entry is in a listed crate's `src/`, names its item, and
/// is there once. Catches an entry for a crate the lint does not read, which could never
/// be reported stale for the right reason.
#[test]
fn every_allow_list_entry_is_in_a_listed_crate_and_is_there_once() {
    let mut keys = BTreeSet::new();
    for entry in ALLOWED {
        let in_listed = CRATES
            .iter()
            .any(|krate| entry.file.starts_with(&format!("crates/{krate}/src/")));
        assert!(in_listed, "{entry:?}");
        assert!(!entry.item.is_empty(), "{entry:?}");
        assert!(
            keys.insert((entry.file, entry.item, entry.token)),
            "{entry:?}"
        );
    }
}

/// Asserts that `cargo xtask determinism` prints its result and exits 0 on this tree, and
/// exits 1 with the reason on stderr where a listed crate is missing.
#[test]
fn the_verb_prints_its_result_and_exits_by_it() {
    let xtask = env!("CARGO_BIN_EXE_xtask");
    let passed = Command::new(xtask)
        .args(["determinism", "--root"])
        .arg(repo())
        .output()
        .expect("xtask");
    assert!(passed.status.success(), "{passed:?}");
    let out = String::from_utf8(passed.stdout).expect("utf-8");
    assert!(out.starts_with("determinism: "), "{out}");
    let tree = Tree::new("verb");
    let failed = Command::new(xtask)
        .args(["determinism", "--root"])
        .arg(tree.root())
        .output()
        .expect("xtask");
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(failed.stderr).expect("utf-8"),
        format!(
            "xtask: crates/{}/src: no such directory (a crate of the determinism lint's list)\n",
            CRATES[0]
        )
    );
}
