//! The vendored Remote Execution API files `mac-farm` is generated from
//! (`proto/third_party`) are the upstream files `proto/third_party/PINS` names.
//!
//! Catches:
//! - a vendored file edited in place (even one comment) without its pin bumped, so the tree no
//!   longer matches the upstream commit PINS names;
//! - a vendored file added without a PINS line, or a PINS line whose file is gone;
//! - one directory that mixes files from two upstream commits or repositories;
//! - a PINS line that is not `<directory> <repository> <commit> <sha256> <path>`;
//! - a PINS file that loses a file `build.rs` compiles or a LICENSE, which would make every
//!   check above pass on less than is built.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

/// One line of PINS.
struct Pin {
    repo: String,
    commit: String,
    sha256: String,
}

fn third_party() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("proto/third_party")
}

/// The lines of the PINS file of `base`, by `<directory>/<path>`, or the problems of its lines.
fn read_pins(base: &Path) -> Result<BTreeMap<String, Pin>, Vec<String>> {
    let text = fs::read_to_string(base.join("PINS")).map_err(|e| vec![format!("PINS: {e}")])?;
    let mut pins = BTreeMap::new();
    let mut problems = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
        match fields[..] {
            [dir, repo, commit, sha256, path] if hex(commit, 40) && hex(sha256, 64) => {
                let pin = Pin {
                    repo: repo.to_owned(),
                    commit: commit.to_owned(),
                    sha256: sha256.to_ascii_lowercase(),
                };
                if pins.insert(format!("{dir}/{path}"), pin).is_some() {
                    problems.push(format!("PINS lists {dir}/{path} twice"));
                }
            }
            _ => problems.push(format!(
                "PINS line {} is not `<directory> <repository> <commit> <sha256> <path>`",
                n + 1
            )),
        }
    }
    if problems.is_empty() {
        Ok(pins)
    } else {
        Err(problems)
    }
}

/// Every regular file under `dir`, as a path relative to `base` with `/` separators.
fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read vendored directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            walk(base, &path, out);
        } else {
            let rel = path.strip_prefix(base).expect("under base");
            let parts: Vec<_> = rel
                .iter()
                .map(|c| c.to_str().expect("UTF-8 path"))
                .collect();
            out.push(parts.join("/"));
        }
    }
}

/// The files `build.rs` compiles or imports, and the licences they come under.
const BUILT: [&str; 13] = [
    "googleapis/LICENSE",
    "googleapis/google/api/annotations.proto",
    "googleapis/google/api/client.proto",
    "googleapis/google/api/field_behavior.proto",
    "googleapis/google/api/http.proto",
    "googleapis/google/api/launch_stage.proto",
    "googleapis/google/bytestream/bytestream.proto",
    "googleapis/google/longrunning/operations.proto",
    "googleapis/google/rpc/error_details.proto",
    "googleapis/google/rpc/status.proto",
    "remote-apis/LICENSE",
    "remote-apis/build/bazel/remote/execution/v2/remote_execution.proto",
    "remote-apis/build/bazel/semver/semver.proto",
];

/// What is wrong with the vendored tree at `base` against its PINS.
fn problems(base: &Path) -> Vec<String> {
    let pins = match read_pins(base) {
        Ok(pins) => pins,
        Err(problems) => return problems,
    };
    let mut files = Vec::new();
    walk(base, base, &mut files);
    files.retain(|f| f != "PINS");
    files.sort();
    let mut problems = Vec::new();
    for file in &files {
        let Some(pin) = pins.get(file) else {
            problems.push(format!("{file}: vendored but not listed in PINS"));
            continue;
        };
        let actual = hex::encode(Sha256::digest(fs::read(base.join(file)).expect("read")));
        if actual != pin.sha256 {
            problems.push(format!(
                "{file}: sha256 {actual} but PINS says {} ({} at {})",
                pin.sha256, pin.repo, pin.commit
            ));
        }
    }
    for key in pins.keys() {
        if !files.contains(key) {
            problems.push(format!("{key}: listed in PINS but not vendored"));
        }
    }
    let mut origin: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for (key, pin) in &pins {
        let dir = key.split('/').next().unwrap_or_default();
        let seen = origin.entry(dir).or_insert((&pin.repo, &pin.commit));
        if *seen != (pin.repo.as_str(), pin.commit.as_str()) {
            problems.push(format!(
                "{key}: {dir} mixes upstream repositories or commits"
            ));
        }
    }
    for key in BUILT {
        if !pins.contains_key(key) {
            problems.push(format!("PINS has no line for {key}"));
        }
    }
    problems
}

