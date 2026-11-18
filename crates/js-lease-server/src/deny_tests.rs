//! The deny list: what its file may hold, what a refusal says, and what a reload keeps.

use std::path::PathBuf;

use js_testkit::dir::TestDir;

use super::*;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A scratch directory and the path of a file in it holding `text`.
fn file_with(text: &[u8]) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "deny_unit");
    let path = dir.path().join("deny");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

/// Asserts that the file's host ids are denied and nothing else is: a blank line, a comment,
/// and spaces around an id are skipped or trimmed, a repeated id counts once, and a text that
/// is no host id is denied by no one.
#[test]
fn the_hosts_of_the_file_are_denied_and_no_other() {
    let (_dir, path) = file_with(b"# farm-3 was retired\n\nfarm-1\n  farm-2.lab  \nfarm-1\n");
    let list = DenyList::open(path).unwrap();
    assert!(list.denies(&host("farm-1")));
    assert!(list.denies(&host("farm-2.lab")));
    assert!(!list.denies(&host("farm-3")));
    assert!(list.denies_text("farm-1"));
    assert!(!list.denies_text("farm-3"));
    assert!(!list.denies_text("Not A Host"));
}

/// Asserts that a list with no file denies no host and that reloading it is a no-op.
#[test]
fn a_list_without_a_file_denies_no_host() {
    let list = DenyList::none();
    assert!(!list.denies(&host("farm-1")));
    assert!(list.reload().is_ok());
    assert!(!list.denies(&host("farm-1")));
}

/// Asserts that a reload takes what the file names now, adding a host and readmitting one
/// whose line is gone.
#[test]
fn a_reload_follows_the_file() {
    let (_dir, path) = file_with(b"farm-1\n");
    let list = DenyList::open(path.clone()).unwrap();
    std::fs::write(&path, b"farm-2\n").unwrap();
    list.reload().unwrap();
    assert!(!list.denies(&host("farm-1")));
    assert!(list.denies(&host("farm-2")));
}

/// Asserts that a reload that fails (the file gone, or a bad line) keeps the hosts denied
/// before it, and says why.
#[test]
fn a_reload_that_fails_keeps_the_hosts_denied() {
    let (_dir, path) = file_with(b"farm-1\n");
    let list = DenyList::open(path.clone()).unwrap();
    std::fs::write(&path, b"farm-2\n!\n").unwrap();
    let bad = list.reload().unwrap_err();
    assert_eq!(
        bad.to_string(),
        format!(
            "--agent-deny-file: {}: line 2: not a host id",
            path.display()
        )
    );
    assert!(list.denies(&host("farm-1")));
    assert!(!list.denies(&host("farm-2")));
    std::fs::remove_file(&path).unwrap();
    assert_eq!(list.reload().unwrap_err().flag(), "--agent-deny-file");
    assert!(list.denies(&host("farm-1")));
}

/// Asserts that a start is refused naming the flag, the file and the line number alone: the
/// refusal never carries the text of the bad line.
#[test]
fn a_bad_line_is_refused_by_its_number_and_never_its_text() {
    let (_dir, path) = file_with(b"farm-1\n# fine\nsecret-looking-TEXT value\n");
    let err = DenyList::open(path.clone()).err().unwrap();
    let said = format!(
        "--agent-deny-file: {}: line 3: not a host id",
        path.display()
    );
    assert_eq!(err.to_string(), said);
}

/// Asserts that a file that is absent, is not text, or is over the size bound is refused
/// naming the flag.
#[test]
fn a_missing_binary_or_oversize_file_is_refused() {
    let (dir, path) = file_with(b"farm-1\n");
    let missing = dir.path().join("absent");
    let err = DenyList::open(missing).err().unwrap();
    assert_eq!(err.flag(), "--agent-deny-file");
    std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    let err = DenyList::open(path.clone()).err().unwrap();
    assert_eq!(
        err.to_string(),
        format!("--agent-deny-file: {}: is not text", path.display())
    );
    let big = vec![b'\n'; usize::try_from(MAX_BYTES).unwrap() + 1];
    std::fs::write(&path, big).unwrap();
    let err = DenyList::open(path.clone()).err().unwrap();
    assert_eq!(
        err.to_string(),
        format!(
            "--agent-deny-file: {}: is larger than {MAX_BYTES} bytes",
            path.display()
        )
    );
}
