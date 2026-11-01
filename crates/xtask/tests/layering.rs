//! The layering rules (`layers.toml`) hold for this workspace, and each rule fails on a
//! planted defect.
//!
//! The defect tests copy the real manifests, `layers.toml` and `Cargo.lock` into a
//! scratch directory and change the copy, so they prove the real table catches the
//! defect, not a toy one. Nothing in the repository is edited.

use std::path::{Path, PathBuf};

use xtask::layering::{Error, FORBIDDEN, check, check_closure};

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch copy of the workspace's manifests, named `name`.
struct Copy {
    dir: PathBuf,
}

impl Copy {
    /// Copies the root files, every crate's manifest and every `.rs` file under its `src/`.
    fn new(name: &str) -> Copy {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("xtask-layering")
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear scratch");
        }
        let copy = Copy { dir };
        for file in ["Cargo.toml", "Cargo.lock", "layers.toml"] {
            copy.put(
                file,
                &std::fs::read_to_string(repo().join(file)).expect(file),
            );
        }
        for entry in std::fs::read_dir(repo().join("crates")).expect("crates") {
            let krate = entry.expect("entry").path();
            let rel = Path::new("crates").join(krate.file_name().expect("name"));
            let manifest = std::fs::read_to_string(krate.join("Cargo.toml")).expect("manifest");
            copy.put(&format!("{}/Cargo.toml", rel.display()), &manifest);
            copy.sources(&krate.join("src"), &rel.join("src"));
        }
        copy
    }

    /// Copies every `.rs` file under `from` to `to` in the scratch directory.
    fn sources(&self, from: &Path, to: &Path) {
        let Ok(entries) = std::fs::read_dir(from) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            let name = to.join(path.file_name().expect("name"));
            if path.is_dir() {
                self.sources(&path, &name);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("source");
                self.put(&name.display().to_string(), &text);
            }
        }
    }

    /// Writes `text` to `rel`.
    fn put(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    /// The text of `rel`.
    fn get(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).expect("read")
    }

    /// Replaces the one occurrence of `old` in `rel` by `new`.
    fn edit(&self, rel: &str, old: &str, new: &str) {
        let text = self.get(rel);
        assert_eq!(text.matches(old).count(), 1, "{old:?} in {rel}");
        self.put(rel, &text.replacen(old, new, 1));
    }

    /// The violations, as the lines they print.
    fn found(&self) -> Vec<String> {
        check(&self.dir)
            .expect("check runs")
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// The error the check fails with.
    fn error(&self) -> Error {
        check(&self.dir).expect_err("check fails")
    }
}

/// Asserts that this workspace passes: the real table, the real manifests, the real
/// lock file and every pure crate's `lib.rs`. A new crate that is in no layer, an edge
/// that points up, a pure crate without its `deny` or a second `js-types` fails here.
#[test]
fn this_workspace_follows_its_layers() {
    let found = check(&repo()).expect("check runs");
    assert!(found.is_empty(), "{found:#?}");
}

/// Asserts that the unmodified copy passes, so each defect test below fails for its
/// defect and not for the copy.
#[test]
fn the_copy_used_for_planted_defects_passes() {
    assert_eq!(Copy::new("clean").found(), Vec::<String>::new());
}

/// Asserts that the lowest layer depending on a higher one is reported with both
/// layers: `js-types` (types) on `js-lease` (runtime). Catches a lower layer growing
/// an edge to a higher one.
#[test]
fn a_dependency_up_a_layer_is_reported() {
    let copy = Copy::new("upward");
    copy.edit(
        "crates/js-types/Cargo.toml",
        "thiserror.workspace = true",
        "thiserror.workspace = true\njs-lease = { path = \"../js-lease\" }",
    );
    assert_eq!(
        copy.found(),
        ["js-types (layer types) depends on js-lease (layer runtime), a higher layer"]
    );
}

/// Asserts that the same edge written as a build dependency and as a target-specific
/// dependency is an edge too, and that a dev-dependency up a layer is not one. Catches a
/// reader that looks at `[dependencies]` only, and one that bans tests reaching up.
#[test]
fn build_and_target_dependencies_are_edges_and_dev_dependencies_are_not() {
    let copy = Copy::new("kinds");
    copy.edit(
        "crates/js-provider/Cargo.toml",
        "[lints]",
        "[build-dependencies]\njs-lease = { path = \"../js-lease\" }\n\n\
         [target.'cfg(unix)'.dependencies]\njs-lease-server = { path = \"../js-lease-server\" }\n\n\
         [dev-dependencies]\njs-cli = { path = \"../js-cli\" }\n\n[lints]",
    );
    assert_eq!(
        copy.found(),
        [
            "js-provider (layer pure) depends on js-lease (layer runtime), a higher layer",
            "js-provider (layer pure) depends on js-lease-server (layer server), a higher layer",
        ]
    );
}

