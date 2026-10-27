//! The release decisions that need no git and no cargo: versions, the shipped set, the
//! files a stamp rewrites.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::stamp::{StampArgs, run as stamp, set_lock_version, set_manifest_version};
use super::*;

fn v(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

fn dirs(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|d| (*d).to_owned()).collect()
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-release-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The repository root, two levels above this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Catches a version parser that accepts what is not a release version: a short or long
/// one, a leading zero or plus sign (which would name a tag no release made), a suffix.
#[test]
fn a_version_is_three_plain_decimal_numbers() {
    assert_eq!("1.22.3".parse(), Ok(v(1, 22, 3)));
    for bad in [
        "1.2",
        "1.2.3.4",
        "1.x.3",
        "01.2.3",
        "1.2.+3",
        "1.2.3-rc1",
        "",
    ] {
        assert_eq!(
            bad.parse::<Version>(),
            Err(Error::NotAVersion(bad.to_owned())),
            "{bad:?}"
        );
    }
    assert_eq!(v(1, 22, 3).to_string(), "1.22.3");
    assert_eq!(v(1, 22, 3).tag(), "v1.22.3");
    assert_eq!(
        Error::NotAVersion("x".to_owned()).to_string(),
        "not a version MAJOR.MINOR.PATCH: \"x\""
    );
}

/// Catches reading a tag that is not a release: no `v`, too few parts, a suffix.
#[test]
fn only_v_and_a_version_is_a_release_tag() {
    assert_eq!(Version::parse_tag("v0.1.0"), Some(v(0, 1, 0)));
    assert_eq!(Version::parse_tag("0.1.0"), None);
    assert_eq!(Version::parse_tag("v0.1"), None);
    assert_eq!(Version::parse_tag("v0.1.0-rc1"), None);
    assert_eq!(Version::parse_tag("vnext"), None);
}

/// Catches ordering tags as text, where v0.1.9 would be above v0.1.10 and the next
/// release would repeat a version; and a tag that is not a release counting as one.
#[test]
fn the_latest_tag_is_the_highest_version_not_the_last_text() {
    assert_eq!(latest(["v0.1.9", "v0.1.10", "v0.1.2"]), Some(v(0, 1, 10)));
    assert_eq!(
        latest(["v0.9.0", "v0.10.0", "v1.0.0-rc1", "vnext"]),
        Some(v(0, 10, 0))
    );
    assert_eq!(latest(["nightly", "v1", "stable"]), None);
    assert_eq!(latest([]), None);
}

/// Catches each way the next version can go wrong: no tag must give the workspace version
/// (not 0.0.1 or a patch above it); the workspace version at or below the latest tag must
/// give one patch above the tag (so a merged PR never has to touch Cargo.toml); only a
/// workspace version above the tag is used as is (a minor or major bump), and then the
/// next release after it is a patch above it.
#[test]
fn the_next_version_is_a_patch_above_the_tag_unless_cargo_is_above_it() {
    assert_eq!(next(None, v(0, 1, 0)), v(0, 1, 0));
    assert_eq!(next(Some(v(0, 1, 0)), v(0, 1, 0)), v(0, 1, 1));
    assert_eq!(next(Some(v(0, 1, 7)), v(0, 1, 0)), v(0, 1, 8));
    assert_eq!(next(Some(v(0, 1, 7)), v(0, 2, 0)), v(0, 2, 0));
    assert_eq!(next(Some(v(0, 1, 7)), v(1, 0, 0)), v(1, 0, 0));
    assert_eq!(next(Some(v(0, 2, 0)), v(0, 2, 0)), v(0, 2, 1));
    assert_eq!(next(Some(v(0, 1, 9)), v(0, 1, 10)), v(0, 1, 10));
}

const METADATA: &str = r#"{
  "workspace_root": "/w",
  "packages": [
    {"name": "app", "manifest_path": "/w/crates/app/Cargo.toml", "dependencies": [
      {"name": "lib", "kind": null, "path": "/w/crates/lib"},
      {"name": "gen", "kind": "build", "path": "/w/crates/gen"},
      {"name": "fake", "kind": "dev", "path": "/w/crates/fake"},
      {"name": "serde", "kind": null, "path": null}
    ]},
    {"name": "lib", "manifest_path": "/w/crates/lib/Cargo.toml", "dependencies": [
      {"name": "app", "kind": null, "path": "/w/crates/app"}
    ]},
    {"name": "gen", "manifest_path": "/w/crates/gen/Cargo.toml", "dependencies": []},
    {"name": "fake", "manifest_path": "/w/crates/fake/Cargo.toml", "dependencies": []},
    {"name": "tools", "manifest_path": "/w/crates/tools/Cargo.toml", "dependencies": [
      {"name": "app", "kind": null, "path": "/w/crates/app"}
    ]},
    {"name": "stray", "manifest_path": "/elsewhere/Cargo.toml", "dependencies": []},
    {"name": "dangling", "manifest_path": "/w/crates/dangling/Cargo.toml", "dependencies": [
      {"name": "ghost", "kind": null, "path": "/w/crates/ghost"}
    ]}
  ]
}"#;

