//! The scaler's records file: written whole, owner-only, renamed into place, and read back;
//! and what a reader refuses.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_lease_server::RecordsFile;
use js_scaler::{Phase, Records};
use js_testkit::dir::TestDir;

use crate::fakes::{record, records};

fn dir(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease-server"),
        name,
    )
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sample() -> Records {
    records(vec![
        record("js-one", Phase::Booting),
        record("js-two", Phase::Releasing),
    ])
}

/// Asserts the posture of the file: what is written is the whole text of the records, in a
/// file only its owner can use, with no temporary file left beside it; reading it back
/// gives the same records; a second save replaces it whole; and a file left half-made by a
/// crash, with a mode anyone can read, is replaced by a new one that is owner-only and
/// never read in place of the file. It catches a file that others can read (it names the
/// machines the scaler pays for), a write in place (a crash would leave half of it), and a
/// reader that takes the temporary file.
#[test]
fn the_records_are_written_whole_owner_only_and_read_back() {
    let dir = dir("records_posture");
    let path = dir.path().join("state.scaler");
    let file = RecordsFile::new(path.clone());
    assert_eq!(file.load().unwrap(), Records::default());

    file.save(&sample()).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        sample().encode().unwrap()
    );
    assert_eq!(mode(&path), 0o600);
    let beside: Vec<PathBuf> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(beside, std::slice::from_ref(&path));
    assert_eq!(file.load().unwrap(), sample());

    let stale = dir.path().join("state.scaler.tmp");
    fs::write(&stale, "{ half of a file").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(file.load().unwrap(), sample());

    let changed = records(vec![record("js-one", Phase::Gone)]);
    file.save(&changed).unwrap();
    assert_eq!(file.load().unwrap(), changed);
    assert_eq!(mode(&path), 0o600);
    assert!(!stale.exists(), "the temporary file was renamed into place");
}

/// Asserts that a save into a directory that is not there fails and says so, so the shell
/// does not run the call the records describe.
#[test]
fn a_save_that_cannot_be_made_is_an_error() {
    let dir = dir("records_missing_dir");
    let file = RecordsFile::new(dir.path().join("absent").join("state.scaler"));
    assert!(file.save(&sample()).is_err());
}

/// Asserts what the reader refuses, each as a start-up error that names the file: a file
/// others can read, one that is not text, one that is not records, one of another version,
/// and a directory where the file should be. It catches a start with no records after a
/// file that cannot be read, which would launch again for machines that already exist.
#[test]
fn a_file_that_cannot_be_read_as_records_stops_the_start() {
    let dir = dir("records_refused");
    let path = dir.path().join("state.scaler");
    let file = RecordsFile::new(path.clone());
    let shown = path.display().to_string();

    fs::write(&path, sample().encode().unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let refused = file.load().unwrap_err().to_string();
    assert!(
        refused.contains(&shown) && refused.contains("0600"),
        "{refused}"
    );

    let versioned = b"{\"version\": 99, \"instances\": [], \"cooldowns\": {}}".to_vec();
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (vec![0xff, 0xfe, 0x00], "it is not text"),
        (b"not json".to_vec(), "records:"),
        (versioned, "version"),
    ];
    for (bytes, why) in cases {
        fs::remove_file(&path).unwrap();
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let refused = file.load().unwrap_err().to_string();
        assert!(refused.contains(&shown), "{refused}");
        assert!(refused.contains("scaler records file"), "{refused}");
        assert!(refused.contains(why), "{refused}");
    }

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(file.load().is_err());
}