/// Asserts that a renamed dependency (`package = "..."`) is judged by the package it
/// names, not by its local key. Catches a rename that hides an upward edge.
#[test]
fn a_renamed_dependency_is_judged_by_its_package() {
    let copy = Copy::new("renamed");
    copy.edit(
        "crates/js-types/Cargo.toml",
        "thiserror.workspace = true",
        "thiserror.workspace = true\nharmless = { path = \"../js-cli\", package = \"js-cli\" }",
    );
    assert_eq!(
        copy.found(),
        ["js-types (layer types) depends on js-cli (layer app), a higher layer"]
    );
}

/// Asserts that a crate in no layer, and a crate in two, are each reported. Catches a
/// moved crate that arrives without a place in the table.
#[test]
fn a_crate_must_be_in_exactly_one_layer() {
    let copy = Copy::new("membership");
    copy.edit("layers.toml", "\"js-client\", ", "");
    copy.edit(
        "layers.toml",
        "crates = [\"js-lease-server\"]",
        "crates = [\"js-lease-server\", \"js-lease\"]",
    );
    assert_eq!(
        copy.found(),
        [
            "js-client is in no layer of layers.toml",
            "js-lease is in more than one layer of layers.toml",
        ]
    );
}

/// Asserts that a name in a layer's `planned` list that is no workspace crate is fine,
/// and that listing the same crate twice in one layer is not a violation. Catches a
/// check that refuses the names reserved for crates that have not arrived.
#[test]
fn planned_names_need_no_crate() {
    let copy = Copy::new("planned");
    copy.edit(
        "layers.toml",
        "planned = [\"js-sim\",",
        "planned = [\"js-types\", \"js-sim\",",
    );
    // `js-types` is listed by two layers now: planned in `pure`, a crate of `types`.
    assert_eq!(
        copy.found(),
        ["js-types is in more than one layer of layers.toml"]
    );
    let copy = Copy::new("planned-twice");
    copy.edit(
        "layers.toml",
        "crates = [\"js-types\"]",
        "crates = [\"js-types\", \"js-types\"]",
    );
    assert_eq!(copy.found(), Vec::<String>::new());
}

/// Asserts that a pure crate that drops its `deny` line is reported, that a `deny` in a
/// comment does not count, and that `forbid` does. Catches a pure crate that is silently
/// allowed to read a clock or hash a collection.
#[test]
fn a_pure_crate_must_deny_the_disallowed_items() {
    let deny = "#![deny(clippy::disallowed_methods, clippy::disallowed_types)]";
    let copy = Copy::new("not-denying");
    copy.edit(
        "crates/js-types/src/lib.rs",
        deny,
        "// #![deny(clippy::disallowed_methods, clippy::disallowed_types)]",
    );
    assert_eq!(
        copy.found(),
        [
            "js-types is in a pure layer but crates/js-types/src/lib.rs does not deny clippy::disallowed_methods and clippy::disallowed_types"
        ]
    );

    let copy = Copy::new("half-denying");
    copy.edit(
        "crates/js-provider/src/lib.rs",
        deny,
        "#![deny(clippy::disallowed_methods)]",
    );
    assert_eq!(copy.found().len(), 1);

    // A lint name that only a trailing comment mentions is not denied.
    let copy = Copy::new("comment-denying");
    copy.edit(
        "crates/js-provider/src/lib.rs",
        deny,
        "#![deny(clippy::disallowed_methods)] // clippy::disallowed_types",
    );
    assert_eq!(
        copy.found(),
        [
            "js-provider is in a pure layer but crates/js-provider/src/lib.rs does not deny clippy::disallowed_methods and clippy::disallowed_types"
        ]
    );
    let copy = Copy::new("block-comment-denying");
    copy.edit(
        "crates/js-provider/src/lib.rs",
        deny,
        "#![deny(clippy::disallowed_methods)] /* clippy::disallowed_types */",
    );
    assert_eq!(copy.found().len(), 1);
    // A trailing comment after a complete attribute is fine.
    let copy = Copy::new("commented-deny");
    copy.edit(
        "crates/js-provider/src/lib.rs",
        deny,
        &format!("{deny} // purity"),
    );
    assert_eq!(copy.found(), Vec::<String>::new());

    let copy = Copy::new("forbidding");
    copy.edit(
        "crates/js-provider/src/lib.rs",
        deny,
        "#![forbid(clippy::disallowed_methods, clippy::disallowed_types)]",
    );
    assert_eq!(copy.found(), Vec::<String>::new());
}