/// Catches a shipped set that is wrong in any direction: a normal or build dependency left
/// out (a change to it would not release), a dev-dependency or a crate that merely depends
/// on a shipped one counted (a test-support change would release), a crate with no path
/// (an external one) treated as a member, and a dependency cycle looping forever.
#[test]
fn the_shipped_crates_are_the_roots_and_their_normal_and_build_dependencies() {
    let metadata = Metadata::parse(METADATA).expect("metadata");
    assert_eq!(
        metadata.shipped_dirs(&strings(&["app"])),
        Ok(dirs(&["crates/app", "crates/gen", "crates/lib"]))
    );
    assert_eq!(
        metadata.shipped_dirs(&strings(&["gen", "fake"])),
        Ok(dirs(&["crates/fake", "crates/gen"]))
    );
}

/// Catches a fail-open graph: a root or path dependency the metadata does not know, and a
/// member outside the workspace root, must refuse rather than be left out of the shipped
/// set (which would silently stop releasing its changes).
#[test]
fn an_unknown_or_foreign_crate_refuses_instead_of_shipping_less() {
    let metadata = Metadata::parse(METADATA).expect("metadata");
    assert_eq!(
        metadata.shipped_dirs(&strings(&["nope"])),
        Err(Error::UnknownCrate("nope".to_owned()))
    );
    assert_eq!(
        metadata.shipped_dirs(&strings(&["dangling"])),
        Err(Error::UnknownCrate("ghost".to_owned()))
    );
    assert_eq!(
        metadata.shipped_dirs(&strings(&["stray"])),
        Err(Error::OutsideWorkspace("stray".to_owned()))
    );
    assert_eq!(
        Error::UnknownCrate("x".to_owned()).to_string(),
        "crate x is not a workspace member"
    );
    assert!(matches!(Metadata::parse("{"), Err(Error::Metadata(_))));
    assert!(matches!(Metadata::parse("{}"), Err(Error::Metadata(_))));
}

/// Catches the decision rule: a docs, markdown, workflow or tests-only change must not
/// ship; source, a manifest or build script of a shipped crate, and the root build inputs
/// must; a crate whose name merely starts with a shipped crate's directory must not.
#[test]
fn only_compiled_files_of_shipped_crates_and_the_root_inputs_ship() {
    let shipped = dirs(&["crates/js-cli", "crates/js-types"]);
    for path in [
        "crates/js-cli/src/main.rs",
        "crates/js-cli/src/deep/mod.rs",
        "crates/js-cli/src/notes.md",
        "crates/js-cli/Cargo.toml",
        "crates/js-types/build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
    ] {
        assert!(is_shipped(path, &shipped), "{path} ships");
    }
    for path in [
        "docs/release.md",
        "README.md",
        "crates/js-cli/README.md",
        "crates/js-cli/tests/cli.rs",
        "crates/js-cli/benches/x.rs",
        "crates/js-cli-extra/src/lib.rs",
        "crates/js-cli",
        "crates/js-cli/srcs/x.rs",
        "crates/xtask/src/lib.rs",
        "crates/js-conformance/src/lib.rs",
        ".github/workflows/release.yml",
        "coverage-baseline",
        "deny.toml",
        "crates/js-cli/Cargo.lock",
    ] {
        assert!(!is_shipped(path, &shipped), "{path} does not ship");
    }
}

/// Catches reporting the wrong path, or one when nothing ships: the first changed path
/// that ships is the reason; a range with only unshipped paths has none.
#[test]
fn the_first_shipped_path_is_named() {
    let shipped = dirs(&["crates/a"]);
    let mixed = strings(&["docs/x.md", "crates/a/src/lib.rs", "Cargo.lock"]);
    assert_eq!(first_shipped(&mixed, &shipped), Some("crates/a/src/lib.rs"));
    assert_eq!(first_shipped(&strings(&["docs/x.md"]), &shipped), None);
    assert_eq!(first_shipped(&[], &shipped), None);
}

const MANIFEST: &str = r#"[workspace]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
version = "9.9.9"
"#;

