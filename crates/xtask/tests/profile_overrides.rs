//! The per-package profile overrides in the root manifest name real workspace packages.
//!
//! Cargo only warns when a `[profile.<p>.package.<name>]` table names a package that
//! does not exist, so a misspelled name keeps CI green and silently drops the
//! optimisation. These tests turn that warning into a failure, and check that the
//! packages whose simulations need the optimisation have it on the test profile. A
//! dependency (not a workspace member) may carry an override only when it is listed in
//! [`DEPENDENCIES`] and locked in `Cargo.lock`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The workspace packages whose simulations run in CI test builds, and those on the path
/// of the sim cell's job front (its server's placement pass and job tables).
const OPTIMISED: [&str; 12] = [
    "js-api-gateway",
    "js-caps",
    "js-jobs",
    "js-lease-server",
    "js-registry",
    "js-sched",
    "js-sched-placer",
    "js-sim",
    "js-sim-cell",
    "js-sizing",
    "js-state",
    "js-types",
];

/// The dependencies (not workspace members) optimised on the test profile: the SQLite the
/// job tables are read through.
const DEPENDENCIES: [&str; 2] = ["libsqlite3-sys", "rusqlite"];

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

/// The package names `Cargo.lock` locks.
fn locked() -> BTreeSet<String> {
    let text = std::fs::read_to_string(repo().join("Cargo.lock")).expect("Cargo.lock");
    let lock: toml::Table = text.parse().expect("Cargo.lock parses");
    let packages = lock["package"].as_array().expect("locked packages");
    let names = packages.iter().map(|p| p["name"].as_str().expect("name"));
    names.map(str::to_owned).collect()
}

/// The names of `listed` that `locked` does not hold.
fn unlocked<'a>(listed: &[&'a str], locked: &BTreeSet<String>) -> Vec<&'a str> {
    listed
        .iter()
        .copied()
        .filter(|name| !locked.contains(*name))
        .collect()
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
fn every_override_names_a_workspace_package_or_a_listed_dependency() {
    let mut allowed = members();
    allowed.extend(DEPENDENCIES.map(str::to_owned));
    assert_eq!(unmatched(&root(), &allowed), Vec::new());
}

#[test]
fn every_listed_dependency_is_locked_and_optimised_on_the_test_profile() {
    assert_eq!(unlocked(&DEPENDENCIES, &locked()), Vec::<&str>::new());
    let manifest = root();
    for package in DEPENDENCIES {
        assert!(
            optimised(&manifest, "test", package),
            "profile.test.package.{package} lacks opt-level = 2"
        );
    }
}

#[test]
fn a_misspelled_dependency_name_is_reported() {
    let locked = locked();
    assert_eq!(
        unlocked(&["rusqlite", "libsqlite3-syss"], &locked),
        ["libsqlite3-syss"]
    );
}

#[test]
fn the_simulation_packages_are_optimised_on_the_test_profile_only() {
    let manifest = root();
    for package in OPTIMISED {
        assert!(
            optimised(&manifest, "test", package),
            "profile.test.package.{package} lacks opt-level = 2"
        );
    }
    let others: Vec<_> = overrides(&manifest)
        .into_iter()
        .filter(|(profile, _)| profile != "test")
        .collect();
    assert_eq!(others, Vec::new(), "only `test` carries package overrides");
}

#[test]
fn the_coverage_profile_is_dev_and_the_coverage_job_builds_with_it() {
    let manifest = root();
    let coverage = &manifest["profile"]["coverage"];
    assert_eq!(coverage["inherits"].as_str(), Some("dev"));
    assert!(coverage.get("opt-level").is_none());
    let ci = std::fs::read_to_string(repo().join(".github/workflows/ci.yml")).expect("ci.yml");
    // The job runs the tests under cargo-nextest, where `--profile` names a nextest
    // profile; the cargo profile is `--cargo-profile`.
    let run = ci
        .lines()
        .find(|l| l.contains("llvm-cov nextest --workspace"))
        .expect("the coverage run line");
    assert!(run.contains("--cargo-profile coverage"), "{run}");
    assert!(ci.contains("root=target/llvm-cov-target/coverage"));
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
