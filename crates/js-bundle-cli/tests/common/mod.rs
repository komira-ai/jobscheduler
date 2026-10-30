//! Helpers shared by the test binaries: running the program, a sample tree, and reading
//! and rewriting a layout directory.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use js_oci::Digest;
use js_testkit::dir::TestDir;
use serde_json::Value;

/// What a run of the program did.
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// The digest `pack` printed.
    pub fn digest(&self) -> String {
        self.stdout.trim().to_owned()
    }
}

pub fn work(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

pub fn text(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

pub fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

pub fn run(args: &[String]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_js-bundle"))
        .args(args)
        .output()
        .expect("start js-bundle");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// A tree of five entries: a program, a symlink to it, a library directory and a file.
pub fn sample_tree(root: &Path, marker: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::write(root.join("bin/tool"), format!("#!/bin/sh\necho {marker}\n")).unwrap();
    fs::set_permissions(root.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("lib/data.txt"), format!("data {marker}\n")).unwrap();
    symlink("tool", root.join("bin/alias")).unwrap();
}

/// The flags of a `pack` of `trees` into `out`, each tree with a licence and a source.
pub fn pack_args(out: &Path, trees: &[(&str, &Path)]) -> Vec<String> {
    let mut args = strings(&[
        "pack",
        "--out",
        &text(out),
        "--name",
        "sample",
        "--bundle-version",
        "1.0.0",
        "--os-version",
        "14.0",
    ]);
    for (name, dir) in trees {
        args.extend(strings(&[
            "--tree",
            &format!("{name}={}", text(dir)),
            "--license",
            &format!("{name}=MIT"),
            "--source-url",
            &format!("{name}=https://example.invalid/{name}"),
        ]));
    }
    args
}

/// Packs the sample tree `marker` as tree `main` into `<work>/<name>` and returns the
/// layout and the digest printed.
pub fn packed(work: &Path, name: &str, marker: &str) -> (PathBuf, String) {
    let tree = work.join(format!("{name}-tree"));
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, marker);
    let out = work.join(name);
    let run = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    (out, run.digest())
}

pub fn verify_args(layout: &Path, scratch: &Path) -> Vec<String> {
    strings(&["verify", &text(layout), "--scratch", &text(scratch)])
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

pub fn blob_path(layout: &Path, digest: &str) -> PathBuf {
    let hex = digest.strip_prefix("sha256:").expect("a sha256 digest");
    layout.join("blobs/sha256").join(hex)
}

pub fn manifest_digest(layout: &Path) -> String {
    let index = read_json(&layout.join("index.json"));
    index["manifests"][0]["digest"].as_str().unwrap().to_owned()
}

pub fn manifest(layout: &Path) -> Value {
    read_json(&blob_path(layout, &manifest_digest(layout)))
}

pub fn config(layout: &Path) -> Value {
    let digest = manifest(layout)["config"]["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    read_json(&blob_path(layout, &digest))
}

/// The digest of layer `index` of the bundle.
pub fn layer_digest(layout: &Path, index: usize) -> String {
    manifest(layout)["layers"][index]["digest"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Edits the manifest and the config of a bundle, writes both back as blobs of the
/// digests they now have, and points the manifest and `index.json` at them. Nothing else
/// changes, so a layout that results is well formed except for the edit.
pub fn rewrite(layout: &Path, edit: impl FnOnce(&mut Value, &mut Value)) {
    let mut manifest = manifest(layout);
    let mut config = config(layout);
    edit(&mut manifest, &mut config);
    let config_bytes = serde_json::to_vec(&config).unwrap();
    let config_digest = Digest::of(&config_bytes).to_string();
    fs::write(blob_path(layout, &config_digest), &config_bytes).unwrap();
    manifest["config"]["digest"] = config_digest.into();
    manifest["config"]["size"] = config_bytes.len().into();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = Digest::of(&manifest_bytes).to_string();
    fs::write(blob_path(layout, &manifest_digest), &manifest_bytes).unwrap();
    let mut index = read_json(&layout.join("index.json"));
    index["manifests"][0]["digest"] = manifest_digest.into();
    index["manifests"][0]["size"] = manifest_bytes.len().into();
    fs::write(
        layout.join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
}