/// Catches reading the workspace version from the wrong place or accepting a manifest
/// without one.
#[test]
fn the_workspace_version_is_read_from_the_manifest() {
    assert_eq!(workspace_version(MANIFEST), Ok(v(0, 1, 0)));
    assert!(matches!(
        workspace_version("[package]\n"),
        Err(Error::Manifest(_))
    ));
    let pre = "[workspace.package]\nversion = \"1.0.0-rc1\"\n";
    assert_eq!(
        workspace_version(pre),
        Err(Error::NotAVersion("1.0.0-rc1".to_owned()))
    );
}

/// Catches a stamp that edits another table's `version` (a dependency named version, a
/// crate's own), loses the lines around it, or silently does nothing when there is no
/// line to set.
#[test]
fn the_stamp_sets_only_the_workspace_package_version() {
    let set = set_manifest_version(MANIFEST, v(0, 2, 5)).expect("set");
    assert_eq!(
        set,
        MANIFEST.replace("version = \"0.1.0\"", "version = \"0.2.5\"")
    );
    let before = "[package]\nversion = \"7.7.7\"\n\n[workspace.package]\nversion.workspace = true\nversion = \"0.1.0\"\n";
    assert_eq!(
        set_manifest_version(before, v(0, 1, 1)),
        Ok("[package]\nversion = \"7.7.7\"\n\n[workspace.package]\nversion.workspace = true\nversion = \"0.1.1\"\n".to_owned())
    );
    assert_eq!(
        set_manifest_version("[workspace.package]\nedition = \"2024\"\n", v(0, 1, 1)),
        Err(Error::NoVersionLine)
    );
    assert_eq!(
        set_manifest_version("[package]\nversion = \"1.0.0\"\n", v(0, 1, 1)),
        Err(Error::NoVersionLine)
    );
    // Only the first version line of the table is set.
    assert_eq!(
        set_manifest_version(
            "[workspace.package]\nversion = \"1.0.0\"\nversion = \"2.0.0\"\n",
            v(0, 1, 1)
        ),
        Ok("[workspace.package]\nversion = \"0.1.1\"\nversion = \"2.0.0\"\n".to_owned())
    );
    // A version line without an equals sign is not one.
    assert_eq!(
        set_manifest_version("[workspace.package]\nversioned = \"1\"\n", v(0, 1, 1)),
        Err(Error::NoVersionLine)
    );
}

const LOCK: &str = r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "serde",
]

[[package]]
name = "serde"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"

[[package]]
name = "other"
version = "2.0.0"
"#;

/// Catches a lock file the build would reject: a workspace member (no `source`) left at
/// the old version fails `cargo build --locked`; an external crate that happens to have
/// the same number must not be touched, and neither must a member at another version.
#[test]
fn the_stamp_moves_workspace_members_in_the_lock_file_and_nothing_else() {
    let set = set_lock_version(LOCK, v(0, 1, 0), v(0, 1, 4));
    let expected = LOCK.replacen("version = \"0.1.0\"", "version = \"0.1.4\"", 1);
    assert_eq!(set, expected);
    assert_eq!(set_lock_version(LOCK, v(0, 1, 0), v(0, 1, 0)), LOCK);
    // And back again.
    assert_eq!(set_lock_version(&set, v(0, 1, 4), v(0, 1, 0)), LOCK);
}