/// Asserts that a second `js-types` in `Cargo.lock` (a git copy) and a dependency on it
/// that is not a path are each reported, in a member and in the root table. Catches the
/// kbf-side mistake the rule exists for: two copies of one crate, so two incompatible
/// `LeaseId` types.
#[test]
fn a_second_js_types_is_reported() {
    let copy = Copy::new("two-js-types");
    let lock = copy.get("Cargo.lock");
    copy.put(
        "Cargo.lock",
        &format!(
            "{lock}\n[[package]]\nname = \"js-types\"\nversion = \"0.1.0\"\nsource = \"git+https://example.invalid/js#abc\"\n"
        ),
    );
    assert_eq!(
        copy.found(),
        ["js-types must be in Cargo.lock once, from the workspace; found 2 entries"]
    );

    let copy = Copy::new("lock-without-js-types");
    let lock = copy.get("Cargo.lock");
    copy.put(
        "Cargo.lock",
        &lock.replace("name = \"js-types\"", "name = \"js-other\""),
    );
    assert_eq!(
        copy.found(),
        ["js-types must be in Cargo.lock once, from the workspace; found 0 entries"]
    );

    let copy = Copy::new("foreign-js-types");
    let lock = copy.get("Cargo.lock");
    copy.put(
        "Cargo.lock",
        &lock.replacen(
            "name = \"js-types\"\nversion",
            "source = \"git+https://example.invalid/js#abc\"\nname = \"js-types\"\nversion",
            1,
        ),
    );
    assert_eq!(copy.found().len(), 1, "{:?}", copy.found());

    let copy = Copy::new("registry-js-types");
    copy.edit(
        "crates/js-provider/Cargo.toml",
        "js-types = { path = \"../js-types\" }",
        "js-types = \"0.1\"",
    );
    assert_eq!(
        copy.found(),
        [
            "crates/js-provider/Cargo.toml depends on js-types by something other than a path or the workspace"
        ]
    );

    let copy = Copy::new("root-js-types");
    copy.edit(
        "Cargo.toml",
        "[workspace.dependencies]",
        "[workspace.dependencies]\njs-types = { git = \"https://example.invalid/js\", rev = \"abc\" }",
    );
    assert_eq!(
        copy.found(),
        ["Cargo.toml depends on js-types by something other than a path or the workspace"]
    );
}

/// Asserts that a dependency written as `name.workspace = true` counts as the workspace
/// one, so a kbf-style table does not trip the source rule when it names a path.
#[test]
fn a_workspace_dependency_on_js_types_is_accepted() {
    let copy = Copy::new("workspace-dep");
    copy.edit(
        "Cargo.toml",
        "[workspace.dependencies]",
        "[workspace.dependencies]\njs-types = { path = \"crates/js-types\" }",
    );
    copy.edit(
        "crates/js-provider/Cargo.toml",
        "js-types = { path = \"../js-types\" }",
        "js-types.workspace = true",
    );
    assert_eq!(copy.found(), Vec::<String>::new());
}

