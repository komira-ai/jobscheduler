//! The workflows that build a release binary name the binary with `--bin`, so that the test-only
//! binaries of a package (the fake update hook of js-agent) are never built, and never shipped,
//! with it. Read from the files as they are in the repository.

use std::path::Path;

const WORKFLOWS: &str = ".github/workflows";

/// The `cargo build` commands in `text` that build for release, comments left out.
fn release_builds(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter(|line| line.contains("cargo build") && line.contains("--release"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_release_build_of_a_package_names_its_binary() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(WORKFLOWS);
    let mut builds = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "yml") {
            let text = std::fs::read_to_string(&path).unwrap();
            builds.extend(release_builds(&text).into_iter().map(|b| (path.clone(), b)));
        }
    }
    // The agent package and the release workflow at least.
    assert!(builds.len() >= 2, "{builds:?}");
    for (path, build) in &builds {
        // xtask has the one binary and is never shipped.
        if build.contains("-p ") && !build.contains("-p xtask") {
            assert!(
                build.contains("--bin "),
                "{} builds a package without --bin: {build}",
                path.display()
            );
        }
    }
    assert!(
        builds
            .iter()
            .any(|(_, build)| build.contains("-p js-agent") && build.contains("--bin js-agent"))
    );
}

#[test]
fn the_check_sees_a_release_build_without_a_binary_name() {
    let text = "      run: cargo build --locked --release -p js-agent\n# cargo build --release\n";
    let found = release_builds(text);
    assert_eq!(found.len(), 1);
    assert!(!found[0].contains("--bin "));
}
