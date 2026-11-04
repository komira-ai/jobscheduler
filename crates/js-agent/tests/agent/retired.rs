//! What was removed with the Podman network path stays removed: the operator script that set
//! the network and its firewall up, and the document that described it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::support::Scratch;

/// The repository's root, two levels above this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file below `dir`, but nothing below `skip`; any directory that cannot be read is an
/// error.
fn files(dir: &Path, skip: &Path, found: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        match (path == skip, path.is_dir()) {
            (true, _) => {}
            (false, true) => files(&path, skip, found)?,
            (false, false) => found.push(path),
        }
    }
    Ok(())
}

/// [`files`] of `dir`, where a `dir` that does not exist holds nothing when it is
/// `optional`; every other failure names the directory.
fn walk(dir: &Path, optional: bool, skip: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    match files(dir, skip, found) {
        Ok(()) => Ok(()),
        Err(why) if optional && why.kind() == io::ErrorKind::NotFound && !dir.exists() => Ok(()),
        Err(why) => Err(format!("{}: {why}", dir.display())),
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
    // `deploy/` held only the script and went with it; the others must be there.
    for (dir, optional) in [
        ("packaging", false),
        (".github", false),
        ("deploy", true),
        ("docs", false),
    ] {
        let walked = walk(&root.join(dir), optional, &skip, &mut found);
        assert_eq!(walked, Ok(()), "{dir}");
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

/// Asserts that the walk of the reference test fails, naming the path, on a directory it
/// cannot read (here a file in its place) and on a required directory that is missing, and
/// takes a missing optional directory as empty. Catches a walk that treats an unreadable
/// subtree as one with no reference in it.
#[test]
fn the_reference_walk_fails_on_a_directory_it_cannot_read() {
    let dir = Scratch::new("retired-walk");
    let plain = dir.write("plain", "egress-network", 0o644);
    let missing = dir.path("missing");
    let skip = dir.path("skip");
    let mut found = Vec::new();
    let said = walk(&plain, true, &skip, &mut found).expect_err("a file is not a directory");
    assert!(
        said.starts_with(&format!("{}: ", plain.display())),
        "{said}"
    );
    let said = walk(&missing, false, &skip, &mut found).expect_err("a required directory");
    assert!(
        said.starts_with(&format!("{}: ", missing.display())),
        "{said}"
    );
    assert_eq!(walk(&missing, true, &skip, &mut found), Ok(()));
    assert!(found.is_empty(), "{found:?}");
}
