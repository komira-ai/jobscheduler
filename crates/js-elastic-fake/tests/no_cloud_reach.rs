//! Nothing in the workspace can reach a cloud host or call a purchase API except through the
//! provider crate's guarded live path.
//!
//! The scan reads the sources, tests and examples of every crate, and the manifests and the
//! lock file, and fails on
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

/// The text files under `dir`, recursively, with their paths relative to `root`.
fn read_tree(root: &Path, dir: &Path, found: &mut Vec<File>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_tree(root, &path, found);
        } else if let Ok(text) = fs::read_to_string(&path) {
            let relative = path.strip_prefix(root).unwrap();
            let path = relative.to_string_lossy().replace('\\', "/");
            found.push(File { path, text });
        }
    }
}

/// The files the scan reads: each crate's `src`, `tests` and `examples`, each crate's
/// manifest, the workspace manifest and the lock file.
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
    let mut crates: Vec<PathBuf> = fs::read_dir(root.join("crates"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    crates.sort();
    for dir in crates {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = fs::read_to_string(&manifest) {
            let path = manifest.strip_prefix(&root).unwrap();
            found.push(File {
                path: path.to_string_lossy().replace('\\', "/"),
                text,
            });
        }
        for part in ["src", "tests", "examples"] {
            read_tree(&root, &dir.join(part), &mut found);
        }
    }
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
