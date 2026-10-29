//! No source or test of this crate names the production API host except one: the default
//! address of the `lambdalabs` kind (`src/kind.rs`), which a table's `api_url` overrides.
//! Everywhere else the address is the caller's to give, and a test that could reach it
//! would be a test that spends.

use std::fs;
use std::path::{Path, PathBuf};

fn files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

/// Whether `text` holds `needle` in any letter case.
fn names(text: &str, needle: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[test]
fn the_scan_is_blind_to_letter_case_in_neither_direction() {
    assert!(names("see Example.COM here", "example.com"));
    assert!(names("see example.com here", "EXAMPLE.com"));
    assert!(!names("see example.org here", "example.com"));
}

#[test]
fn nothing_names_the_production_host() {
    // Built from parts so that this file does not hold them either.
    let needles = [["lambda", ".ai"].concat(), ["lambdalabs", ".com"].concat()];
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    files(&root.join("src"), &mut found);
    files(&root.join("tests"), &mut found);
    assert!(found.len() > 10, "the scan found {} files", found.len());
    let default_address = root.join("src").join("kind.rs");
    for path in found {
        if path == default_address {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in &needles {
            assert!(!names(&text, needle), "{} names {needle}", path.display());
        }
    }
}
