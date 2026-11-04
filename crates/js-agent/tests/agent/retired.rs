//! What was removed with the Podman network path stays removed: the operator script that set
//! the network and its firewall up, and the document that described it.

use std::fs;
use std::path::{Path, PathBuf};

/// The repository's root, two levels above this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file below `dir` (none when `dir` does not exist), but nothing below `skip`.
fn files(dir: &Path, skip: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        match (path == skip, path.is_dir()) {
            (true, _) => {}
            (false, true) => files(&path, skip, found),
            (false, false) => found.push(path),
        }
    }
}

/// Asserts that `deploy/js-agent/egress-network.sh` does not exist and that no file under
/// `packaging/`, `.github/`, `deploy/` and `docs/` (but `docs/design/`, which keeps the
/// history) names `egress-network` or `docs/egress.md`; and that the walk read the
/// workflows and the docs, so it cannot pass on an empty tree. Catches the script coming
/// back, and a package spec, a workflow or a document that still ships, calls or links it.
#[test]
fn the_shell_script_has_no_remaining_reference() {
    let root = root();
    assert!(!root.join("deploy/js-agent/egress-network.sh").exists());
    let skip = root.join("docs/design");
    let mut found = Vec::new();
    for dir in ["packaging", ".github", "deploy", "docs"] {
        files(&root.join(dir), &skip, &mut found);
    }
    for dir in [".github/workflows", "docs"] {
        let read = found.iter().filter(|path| path.starts_with(root.join(dir)));
        assert!(read.count() > 0, "nothing read under {dir}");
    }
    for path in &found {
        let text = fs::read(path).expect("a readable file");
        let text = String::from_utf8_lossy(&text);
        for name in ["egress-network", "docs/egress.md"] {
            assert!(!text.contains(name), "{} names {name}", path.display());
        }
    }
}
