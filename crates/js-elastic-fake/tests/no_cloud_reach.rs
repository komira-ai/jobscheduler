//! Nothing in the workspace can reach a cloud host or call a purchase API except through the
//! provider crate's guarded live path.
//!
//! The scan reads every file under every crate directory (sources, tests, examples, build
//! scripts, benches, fixtures, nested crates; non-UTF-8 files lossily), the workspace manifest
//! and the lock file, and a second test requires every `Cargo.toml` in the repository to lie
//! where the scan reads. It fails on
//!
//! - the name of a cloud's API host anywhere but the one place a provider keeps its default
//!   address;
//! - the name of a purchase API (the path or method that makes a machine) anywhere but in
//!   the provider crate that guards it;
//! - the names of the provider's raw API types outside that crate, because holding
//!   one is holding a way to make a machine that skips the guard;
//! - a dependency on a cloud vendor's SDK.
//!
//! It is the sibling of `js-provider-lambdalabs/tests/no_live_host.rs`, which looks inside
//! one crate; this one looks at all of them, so a new provider crate, or a test that
//! reaches a real cloud, is caught wherever it is written. A provider PR that adds a real
//! API adds its crate to the allowlists below, in the same change, and that edit is the
//! review point.

use std::fs;
use std::path::{Path, PathBuf};

/// A file of the workspace: its path from the workspace root, with `/` separators, and
/// its text.
struct File {
    path: String,
    text: String,
}

/// One thing that must not be named, and the only places that may name it.
struct Rule {
    what: &'static str,
    needles: Vec<String>,
    /// Path prefixes of the files allowed to name it.
    allowed: Vec<&'static str>,
}

/// The provider crate whose live path is guarded by its config (`launch` disabled and in
/// dry run by default, `max_machines`, a launch interval), and whose tests never name the
/// production host.
const LAMBDA_CRATE: &str = "crates/js-provider-lambdalabs/";

/// The one file that may name the production host: the default address of the
/// `lambdalabs` kind, which a pool's `api_url` overrides.
const LAMBDA_DEFAULT_ADDRESS: &str = "crates/js-provider-lambdalabs/src/kind.rs";

/// Parts joined, so that this file does not hold what it looks for.
fn joined(parts: &[&str]) -> String {
    parts.concat()
}

fn rules() -> Vec<Rule> {
    vec![
        Rule {
            what: "the host of a cloud API",
            needles: vec![
                joined(&["amazon", "aws.com"]),
                joined(&["google", "apis.com"]),
                joined(&["cloud.", "google.com"]),
                joined(&["lambda", ".ai"]),
                joined(&["lambda", "labs.com"]),
                joined(&["az", "ure.com"]),
                joined(&["windows", ".net"]),
                joined(&["oracle", "cloud.com"]),
            ],
            allowed: vec![LAMBDA_DEFAULT_ADDRESS],
        },
        Rule {
            what: "a purchase API",
            needles: vec![
                joined(&["instance-operations", "/launch"]),
                joined(&["Run", "Instances"]),
                joined(&["run", "_instances"]),
                joined(&["Create", "Fleet"]),
                joined(&["queued", "Resources"]),
                joined(&["CreateQueued", "Resource"]),
            ],
            allowed: vec![LAMBDA_CRATE],
        },
        Rule {
            what: "the raw API of a provider",
            needles: vec![joined(&["Lambda", "Api"]), joined(&["Http", "Api"])],
            allowed: vec![LAMBDA_CRATE],
        },
        Rule {
            what: "a cloud vendor's SDK",
            needles: vec![
                joined(&["aws", "-sdk"]),
                joined(&["aws", "-config"]),
                joined(&["rus", "oto"]),
                joined(&["google", "-cloud"]),
                joined(&["azure", "_core"]),
            ],
            allowed: Vec::new(),
        },
    ]
}