/// Catches the stamp verb leaving the two files out of step, writing without reading
/// both first, and each failure (a missing file, an unreadable version, a file that
/// cannot be written) going unreported.
#[test]
fn stamp_rewrites_both_files_and_reports_each_failure() {
    let dir = scratch("stamp");
    std::fs::write(dir.join("Cargo.toml"), MANIFEST).expect("manifest");
    std::fs::write(dir.join("Cargo.lock"), LOCK).expect("lock");
    let args = |to, dir: &Path| StampArgs {
        to,
        repo_dir: dir.to_owned(),
    };
    let said = stamp(&args(v(0, 3, 0), &dir)).expect("stamp");
    assert!(said.ends_with(": 0.1.0 -> 0.3.0"), "{said}");
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("read");
    assert_eq!(workspace_version(&manifest), Ok(v(0, 3, 0)));
    let lock = std::fs::read_to_string(dir.join("Cargo.lock")).expect("read");
    assert_eq!(lock, set_lock_version(LOCK, v(0, 1, 0), v(0, 3, 0)));

    // No manifest, no lock, an unreadable version, a version line to set but no
    // permission to write either file.
    let empty = scratch("stamp-empty");
    assert!(matches!(
        stamp(&args(v(1, 0, 0), &empty)),
        Err(Error::Io { .. })
    ));
    std::fs::write(empty.join("Cargo.toml"), MANIFEST).expect("manifest");
    assert!(matches!(
        stamp(&args(v(1, 0, 0), &empty)),
        Err(Error::Io { .. })
    ));
    std::fs::write(empty.join("Cargo.lock"), LOCK).expect("lock");
    std::fs::write(
        empty.join("Cargo.toml"),
        "[workspace.package]\nedition = \"2024\"\n",
    )
    .expect("manifest");
    assert!(matches!(
        stamp(&args(v(1, 0, 0), &empty)),
        Err(Error::Manifest(_))
    ));
    std::fs::write(
        empty.join("Cargo.toml"),
        "[workspace.package]\nversion.workspace = true\n",
    )
    .expect("manifest");
    assert!(matches!(
        stamp(&args(v(1, 0, 0), &empty)),
        Err(Error::Manifest(_))
    ));
    // A version only a dotted key sets: read by toml, but there is no line to rewrite.
    std::fs::write(
        empty.join("Cargo.toml"),
        "[workspace]\npackage.version = \"0.1.0\"\n",
    )
    .expect("manifest");
    assert_eq!(stamp(&args(v(1, 0, 0), &empty)), Err(Error::NoVersionLine));
    std::fs::write(empty.join("Cargo.toml"), MANIFEST).expect("manifest");
    let mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("permissions");
    };
    mode(&empty.join("Cargo.toml"), 0o444);
    assert!(matches!(
        stamp(&args(v(1, 0, 0), &empty)),
        Err(Error::Io { .. })
    ));
    mode(&empty.join("Cargo.toml"), 0o644);
    mode(&empty.join("Cargo.lock"), 0o444);
    assert!(matches!(
        stamp(&args(v(1, 0, 0), &empty)),
        Err(Error::Io { .. })
    ));
}

/// Catches `release stamp` and the lock file drifting apart in this repository: the stamp
/// moves every package without a source, which is right only while every workspace member
/// has the workspace version. A member with a version of its own would be stamped wrong
/// and `cargo build --locked` would fail in the release build, not here.
#[test]
fn every_workspace_package_in_the_lock_has_the_workspace_version() {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("Cargo.lock");
    let version = workspace_version(&manifest).expect("workspace version");
    let members = lock
        .split("[[package]]")
        .skip(1)
        .filter(|package| !package.contains("\nsource = "))
        .count();
    assert!(members > 10, "{members} workspace packages in the lock");
    let at_version = format!("\nversion = \"{version}\"\n");
    for package in lock
        .split("[[package]]")
        .skip(1)
        .filter(|package| !package.contains("\nsource = "))
    {
        assert!(package.contains(&at_version), "{package}");
    }
    // And a stamp leaves none behind at the old version.
    let stamped = set_lock_version(&lock, version, v(99, 0, 0));
    assert!(!stamped.contains(&at_version));
}

/// Catches the shipped set of this repository being wrong: it must hold the three shipped
/// binaries and what they link, and none of the crates that only test or build the
/// repository, whose changes must not release.
#[test]
fn this_repositorys_shipped_crates() {
    let root = repo_root();
    let out = std::process::Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&root)
        .output()
        .expect("cargo metadata");
    assert!(out.status.success());
    let json = String::from_utf8(out.stdout).expect("utf-8");
    let roots = strings(&["js-lease-server", "js-cli", "js-agent"]);
    let shipped = Metadata::parse(&json)
        .expect("metadata")
        .shipped_dirs(&roots)
        .expect("shipped");
    for member in [
        "js-agent",
        "js-agent-wire",
        "js-cli",
        "js-client",
        "js-lease",
        "js-lease-server",
        "js-pool-static",
        "js-provider",
        "js-registry",
        "js-types",
    ] {
        assert!(
            shipped.contains(&format!("crates/{member}")),
            "{member} ships"
        );
    }
    // The fake provider is a dev-dependency of the binaries: only their tests link it.
    for member in ["xtask", "js-coverage", "js-conformance", "js-provider-fake"] {
        assert!(
            !shipped.contains(&format!("crates/{member}")),
            "{member} does not"
        );
    }
    assert!(is_shipped("crates/js-agent/src/main.rs", &shipped));
    assert!(!is_shipped("crates/js-conformance/src/lib.rs", &shipped));
}

/// Catches an io error that loses the path or the reason.
#[test]
fn an_io_error_names_the_file() {
    let missing = scratch("io").join("absent");
    let Err(error) = read(&missing) else {
        panic!("a missing file is an error")
    };
    let text = error.to_string();
    assert!(text.starts_with(&missing.display().to_string()), "{text}");
    assert!(text.contains("No such file"), "{text}");
    assert!(matches!(
        write(&missing.join("x"), ""),
        Err(Error::Io { .. })
    ));
}