/// Asserts that every way the inputs can be wrong is an error that names the file, not a
/// pass. Catches a check that passes on a repository it could not read.
#[test]
fn unreadable_or_malformed_inputs_are_errors() {
    let reads = |copy: &Copy, file: &str| {
        let error = copy.error();
        assert!(
            matches!(&error, Error::Read { path, .. } if path.ends_with(file)),
            "{error:?}"
        );
    };
    let invalid = |copy: &Copy, file: &str, why: &str| {
        let error = copy.error();
        assert!(
            matches!(&error, Error::Invalid { path, why: w } if path.ends_with(file) && w.contains(why)),
            "{error:?}"
        );
        assert!(error.to_string().contains(file));
    };

    let copy = Copy::new("no-layers");
    std::fs::remove_file(copy.dir.join("layers.toml")).expect("remove");
    reads(&copy, "layers.toml");

    let copy = Copy::new("bad-layers");
    copy.put("layers.toml", "layer = 3");
    invalid(&copy, "layers.toml", "");

    let copy = Copy::new("no-root");
    std::fs::remove_file(copy.dir.join("Cargo.toml")).expect("remove");
    reads(&copy, "Cargo.toml");

    let copy = Copy::new("bad-root");
    copy.put("Cargo.toml", "[workspace");
    invalid(&copy, "Cargo.toml", "");

    let copy = Copy::new("no-members");
    copy.put("Cargo.toml", "[workspace]\n");
    invalid(&copy, "Cargo.toml", "no workspace.members");

    let copy = Copy::new("member-not-text");
    copy.put("Cargo.toml", "[workspace]\nmembers = [3]\n");
    invalid(&copy, "Cargo.toml", "not a string");

    let copy = Copy::new("no-parent");
    copy.put("Cargo.toml", "[workspace]\nmembers = [\"missing/*\"]\n");
    reads(&copy, "missing");

    let copy = Copy::new("no-member-manifest");
    copy.put("Cargo.toml", "[workspace]\nmembers = [\"nowhere\"]\n");
    reads(&copy, "nowhere/Cargo.toml");

    let copy = Copy::new("no-name");
    copy.put("crates/js-types/Cargo.toml", "[package]\n");
    invalid(&copy, "crates/js-types/Cargo.toml", "no package.name");

    let copy = Copy::new("no-lock");
    std::fs::remove_file(copy.dir.join("Cargo.lock")).expect("remove");
    reads(&copy, "Cargo.lock");

    let copy = Copy::new("no-lib");
    std::fs::remove_file(copy.dir.join("crates/js-types/src/lib.rs")).expect("remove");
    reads(&copy, "crates/js-types/src/lib.rs");
}

/// Asserts that explicit member directories work (not only `dir/*`), that a directory
/// under a glob without a manifest is skipped, and that a lock file without any package
/// is a violation, not a crash.
#[test]
fn explicit_members_and_stray_directories() {
    let copy = Copy::new("explicit");
    copy.edit(
        "Cargo.toml",
        "members = [\"crates/*\"]",
        "members = [\"crates/js-types\", \"crates/*\"]",
    );
    std::fs::create_dir_all(copy.dir.join("crates/not-a-crate")).expect("stray");
    assert_eq!(copy.found(), Vec::<String>::new());
    copy.put("Cargo.lock", "version = 4\n");
    assert_eq!(
        copy.found(),
        ["js-types must be in Cargo.lock once, from the workspace; found 0 entries"]
    );
}

/// Asserts that a binary target of a pure crate must deny the lints too (`src/main.rs`,
/// `src/bin/*.rs`, `src/bin/*/main.rs`, a `[[bin]]` path), and that one that does passes.
/// Catches a pure crate whose binary reads a clock because only its `lib.rs` was checked.
#[test]
fn binary_targets_of_a_pure_crate_must_deny_too() {
    let deny = "#![deny(clippy::disallowed_methods, clippy::disallowed_types)]";
    let report = |file: &str| {
        format!(
            "js-provider is in a pure layer but crates/js-provider/{file} does not deny clippy::disallowed_methods and clippy::disallowed_types"
        )
    };
    for (name, file) in [
        ("bin-main", "src/main.rs"),
        ("bin-dir", "src/bin/tool.rs"),
        ("bin-sub", "src/bin/other/main.rs"),
        ("bin-path", "tools/run.rs"),
    ] {
        let copy = Copy::new(name);
        copy.put(&format!("crates/js-provider/{file}"), "fn main() {}\n");
        if name == "bin-path" {
            copy.edit(
                "crates/js-provider/Cargo.toml",
                "[lints]",
                "[[bin]]\nname = \"run\"\npath = \"tools/run.rs\"\n\n[lints]",
            );
        }
        assert_eq!(copy.found(), [report(file)], "{name}");
        copy.put(
            &format!("crates/js-provider/{file}"),
            &format!("{deny}\nfn main() {{}}\n"),
        );
        assert_eq!(copy.found(), Vec::<String>::new(), "{name} with the deny");
    }
}

