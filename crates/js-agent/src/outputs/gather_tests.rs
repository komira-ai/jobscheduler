//! The gathering of a job's outputs, on a scratch directory as the job left it: what is an
//! output, what is missing, and each refusal with its exact reason.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;

use js_logstore::outputs::OutputFailure;
use js_testkit::dir::TestDir;

use super::gather::{Caps, Gathered, gather};

/// Caps no test reaches but the one it names.
const ROOMY: Caps = Caps {
    file_bytes: 1 << 20,
    total_bytes: 1 << 30,
    files: 100,
    entries: 1000,
    failures: 100,
    depth: 16,
};

/// Writes `text` at `path` below `root`, making its directories.
fn put(root: &Path, path: &str, text: &str) {
    let at = root.join(path);
    fs::create_dir_all(at.parent().unwrap()).unwrap();
    fs::write(at, text).unwrap();
}

/// `paths` as a declaration.
fn declared(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

/// What a gathering found, readably: each file's path and what it holds, the missing paths,
/// and each failure as `path: reason`.
fn seen(found: Gathered) -> (Vec<(String, String)>, Vec<String>, Vec<String>) {
    let files = found
        .files
        .into_iter()
        .map(|mut gotten| {
            let mut text = String::new();
            gotten.file.read_to_string(&mut text).unwrap();
            (gotten.path.to_string(), text)
        })
        .collect();
    let failed = found
        .failed
        .iter()
        .map(|OutputFailure { path, reason }| format!("{path}: {reason}"))
        .collect();
    (files, found.missing, failed)
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Asserts that a declared file and the files of a declared directory's tree (in the order of
/// their names) are outputs, in the order of the declaration, and that a declared path the job
/// did not make (a missing file, a missing directory on the way) is missing. It catches a
/// directory's tree gathered in no fixed order and a missing output taken for a failure.
#[test]
fn files_and_trees_are_gathered_in_order_and_absences_are_missing() {
    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-order");
    let root = dir.path();
    put(root, "out/sub/deep.txt", "d");
    put(root, "out/report.json", "r");
    put(root, "one.txt", "1");
    let found = gather(
        root,
        &declared(&["out", "one.txt", "absent", "nodir/x"]),
        ROOMY,
    );
    assert_eq!(
        seen(found),
        (
            pairs(&[
                ("out/report.json", "r"),
                ("out/sub/deep.txt", "d"),
                ("one.txt", "1")
            ]),
            strings(&["absent", "nodir/x"]),
            Vec::new()
        )
    );
}

/// Asserts that no link is followed and nothing but a regular file of one name is read: a
/// symbolic link as the output and as a directory on the way to it, and a second name of a
/// file, are each refused `not-a-file`; in a tree, a link is refused the same way and a name
/// that is not an output path's segment (a space) is refused `name`, shown with `?`; the
/// regular file beside them is still gathered. It catches an upload of a file the job pointed
/// at outside its directory.
#[test]
fn links_second_names_and_bad_names_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-links");
    let root = dir.path();
    put(root, "real.txt", "real");
    put(root, "d/f", "behind a link");
    symlink(root.join("real.txt"), root.join("link")).unwrap();
    symlink(root.join("d"), root.join("via")).unwrap();
    put(root, "hard1", "two names");
    fs::hard_link(root.join("hard1"), root.join("hard2")).unwrap();
    put(root, "tree/ok", "kept");
    symlink(root.join("real.txt"), root.join("tree/l")).unwrap();
    put(root, "tree/a b", "spaced");
    let found = gather(
        root,
        &declared(&["link", "via/f", "hard1", "real.txt", "tree"]),
        ROOMY,
    );
    assert_eq!(
        seen(found),
        (
            pairs(&[("real.txt", "real"), ("tree/ok", "kept")]),
            Vec::new(),
            strings(&[
                "link: not-a-file",
                "via/f: not-a-file",
                "hard1: not-a-file",
                "tree/a?b: name",
                "tree/l: not-a-file",
            ])
        )
    );
}

/// Asserts that a pipe declared as an output, and one in a declared tree, are refused
/// `not-a-file` and never opened, and that a name in a tree that is not UTF-8 is refused `name`,
/// its byte shown as `?`. Linux only: macOS's file system holds no name that is not UTF-8, and
/// the pipe is made with `mknodat`, which rustix has for Linux. It catches a pipe that would
/// hold the agent waiting for a writer.
#[cfg(target_os = "linux")]
#[test]
fn pipes_and_names_that_are_not_utf8_are_refused() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    use rustix::fs::{CWD, FileType, Mode, mknodat};

    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-pipes");
    let root = dir.path();
    let fifo = |path: &Path| {
        let mode = Mode::RUSR | Mode::WUSR;
        mknodat(CWD, path, FileType::Fifo, mode, 0).unwrap();
    };
    fifo(&root.join("fifo"));
    put(root, "tree/ok", "kept");
    fifo(&root.join("tree/p"));
    let odd = root.join("tree").join(OsStr::from_bytes(b"x\xff"));
    fs::write(odd, "not utf-8").unwrap();
    let found = gather(root, &declared(&["fifo", "tree"]), ROOMY);
    assert_eq!(
        seen(found),
        (
            pairs(&[("tree/ok", "kept")]),
            Vec::new(),
            strings(&["fifo: not-a-file", "tree/p: not-a-file", "tree/x?: name"])
        )
    );
}

