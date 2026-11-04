//! The fake host pool `js-pool-fake` stays a test double, and the removed agentless
//! hosts-file pool stays removed: no crate depends on the fake outside its
//! `[dev-dependencies]`, no crate keeps the pool's old name `js-pool-static`, and no
//! production source names the pool's capacity source, kind variant or provider key.
//!
//! Each scan reads the real workspace, and each has a case over a small tree written to a
//! scratch directory that holds the defect, so a scan that reads nothing fails.

use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch tree for one case, removed when it is dropped.
struct Tree {
    dir: TestDir,
}

impl Tree {
    fn new(name: &str) -> Tree {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pool-fake");
        Tree {
            dir: TestDir::under(root, name),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `rel` under the tree.
    fn put(&self, rel: &str, text: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    /// A crate `dir` whose package is `package`, with `tables` after its `[package]`.
    fn krate(&self, dir: &str, package: &str, tables: &str) {
        let manifest = format!("[package]\nname = \"{package}\"\n\n{tables}");
        self.put(&format!("crates/{dir}/Cargo.toml"), &manifest);
        self.put(&format!("crates/{dir}/src/lib.rs"), "\n");
    }
}

/// The crates that may build against the fake host pool `js-pool-fake` outside tests:
/// none. It is a test double; the booking table (`HostBook`, `Candidate`, `Held`,
/// `SeatError`) is `js-provider`'s, and no production pool is built from a hosts file.
const MAY_DEPEND_ON_POOL_FAKE: &[&str] = &[];

/// The name of the crate before it became a test double, which no crate may keep.
const OLD_POOL_NAME: &str = "js-pool-static";

/// The manifests of the crates under `root/crates`, as (directory name, manifest).
fn manifests(root: &Path) -> Vec<(String, toml::Table)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).expect("crates") {
        let dir = entry.expect("entry").path();
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let name = dir
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        found.push((name, text.parse().expect("manifest parses")));
    }
    found
}

/// The package name of `manifest`.
fn package(manifest: &toml::Table) -> String {
    manifest["package"]["name"]
        .as_str()
        .expect("package.name")
        .to_owned()
}

/// The packages a dependency table names: its keys, or `package` where one is renamed.
fn named(table: Option<&toml::Value>) -> Vec<String> {
    let Some(table) = table.and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    table
        .iter()
        .map(|(key, value)| {
            let package = value.get("package").and_then(toml::Value::as_str);
            package.unwrap_or(key).to_owned()
        })
        .collect()
}

/// Every crate under `root/crates` that names `js-pool-fake` as a normal dependency:
/// under `[dependencies]` or `[build-dependencies]`, plain or target-specific, by its key
/// or by `package = "js-pool-fake"`. A `[dev-dependencies]` entry is not one. In name
/// order.
fn normal_dependents_of_pool_fake(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for (_, manifest) in manifests(root) {
        let mut deps = Vec::new();
        for kind in ["dependencies", "build-dependencies"] {
            deps.extend(named(manifest.get(kind)));
            if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
                for target in targets.values() {
                    deps.extend(named(target.get(kind)));
                }
            }
        }
        if deps.iter().any(|dep| dep == "js-pool-fake") {
            found.push(package(&manifest));
        }
    }
    found.sort();
    found
}

/// The crates under `root/crates` whose directory or package is named `name`, or that
/// name a package `name` in any dependency table, dev-dependencies included. In name order.
fn crates_naming(root: &Path, name: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (dir, manifest) in manifests(root) {
        let depends = ["dependencies", "dev-dependencies", "build-dependencies"]
            .iter()
            .any(|kind| named(manifest.get(*kind)).iter().any(|dep| dep == name));
        if dir == name || package(&manifest) == name || depends {
            found.push(package(&manifest));
        }
    }
    found.sort();
    found
}

/// Asserts that in this workspace no crate has the fake host pool `js-pool-fake` as a
/// normal dependency ([`MAY_DEPEND_ON_POOL_FAKE`] is empty), so it is reachable only from
/// `[dev-dependencies]`, and that no crate is named, or depends on, `js-pool-static`.
/// Catches a production crate that depends on the fake again (the agentless pool coming
/// back through it) and the old crate name left behind.
#[test]
fn the_fake_pool_is_reachable_only_from_dev_dependencies() {
    assert_eq!(
        normal_dependents_of_pool_fake(&repo()),
        MAY_DEPEND_ON_POOL_FAKE
    );
    assert_eq!(crates_naming(&repo(), OLD_POOL_NAME), Vec::<String>::new());
}