/// A copy of the vendored tree in a scratch directory.
fn copy(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-mac-farm-pins"),
        name,
    );
    let mut files = Vec::new();
    walk(&third_party(), &third_party(), &mut files);
    for file in files {
        let to = dir.path().join(&file);
        fs::create_dir_all(to.parent().expect("parent")).expect("mkdir");
        fs::copy(third_party().join(&file), to).expect("copy");
    }
    dir
}

/// Asserts that the vendored files are exactly the pinned upstream files, every file the build
/// reads among them.
#[test]
fn every_vendored_file_matches_its_pin() {
    assert_eq!(problems(&third_party()), Vec::<String>::new());
}

/// Asserts that an edited file, an unlisted file, a listed file that is gone, two commits in one
/// directory, a malformed line and a lost pin of a built file are each named.
#[test]
fn each_way_of_leaving_the_pins_is_named() {
    let edited = copy("edited");
    let status = edited.path().join("googleapis/google/rpc/status.proto");
    let mut text = fs::read_to_string(&status).expect("read");
    text.push_str("// edited\n");
    fs::write(&status, &text).expect("write");
    let got = problems(edited.path());
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("googleapis/google/rpc/status.proto: sha256 "),
        "{got:?}"
    );

    let unlisted = copy("unlisted");
    fs::write(unlisted.path().join("googleapis/extra.proto"), "").expect("write");
    assert_eq!(
        problems(unlisted.path()),
        ["googleapis/extra.proto: vendored but not listed in PINS"]
    );

    let gone = copy("gone");
    fs::remove_file(
        gone.path()
            .join("remote-apis/build/bazel/semver/semver.proto"),
    )
    .expect("remove");
    assert_eq!(
        problems(gone.path()),
        ["remote-apis/build/bazel/semver/semver.proto: listed in PINS but not vendored"]
    );

    let pins_of = |dir: &TestDir| fs::read_to_string(dir.path().join("PINS")).expect("read");
    let mixed = copy("mixed");
    let text = pins_of(&mixed).replacen(
        "googleapis https://github.com/googleapis/googleapis ab5c1b0777e6f1f7c89bd0eb55b05532d97b68e0 e79e",
        "googleapis https://github.com/googleapis/googleapis 0000000000000000000000000000000000000000 e79e",
        1,
    );
    fs::write(mixed.path().join("PINS"), text).expect("write");
    assert_eq!(
        problems(mixed.path()),
        [
            "googleapis/google/api/annotations.proto: googleapis mixes upstream repositories or commits"
        ]
    );

    let malformed = copy("malformed");
    let mut text = pins_of(&malformed);
    text.push_str("googleapis https://example.invalid not-a-commit\n");
    fs::write(malformed.path().join("PINS"), text).expect("write");
    let got = problems(malformed.path());
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].ends_with("is not `<directory> <repository> <commit> <sha256> <path>`"));

    let lost = copy("lost");
    fs::remove_file(lost.path().join("googleapis/LICENSE")).expect("remove");
    let text: String = pins_of(&lost)
        .lines()
        .filter(|l| !l.ends_with(" LICENSE") || l.starts_with("remote-apis"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(lost.path().join("PINS"), text).expect("write");
    assert_eq!(
        problems(lost.path()),
        ["PINS has no line for googleapis/LICENSE"]
    );
}