/// Asserts each cap: a file over the most one file may hold is `too-large`; past the most files
/// the next file is `over-limit` and the walk stops there (the file after it is not listed);
/// past the most bytes the next file is `over-limit` and a smaller one after it still fits
/// the bytes; a directory deeper than the tree may go is `over-limit` while the files above it
/// are gathered. It catches a cap that lets one file or one tree take the whole upload.
#[test]
fn each_cap_refuses_what_is_past_it() {
    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-caps");
    let root = dir.path();
    put(root, "big", "four");
    put(root, "small", "s");
    let one_file = Caps {
        file_bytes: 3,
        ..ROOMY
    };
    assert_eq!(
        seen(gather(root, &declared(&["big", "small"]), one_file)),
        (
            pairs(&[("small", "s")]),
            Vec::new(),
            strings(&["big: too-large"])
        )
    );
    put(root, "n/1", "a");
    put(root, "n/2", "b");
    put(root, "n/3", "c");
    put(root, "n/4", "d");
    let two_files = Caps { files: 2, ..ROOMY };
    assert_eq!(
        seen(gather(root, &declared(&["n"]), two_files)),
        (
            pairs(&[("n/1", "a"), ("n/2", "b")]),
            Vec::new(),
            strings(&["n/3: over-limit"])
        )
    );
    put(root, "b/1", "aa");
    put(root, "b/2", "bb");
    put(root, "b/3", "c");
    let three_bytes = Caps {
        total_bytes: 3,
        ..ROOMY
    };
    assert_eq!(
        seen(gather(root, &declared(&["b"]), three_bytes)),
        (
            pairs(&[("b/1", "aa"), ("b/3", "c")]),
            Vec::new(),
            strings(&["b/2: over-limit"])
        )
    );
    put(root, "t/top", "t");
    put(root, "t/a/deep", "x");
    let shallow = Caps { depth: 1, ..ROOMY };
    assert_eq!(
        seen(gather(root, &declared(&["t"]), shallow)),
        (
            pairs(&[("t/top", "t")]),
            Vec::new(),
            strings(&["t/a: over-limit"])
        )
    );
}