/// Asserts that an `allow`, `expect` or `warn` of a disallowed lint (or of the groups that
/// hold them) anywhere in a pure crate's sources is reported, in a nested module and in a
/// multi-line attribute, that a `deny` of them is not, and that a lint name outside an
/// attribute is not. Catches `#[allow(clippy::disallowed_methods)]` on a clock read.
#[test]
fn a_pure_crate_may_not_turn_a_purity_lint_down() {
    let relaxed = |file: &str, attribute: &str| {
        format!("{file} relaxes a purity lint in a pure crate: {attribute}")
    };
    let copy = Copy::new("allow-method");
    copy.put(
        "crates/js-agent-wire/src/inner/clock.rs",
        "#[allow(clippy::disallowed_methods)]\nfn now() { let _ = std::time::Instant::now(); }\n",
    );
    assert_eq!(
        copy.found(),
        [relaxed(
            "crates/js-agent-wire/src/inner/clock.rs",
            "#[allow(clippy::disallowed_methods"
        )]
    );

    let copy = Copy::new("expect-types");
    copy.put(
        "crates/js-types/src/map.rs",
        "#![expect(\n    unused,\n    clippy::disallowed_types\n)]\n",
    );
    assert_eq!(copy.found().len(), 1, "{:?}", copy.found());

    let copy = Copy::new("allow-group");
    copy.put("crates/js-provider/src/g.rs", "#![allow(clippy::style)]\n");
    assert_eq!(copy.found().len(), 1);

    let copy = Copy::new("deny-and-mention");
    copy.put(
        "crates/js-provider/src/ok.rs",
        "#[derive(Debug)]\nstruct A;\n// the lint clippy::disallowed_types is denied in lib.rs\n#[deny(clippy::disallowed_types)]\nmod m {}\n",
    );
    assert_eq!(copy.found(), Vec::<String>::new());

    // Files that are not Rust, and a lint name with no attribute before it, are not read as
    // attributes.
    let copy = Copy::new("not-attributes");
    copy.put(
        "crates/js-provider/src/notes.txt",
        "#[allow(clippy::all)]\n",
    );
    copy.put(
        "crates/js-provider/src/bin/notes.md",
        "#[allow(clippy::all)]\n",
    );
    copy.put(
        "crates/js-provider/src/c.rs",
        "// clippy::all is the group\n",
    );
    assert_eq!(copy.found(), Vec::<String>::new());

    // The same attribute in a crate that is not pure is allowed.
    let copy = Copy::new("allow-impure");
    copy.put(
        "crates/js-client/src/clock.rs",
        "#[allow(clippy::disallowed_methods)]\nfn f() {}\n",
    );
    assert_eq!(copy.found(), Vec::<String>::new());
}

/// Asserts that a dependency on a `kbf-*` crate is refused whether it is a path, a git or a
/// workspace dependency, and that an unlisted path dependency is refused too, in a normal,
/// a build and a target-specific table. Catches this repository depending on the one it
/// was lifted out of.
#[test]
fn a_dependency_that_is_neither_listed_nor_declared_is_reported() {
    let unknown = |manifest: &str, package: &str| {
        format!(
            "{manifest} depends on {package}, which is neither a crate listed in layers.toml nor a registry dependency of the root [workspace.dependencies]"
        )
    };
    let copy = Copy::new("kbf-path");
    copy.edit(
        "crates/js-client/Cargo.toml",
        "[lints]",
        "[dependencies.kbf-types]\npath = \"../kbf-types\"\n\n[lints]",
    );
    assert_eq!(
        copy.found(),
        [unknown("crates/js-client/Cargo.toml", "kbf-types")]
    );

    let copy = Copy::new("kbf-git");
    copy.edit(
        "crates/js-provider/Cargo.toml",
        "[lints]",
        "[build-dependencies]\nkbf-types = { git = \"https://example.invalid/kbf\" }\n\n[lints]",
    );
    assert_eq!(
        copy.found(),
        [unknown("crates/js-provider/Cargo.toml", "kbf-types")]
    );

    let copy = Copy::new("kbf-workspace");
    copy.edit(
        "Cargo.toml",
        "[workspace.dependencies]",
        "[workspace.dependencies]\nkbf-types = \"0.1\"",
    );
    copy.edit(
        "crates/js-provider/Cargo.toml",
        "[lints]",
        "[target.'cfg(unix)'.dependencies]\nkbf-types.workspace = true\n\n[lints]",
    );
    assert_eq!(
        copy.found(),
        [unknown("crates/js-provider/Cargo.toml", "kbf-types")]
    );

    let copy = Copy::new("unlisted-path");
    copy.edit(
        "crates/js-client/Cargo.toml",
        "[lints]",
        "[dependencies.mystery]\npath = \"../mystery\"\n\n[lints]",
    );
    assert_eq!(
        copy.found(),
        [unknown("crates/js-client/Cargo.toml", "mystery")]
    );

    // A kbf crate as a dev-dependency is a test's business (deny.toml bans it anyway).
    let copy = Copy::new("kbf-dev");
    copy.edit(
        "crates/js-client/Cargo.toml",
        "[lints]",
        "[dev-dependencies.kbf-types]\npath = \"../kbf-types\"\n\n[lints]",
    );
    assert_eq!(copy.found(), Vec::<String>::new());
}