/// Asserts that the scan reports `js-pool-fake` under `[dependencies]`, under a
/// target-specific `[dependencies]` table, under `[build-dependencies]` plain and
/// target-specific, and under a renamed key, and that a `[dev-dependencies]` entry is not
/// reported. Catches a scan that passes every
/// workspace because it reads the wrong table or the key alone.
#[test]
fn a_normal_dependency_on_js_pool_fake_is_reported() {
    let pool = "js-pool-fake = { path = \"../js-pool-fake\" }";
    let tree = Tree::new("normal");
    tree.krate("plain", "plain", &format!("[dependencies]\n{pool}\n"));
    tree.krate(
        "target",
        "target",
        &format!("[target.'cfg(unix)'.dependencies]\n{pool}\n"),
    );
    tree.krate(
        "renamed",
        "renamed",
        "[dependencies]\nhosts = { path = \"../js-pool-fake\", package = \"js-pool-fake\" }\n",
    );
    tree.krate("build", "build", &format!("[build-dependencies]\n{pool}\n"));
    tree.krate(
        "target-build",
        "target-build",
        &format!("[target.'cfg(unix)'.build-dependencies]\n{pool}\n"),
    );
    tree.krate("dev", "dev", &format!("[dev-dependencies]\n{pool}\n"));
    assert_eq!(
        normal_dependents_of_pool_fake(tree.root()),
        ["build", "plain", "renamed", "target", "target-build"]
    );
}

/// Asserts that the old-name scan reports a crate whose package or directory is named
/// `js-pool-static` and a crate that lists it under `[dev-dependencies]`, and not a crate
/// that names neither. Catches a scan that passes every workspace because it reads the
/// wrong key.
#[test]
fn a_crate_under_the_old_pool_name_is_reported() {
    let tree = Tree::new("old-name");
    tree.krate("js-pool-fake", OLD_POOL_NAME, "");
    tree.krate(OLD_POOL_NAME, "renamed-dir", "");
    tree.krate(
        "user",
        "user",
        "[dev-dependencies]\njs-pool-static = { path = \"../js-pool-static\" }\n",
    );
    tree.krate(
        "clean",
        "clean",
        "[dependencies]\njs-pool-fake = { path = \"../x\" }\n",
    );
    assert_eq!(
        crates_naming(tree.root(), OLD_POOL_NAME),
        [OLD_POOL_NAME, "renamed-dir", "user"]
    );
}

/// The names of the removed agentless hosts-file pool that no production source may use:
/// its capacity source, its kind's variant and its provider key.
const AGENTLESS_WORDS: [&str; 3] = ["StaticPoolSource", "Built::Static", "hosts_file"];

/// The one production file that may name them: the refusal of the old provider kind,
/// whose text names the keys it found.
const AGENTLESS_REFUSAL: &str = "crates/js-lease-server/src/file/agentless.rs";

/// Whether `text` holds `word` as a whole name: not inside a longer identifier.
fn names(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back().is_none_or(|c| !ident(c));
        let after = text[at + word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !ident(c));
        before && after
    })
}

/// Every `.rs` file under `root/crates/*/src`, but those of `js-pool-fake` and
/// [`AGENTLESS_REFUSAL`], that names one of [`AGENTLESS_WORDS`], as `path: word`, in order.
fn agentless_mentions(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, found);
                continue;
            }
            let rel = path.strip_prefix(root).expect("under root");
            let rel = rel.display().to_string();
            if !rel.ends_with(".rs") || rel == AGENTLESS_REFUSAL {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source");
            for word in AGENTLESS_WORDS {
                if names(&text, word) {
                    found.push(format!("{rel}: {word}"));
                }
            }
        }
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).expect("crates") {
        let krate = entry.expect("entry").path();
        if krate.file_name().is_some_and(|name| name == "js-pool-fake") {
            continue;
        }
        walk(root, &krate.join("src"), &mut found);
    }
    found.sort();
    found
}

/// Asserts that no production source outside `js-pool-fake` names the removed agentless
/// pool: its `StaticPoolSource`, the `Built::Static` variant, or the `hosts_file` key,
/// except the refusal of the old kind in [`AGENTLESS_REFUSAL`]. Catches a half-removed
/// pool: a source, variant or key reader left in the server after the kind is gone.
#[test]
fn no_production_code_names_the_agentless_pool() {
    assert_eq!(agentless_mentions(&repo()), Vec::<String>::new());
}

/// Asserts that the scan reports each word in a production source, in a nested module
/// too, and passes the refusal, `js-pool-fake` and a longer name that only contains the
/// key. Catches a scan that reads no file, or one that would pass any tree.
#[test]
fn a_mention_of_the_agentless_pool_is_reported() {
    let tree = Tree::new("agentless-words");
    tree.put(
        "crates/js-lease-server/src/capacity.rs",
        "//! StaticPoolSource and Built::Static.\n",
    );
    tree.put(
        "crates/js-registry/src/deep/old.rs",
        "const KEY: &str = \"hosts_file\";\nfn hosts_file_budget() {}\n",
    );
    tree.put(
        "crates/js-pool-fake/src/old.rs",
        "struct StaticPoolSource; // hosts_file\n",
    );
    tree.put(AGENTLESS_REFUSAL, "// provider.hosts_file\n");
    tree.put("crates/js-cli/src/notes.txt", "hosts_file\n");
    assert_eq!(
        agentless_mentions(tree.root()),
        [
            "crates/js-lease-server/src/capacity.rs: Built::Static",
            "crates/js-lease-server/src/capacity.rs: StaticPoolSource",
            "crates/js-registry/src/deep/old.rs: hosts_file",
        ]
    );
    assert!(!names("fn hosts_file_budget()", "hosts_file"));
    assert!(names("\"hosts_file\"", "hosts_file"));
}