/// Whether `text` holds `needle` in any letter case.
fn names(text: &str, needle: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Every violation in `files`, one line each.
fn violations(files: &[File], rules: &[Rule]) -> Vec<String> {
    let mut found = Vec::new();
    for file in files {
        for rule in rules {
            let allowed = rule
                .allowed
                .iter()
                .any(|prefix| file.path.starts_with(prefix));
            for needle in &rule.needles {
                if !allowed && names(&file.text, needle) {
                    found.push(format!("{} names {} ({needle})", file.path, rule.what));
                }
            }
        }
    }
    found
}

/// Every file under `dir`, recursively, with its path relative to `root`. A file that is not
/// UTF-8 is read lossily, never skipped: a name hidden in a binary-looking file is still
/// found. Symlinks are not followed, and `target` and `.git` are not entered.
fn read_tree(root: &Path, dir: &Path, found: &mut Vec<File>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let kind = fs::symlink_metadata(&path).unwrap().file_type();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if kind.is_dir() {
            if name != "target" && name != ".git" {
                read_tree(root, &path, found);
            }
        } else if kind.is_file() {
            let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
            let relative = path.strip_prefix(root).unwrap();
            let path = relative.to_string_lossy().replace('\\', "/");
            found.push(File { path, text });
        }
    }
}

/// The manifests in `paths` (root-relative, `/` separators) that no scan reaches: the scan
/// reads the workspace manifest and everything under `crates/`, so a `Cargo.toml` anywhere
/// else (`xtask/`, `tools/`, a new top-level directory) is a crate the scan would never read.
fn stray_manifests(paths: &[String]) -> Vec<String> {
    let reached = |path: &&String| path.as_str() == "Cargo.toml" || path.starts_with("crates/");
    let manifests = paths.iter().filter(|path| path.ends_with("Cargo.toml"));
    manifests.filter(|path| !reached(path)).cloned().collect()
}

/// The files the scan reads: every file under each crate directory (sources, tests,
/// examples, `build.rs`, benches, fixtures, nested crates), the workspace manifest and the
/// lock file.
fn workspace() -> Vec<File> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = Vec::new();
    for top in ["Cargo.toml", "Cargo.lock"] {
        let text = fs::read_to_string(root.join(top)).unwrap();
        found.push(File {
            path: top.to_owned(),
            text,
        });
    }
    read_tree(&root, &root.join("crates"), &mut found);
    found
}

fn file(path: &str, text: &str) -> File {
    File {
        path: path.to_owned(),
        text: text.to_owned(),
    }
}

/// Asserts that the scan can fail, on planted text: a cloud host in a crate that is not
/// the provider's, a purchase path in a test of another crate, the provider's raw API type
/// in the server, a vendor SDK in a manifest and in the lock file. Each is named; the same
/// words in the places the allowlist gives are not; and the match is blind to letter case
/// in neither direction. It catches a scan whose rules were emptied or whose allowlist
/// grew too wide.
#[test]
fn the_scan_fails_on_planted_reaches_and_passes_the_guarded_places() {
    let rules = rules();
    let host = joined(&["https://api.", "lambda", "labs.com/v1"]);
    let planted = [
        file("crates/js-lease-server/tests/x.rs", &host),
        file(
            "crates/js-scaler/tests/y.rs",
            &joined(&["post(\"/", "instance-operations", "/launch\")"]),
        ),
        file(
            "crates/js-lease-server/src/z.rs",
            &joined(&["use js_provider_lambdalabs::", "Http", "Api;"]),
        ),
        file(
            "crates/js-lease-server/Cargo.toml",
            &joined(&["aws", "-sdk", "-ec2 = \"1\""]),
        ),
        file("Cargo.lock", &joined(&["name = \"", "rus", "oto\""])),
    ];
    let found = violations(&planted, &rules);
    assert_eq!(found.len(), 5, "{found:#?}");
    assert!(found[0].contains("crates/js-lease-server/tests/x.rs"));
    assert!(found[0].contains("host"));
    assert!(found[1].contains("purchase"));
    assert!(found[2].contains("raw API"));
    assert!(found[3].contains("SDK"));
    assert!(found[4].contains("SDK"));

    let guarded = [
        file(LAMBDA_DEFAULT_ADDRESS, &host),
        file(
            "crates/js-provider-lambdalabs/src/http.rs",
            &joined(&[
                "post(\"/",
                "instance-operations",
                "/launch\")",
                "Http",
                "Api",
            ]),
        ),
        file(
            "crates/js-scaler/src/lib.rs",
            "a pure crate that names nothing",
        ),
    ];
    assert_eq!(violations(&guarded, &rules), Vec::<String>::new());

    let other_file_of_the_provider = file("crates/js-provider-lambdalabs/tests/t.rs", &host);
    assert_eq!(violations(&[other_file_of_the_provider], &rules).len(), 1);

    assert!(names("see Example.COM here", "example.com"));
    assert!(names("see example.com here", "EXAMPLE.com"));
    assert!(!names("see example.org here", "example.com"));
}