/// Asserts that an external dependency a member declares by version instead of
/// `workspace = true` is reported. Catches a moved crate that brings its own version of a
/// dependency, so the two lock files diverge.
#[test]
fn an_external_dependency_must_come_from_the_workspace_table() {
    let copy = Copy::new("own-version");
    copy.edit(
        "crates/js-client/Cargo.toml",
        "[lints]",
        "[dependencies.toml]\nversion = \"1\"\n\n[lints]",
    );
    assert_eq!(
        copy.found(),
        [
            "crates/js-client/Cargo.toml depends on toml without `workspace = true`; external dependencies are declared once in the root [workspace.dependencies]"
        ]
    );
}

/// Asserts that the real workspace's pure crates reach no forbidden crate, that the walk
/// can see them (a runtime crate does reach `tokio`, so the check does not pass on an empty
/// walk), and that a planted transitive dependency is reported with its chain: the pure
/// `js-provider` made to depend on the server `js-lease-server`, which reaches `tokio`.
#[test]
fn pure_crates_reach_no_runtime_or_network_crate() {
    let metadata = xtask::layering::cargo_metadata(&repo()).expect("cargo metadata");
    assert_eq!(check_closure(&repo(), &metadata).expect("walk"), []);

    // The walk is not vacuous: from a runtime crate the walk finds forbidden crates.
    let mut planted = metadata.clone();
    let id_of = |m: &serde_json::Value, name: &str| {
        m["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("{name} in metadata"))["id"]
            .clone()
    };
    let lease = id_of(&metadata, "js-lease-server");
    let provider = id_of(&metadata, "js-provider");
    let node = planted["resolve"]["nodes"]
        .as_array_mut()
        .expect("nodes")
        .iter_mut()
        .find(|n| n["id"] == provider)
        .expect("node");
    node["deps"]
        .as_array_mut()
        .expect("deps")
        .push(serde_json::json!({
            "name": "js_lease_server",
            "pkg": lease,
            "dep_kinds": [{ "kind": null, "target": null }],
        }));
    let found: Vec<String> = check_closure(&repo(), &planted)
        .expect("walk")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(!found.is_empty(), "the planted chain is found");
    for line in &found {
        assert!(
            line.starts_with("a pure crate reaches a forbidden crate: js-")
                && line.contains("js-provider -> js-lease-server -> "),
            "{line}"
        );
        assert!(
            FORBIDDEN
                .iter()
                .any(|f| line.ends_with(&format!(" -> {f}"))),
            "{line}"
        );
    }

    // The same edge as a dev-dependency or a build dependency is not linked into the library.
    for kind in ["dev", "build"] {
        let mut other = metadata.clone();
        other["resolve"]["nodes"]
            .as_array_mut()
            .expect("nodes")
            .iter_mut()
            .find(|n| n["id"] == provider)
            .expect("node")["deps"]
            .as_array_mut()
            .expect("deps")
            .push(serde_json::json!({
                "name": "js_lease_server",
                "pkg": lease,
                "dep_kinds": [{ "kind": kind, "target": null }],
            }));
        assert_eq!(check_closure(&repo(), &other).expect("walk"), [], "{kind}");
    }

    // Metadata that is not what `cargo metadata` prints, a directory cargo cannot start
    // in and a manifest it refuses are errors, not a pass.
    let wrong = check_closure(&repo(), &serde_json::Value::Null).expect_err("null metadata");
    assert!(matches!(&wrong, Error::Invalid { why, .. } if why.starts_with("cargo metadata")));
    let missing = repo().join("no-such-directory");
    assert!(matches!(
        xtask::layering::cargo_metadata(&missing),
        Err(Error::Read { .. })
    ));
    let broken = Copy::new("broken-manifest");
    broken.put("Cargo.toml", "[workspace");
    assert!(matches!(
        xtask::layering::cargo_metadata(&broken.dir),
        Err(Error::Invalid { .. })
    ));

    // A pure crate that is not a workspace member is an error, not a pass.
    let copy = Copy::new("closure-renamed");
    copy.edit(
        "layers.toml",
        "crates = [\"js-types\"]",
        "crates = [\"js-renamed\"]",
    );
    assert!(check_closure(&copy.dir, &metadata).is_err());
}