/// Asserts what is gathered of no usable declaration or directory: a declaration the store's
/// rules refuse is all failed `path`; a working directory that is not there makes every output
/// missing; one that is a file fails every output `read`. It catches a declaration that a
/// server should have refused taken in part.
#[test]
fn a_bad_declaration_or_directory_fails_every_output() {
    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-root");
    let root = dir.path();
    put(root, "ok", "fine");
    assert_eq!(
        seen(gather(root, &declared(&["ok", "../escape"]), ROOMY)),
        (
            Vec::new(),
            Vec::new(),
            strings(&["ok: path", "../escape: path"])
        )
    );
    let gone = root.join("gone");
    assert_eq!(
        seen(gather(&gone, &declared(&["a", "b/c"]), ROOMY)),
        (Vec::new(), strings(&["a", "b/c"]), Vec::new())
    );
    assert_eq!(
        seen(gather(&root.join("ok"), &declared(&["a"]), ROOMY)),
        (Vec::new(), Vec::new(), strings(&["a: read"]))
    );
}

/// Sets the mode of `path`.
fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Asserts that what the agent cannot open is failed `read` and the rest gathered: a directory
/// on the way that cannot be searched, a declared directory that cannot be read or listed (no
/// search permission), a file that cannot be read, and a file in a working directory that can
/// be opened but not searched. Run as a
/// user other than root, as the tests are (root reads through every mode). It catches a
/// failure to open taken for a missing output.
#[test]
fn what_cannot_be_opened_is_failed_read() {
    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-modes");
    let root = dir.path();
    put(root, "locked/f", "x");
    put(root, "closed/f", "x");
    put(root, "secret.txt", "x");
    put(root, "listed/f", "x");
    put(root, "fine", "kept");
    chmod(&root.join("locked"), 0o000);
    chmod(&root.join("closed"), 0o000);
    chmod(&root.join("secret.txt"), 0o000);
    chmod(&root.join("listed"), 0o600);
    let found = gather(
        root,
        &declared(&["locked/f", "closed", "secret.txt", "listed", "fine"]),
        ROOMY,
    );
    for name in ["locked", "closed", "listed"] {
        chmod(&root.join(name), 0o700);
    }
    chmod(&root.join("secret.txt"), 0o600);
    assert_eq!(
        seen(found),
        (
            pairs(&[("fine", "kept")]),
            Vec::new(),
            strings(&[
                "locked/f: read",
                "closed: read",
                "secret.txt: read",
                "listed: read",
            ])
        )
    );
    let unsearched = root.join("unsearched");
    put(&unsearched, "f", "x");
    chmod(&unsearched, 0o400);
    let found = gather(&unsearched, &declared(&["f"]), ROOMY);
    chmod(&unsearched, 0o700);
    assert_eq!(seen(found), (Vec::new(), Vec::new(), strings(&["f: read"])));
}

/// Asserts the bounds on what a gathering looks at: a directory with more names than entries
/// are left is refused `over-limit` whole; once the entries are spent, the next declared path is
/// `over-limit`; and the failures listed stop at their cap, the one past it `over-limit` and the
/// rest not listed. It catches a walk, or a manifest, a job could make as large as it likes.
#[test]
fn entries_and_failures_are_bounded() {
    let dir = TestDir::under(std::env::temp_dir(), "outputs-gather-bounds");
    let root = dir.path();
    put(root, "e/1", "a");
    put(root, "e/2", "b");
    put(root, "e/3", "c");
    let three = Caps {
        entries: 3,
        ..ROOMY
    };
    assert_eq!(
        seen(gather(root, &declared(&["e"]), three)),
        (Vec::new(), Vec::new(), strings(&["e: over-limit"]))
    );
    put(root, "w/a", "a");
    put(root, "w/b", "b");
    put(root, "x", "x");
    assert_eq!(
        seen(gather(root, &declared(&["w", "x"]), three)),
        (
            pairs(&[("w/a", "a"), ("w/b", "b")]),
            Vec::new(),
            strings(&["x: over-limit"])
        )
    );
    for name in ["a b", "c d", "e f", "g h"] {
        put(root, &format!("bad/{name}"), "x");
    }
    let two = Caps {
        failures: 2,
        ..ROOMY
    };
    assert_eq!(
        seen(gather(root, &declared(&["bad"]), two)),
        (
            Vec::new(),
            Vec::new(),
            strings(&["bad/a?b: name", "bad/c?d: name", "bad/e?f: over-limit"])
        )
    );
}
