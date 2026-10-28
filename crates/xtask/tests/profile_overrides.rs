//! The per-package profile overrides in the root manifest name real workspace packages.
//!
//! Cargo only warns when a `[profile.<p>.package.<name>]` table names a package that
//! does not exist, so a misspelled name keeps CI green and silently drops the
//! optimisation. These tests turn that warning into a failure, and check that the
//! packages whose replay simulations need the optimisation have it on both profiles.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The packages whose simulations run in CI test builds.
const OPTIMISED: [&str; 4] = ["js-sched", "js-sim", "js-sizing", "js-types"];

/// The profiles that build test code.
const PROFILES: [&str; 2] = ["dev", "test"];

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The root manifest, parsed.
fn root() -> toml::Table {
    let text = std::fs::read_to_string(repo().join("Cargo.toml")).expect("root manifest");
    text.parse().expect("root manifest parses")
}

/// The package names of every crate under `crates/`.
fn members() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(repo().join("crates")).expect("crates") {
        let manifest = entry.expect("entry").path().join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).expect("crate manifest");
        let table: toml::Table = text.parse().expect("crate manifest parses");
        let name = table["package"]["name"].as_str().expect("package name");
        names.insert(name.to_owned());
    }
    names
}

/// The `(profile, package)` pairs `manifest` has override tables for.
fn overrides(manifest: &toml::Table) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let Some(profiles) = manifest.get("profile").and_then(|p| p.as_table()) else {
        return found;
    };
    for (profile, body) in profiles {
        let Some(packages) = body.get("package").and_then(|p| p.as_table()) else {
            continue;
        };
        for package in packages.keys() {
            found.push((profile.clone(), package.clone()));
        }
    }
    found
}

/// The overrides in `manifest` that name no member of `members`.
fn unmatched(manifest: &toml::Table, members: &BTreeSet<String>) -> Vec<(String, String)> {
    overrides(manifest)
        .into_iter()
        .filter(|(_, package)| !members.contains(package))
        .collect()
}

/// Whether `profile` sets `opt-level = 2` for `package` in `manifest`.
fn optimised(manifest: &toml::Table, profile: &str, package: &str) -> bool {
    manifest
        .get("profile")
        .and_then(|p| p.get(profile))
        .and_then(|p| p.get("package"))
        .and_then(|p| p.get(package))
        .and_then(|p| p.get("opt-level"))
        .and_then(|v| v.as_integer())
        == Some(2)
}

/// A manifest with one override table per `(profile, package)` pair given.
fn manifest_with(pairs: &[(&str, &str)]) -> toml::Table {
    let mut text = String::new();
    for (profile, package) in pairs {
        text.push_str(&format!(
            "[profile.{profile}.package.{package}]\nopt-level = 2\n"
        ));
    }
    text.parse().expect("parses")
}

#[test]
fn every_override_names_a_workspace_package() {
    assert_eq!(unmatched(&root(), &members()), Vec::new());
}

#[test]
fn the_simulation_packages_are_optimised_on_both_profiles() {
    let manifest = root();
    for profile in PROFILES {
        for package in OPTIMISED {
            assert!(
                optimised(&manifest, profile, package),
                "profile.{profile}.package.{package} lacks opt-level = 2"
            );
        }
    }
}

#[test]
fn a_misspelled_package_name_is_reported() {
    let members = members();
    let manifest = manifest_with(&[("dev", "js-sched"), ("test", "js-sizng")]);
    assert_eq!(
        unmatched(&manifest, &members),
        vec![("test".to_owned(), "js-sizng".to_owned())]
    );
}

#[test]
fn a_missing_or_weaker_override_is_reported() {
    let manifest = manifest_with(&[("dev", "js-sched")]);
    assert!(optimised(&manifest, "dev", "js-sched"));
    assert!(!optimised(&manifest, "test", "js-sched"));
    assert!(!optimised(&manifest, "dev", "js-sim"));
    let weaker: toml::Table = "[profile.dev.package.js-sched]\nopt-level = 1\n"
        .parse()
        .expect("parses");
    assert!(!optimised(&weaker, "dev", "js-sched"));
}
