//! The macos job's path filter in `.github/workflows/ci.yml` lists every workspace crate
//! the job builds or tests, read from the workflow text and from `cargo metadata`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// The packages the macos job builds and tests (`-p` in its cargo steps).
const BUILT: [&str; 3] = ["js-agent", "js-cli", "js-mac-pool"];

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The lines of the `changes` job's `PATHS: |` block.
fn filter_paths() -> Vec<String> {
    let text =
        std::fs::read_to_string(root().join(".github/workflows/ci.yml")).expect("read ci.yml");
    let mut lines = text.lines().skip_while(|l| l.trim() != "PATHS: |");
    let header = lines.next().expect("no PATHS block in ci.yml");
    let indent = header.len() - header.trim_start().len() + 2;
    lines
        .take_while(|l| l.len() > indent && l.starts_with(&" ".repeat(indent)))
        .map(|l| l.trim().to_owned())
        .collect()
}

/// Workspace package name to the workspace packages it depends on by path (normal, build
/// and dev dependencies), and to its directory relative to the root.
fn workspace() -> BTreeMap<String, (BTreeSet<String>, String)> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root())
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("metadata json");
    let packages = meta["packages"].as_array().expect("packages");
    let prefix = format!(
        "{}/",
        meta["workspace_root"].as_str().expect("workspace_root")
    );
    packages
        .iter()
        .map(|p| {
            let deps = p["dependencies"]
                .as_array()
                .expect("dependencies")
                .iter()
                .filter(|d| !d["path"].is_null())
                .map(|d| d["name"].as_str().expect("dep name").to_owned())
                .collect();
            let manifest = p["manifest_path"].as_str().expect("manifest_path");
            let dir = manifest
                .strip_prefix(&prefix)
                .and_then(|m| m.strip_suffix("Cargo.toml"))
                .expect("manifest under the workspace root");
            (
                p["name"].as_str().expect("name").to_owned(),
                (deps, dir.to_owned()),
            )
        })
        .collect()
}

/// The built packages and everything they depend on inside the workspace.
fn closure(ws: &BTreeMap<String, (BTreeSet<String>, String)>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<String> = BUILT.iter().map(|s| (*s).to_owned()).collect();
    while let Some(name) = todo.pop() {
        if seen.insert(name.clone()) {
            todo.extend(ws[&name].0.iter().filter(|d| ws.contains_key(*d)).cloned());
        }
    }
    seen
}

#[test]
fn every_crate_the_macos_job_builds_is_in_the_path_filter() {
    let ws = workspace();
    let paths = filter_paths();
    let missing: Vec<String> = closure(&ws)
        .into_iter()
        .filter(|name| !paths.contains(&ws[name].1))
        .collect();
    assert!(
        missing.is_empty(),
        "crates missing from the macos path filter in ci.yml: {missing:?}"
    );
}

#[test]
fn the_path_filter_keeps_the_files_that_pin_the_build() {
    let paths = filter_paths();
    for file in [
        "Cargo.lock",
        "Cargo.toml",
        "rust-toolchain.toml",
        ".github/workflows/ci.yml",
    ] {
        assert!(
            paths.iter().any(|p| p == file),
            "{file} is missing from the macos path filter"
        );
    }
}
