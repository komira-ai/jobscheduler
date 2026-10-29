//! The file of hosts the registry refused to remove: its format, how it is written, and
//! what a reader refuses.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_lease_server::UnremovedFile;
use js_testkit::dir::TestDir;

fn dir(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease-server"),
        name,
    )
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn hosts(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Asserts the bytes of version 1: the text of the file is exactly the golden, with the
/// hosts in order whatever order they were given, and the golden reads back as the same
/// hosts. It catches a change of the format that a file written by an earlier release
/// would not survive (a renamed field, another order, a new version number).
#[test]
fn version_one_is_the_golden_text() {
    let dir = dir("unremoved_golden");
    let path = dir.path().join("state.scaler.unremoved");
    let file = UnremovedFile::new(path.clone());
    let listed = hosts(&["js-burst-0002", "js-burst-0001"]);
    file.save(&listed).unwrap();
    let golden = include_str!("golden/unremoved_v1.json");
    assert_eq!(fs::read_to_string(&path).unwrap(), golden);

    fs::remove_file(&path).unwrap();
    fs::write(&path, golden).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(file.load().unwrap(), listed);
}

/// Asserts the posture of the file: it is written in a file only its owner can use with
/// no temporary file left beside it, a second save replaces it whole (an empty set
/// included), and a half-made file a crash left under the temporary name, with a mode
/// anyone can read, is replaced by a new owner-only one and never read in place of the
/// file. It catches a file others can read, a write in place, and a reader that takes the
/// temporary file.
#[test]
fn the_hosts_are_written_whole_owner_only_and_read_back() {
    let dir = dir("unremoved_posture");
    let path = dir.path().join("state.scaler.unremoved");
    let file = UnremovedFile::new(path.clone());
    let stale = dir.path().join("state.scaler.unremoved.tmp");
    fs::write(&stale, "{ half of a file").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();

    file.save(&hosts(&["js-a"])).unwrap();
    assert_eq!(mode(&path), 0o600);
    let beside: Vec<PathBuf> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(beside, std::slice::from_ref(&path));
    assert_eq!(file.load().unwrap(), hosts(&["js-a"]));

    file.save(&BTreeSet::new()).unwrap();
    assert_eq!(file.load().unwrap(), BTreeSet::new());
    assert_eq!(mode(&path), 0o600);
}

/// Asserts that no file is an empty list, and that a save into a directory that is not
/// there is an error. It catches a missing file refused at start (a pool that never had a
/// refusal would not start) and a save that cannot fail.
#[test]
fn a_missing_file_is_an_empty_list_and_a_save_that_cannot_be_made_is_an_error() {
    let dir = dir("unremoved_missing");
    let file = UnremovedFile::new(dir.path().join("state.scaler.unremoved"));
    assert_eq!(file.load().unwrap(), BTreeSet::new());
    let absent = UnremovedFile::new(dir.path().join("absent").join("state.scaler.unremoved"));
    assert!(absent.save(&hosts(&["js-a"])).is_err());
}

/// Asserts what the reader refuses, each as a start-up error that names the file: a file
/// others can read, one that is not text, one that is not JSON, one of another version or
/// of none, one whose hosts are not a list of text, and a directory where the file should
/// be. It catches a damaged file read as an empty list, which would drop the retries
/// without a word.
#[test]
fn a_file_that_cannot_be_read_stops_the_start() {
    let dir = dir("unremoved_refused");
    let path = dir.path().join("state.scaler.unremoved");
    let file = UnremovedFile::new(path.clone());
    let shown = path.display().to_string();

    fs::write(&path, "{\"version\":1,\"hosts\":[\"js-a\"]}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let refused = file.load().unwrap_err().to_string();
    assert!(
        refused.contains(&shown) && refused.contains("0600"),
        "{refused}"
    );

    let cases: Vec<(Vec<u8>, &str)> = vec![
        (vec![0xff, 0xfe, 0x00], "it is not text"),
        (b"not json".to_vec(), "expected"),
        (b"{\"version\":2,\"hosts\":[]}".to_vec(), "version Some(2)"),
        (b"{\"hosts\":[]}".to_vec(), "version None"),
        (
            b"{\"version\":1,\"hosts\":\"js-a\"}".to_vec(),
            "invalid type",
        ),
        (b"{\"version\":1}".to_vec(), "missing field"),
    ];
    for (bytes, why) in cases {
        fs::remove_file(&path).unwrap();
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let refused = file.load().unwrap_err().to_string();
        assert!(refused.contains(&shown), "{refused}");
        assert!(refused.contains("unremoved-hosts file"), "{refused}");
        assert!(refused.contains(why), "{refused}");
    }

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(file.load().is_err());
}