/// Asserts that no source, test, example, manifest or lock file of the workspace names a
/// cloud host outside the provider's default address, a purchase API outside the provider
/// crate, the provider's raw API outside it, or a vendor SDK; and that the scan did read
/// the workspace. It catches the day a test is pointed at a real cloud, a second crate
/// grows a way to launch a machine, or an SDK is added to the build.
#[test]
fn nothing_in_the_workspace_reaches_a_cloud_host_or_a_purchase_api_unguarded() {
    let files = workspace();
    assert!(files.len() > 300, "the scan read {} files", files.len());
    for must in [
        "Cargo.lock",
        LAMBDA_DEFAULT_ADDRESS,
        "crates/js-scaler/src/lib.rs",
        "crates/js-elastic-fake/tests/no_cloud_reach.rs",
    ] {
        assert!(files.iter().any(|f| f.path == must), "{must} was not read");
    }
    let found = violations(&files, &rules());
    assert_eq!(found, Vec::<String>::new());
}

/// Asserts the placement guard can fail: a manifest in `xtask/`, in `tools/` or in a new
/// top-level directory is named as unreached, while the workspace manifest, a crate's and a
/// nested crate's are not. It catches a crate placed where the scan does not look.
#[test]
fn a_manifest_outside_the_scanned_directories_is_named() {
    let paths: Vec<String> = [
        "Cargo.toml",
        "crates/a/Cargo.toml",
        "crates/a/nested/Cargo.toml",
        "xtask/Cargo.toml",
        "tools/x/Cargo.toml",
        "crates/a/src/lib.rs",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(
        stray_manifests(&paths),
        ["xtask/Cargo.toml", "tools/x/Cargo.toml"]
    );
}

/// Asserts that every `Cargo.toml` in the repository (outside `target` and `.git`) lies in a
/// directory the scan reads. It catches a crate added outside `crates/`, which the scan
/// would never see.
#[test]
fn every_manifest_in_the_repository_is_in_a_scanned_directory() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut everything = Vec::new();
    read_tree_all(&root, &root, &mut everything);
    assert!(everything.iter().any(|path| path == "Cargo.toml"));
    assert_eq!(stray_manifests(&everything), Vec::<String>::new());
}

/// The relative paths of all files under `dir`, `target` and `.git` not entered, symlinks not
/// followed.
fn read_tree_all(root: &Path, dir: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let kind = fs::symlink_metadata(&path).unwrap().file_type();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if kind.is_dir() && name != "target" && name != ".git" {
            read_tree_all(root, &path, found);
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).unwrap();
            found.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Asserts that a name inside a file that is not UTF-8 is still found: the scan reads such
/// files lossily instead of skipping them. It catches a scan that a binary-looking file
/// defeats.
#[test]
fn a_name_in_a_file_that_is_not_utf8_is_still_found() {
    let dir = std::env::temp_dir().join(format!("no-cloud-reach-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut bytes = vec![0xff, 0xfe, 0x00];
    bytes.extend_from_slice(joined(&["Run", "Instances"]).as_bytes());
    fs::write(dir.join("blob.bin"), bytes).unwrap();
    let mut found = Vec::new();
    read_tree(&dir, &dir, &mut found);
    assert_eq!(violations(&found, &rules()).len(), 1);
    fs::remove_file(dir.join("blob.bin")).unwrap();
    fs::remove_dir(&dir).unwrap();
}
