//! The key file: owner-only, one token, trimmed, never printed.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use js_provider_lambdalabs::{ApiKey, KeyError};
use rustix::fs::{CWD, FileType, Mode, mknodat};

/// A directory of files for one test, removed when dropped.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("lambdalabs-key")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, bytes: &[u8], mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.join("key");
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load(name: &str, bytes: &[u8], mode: u32) -> Result<ApiKey, KeyError> {
    let dir = Dir::new(name);
    ApiKey::from_file(&dir.write(bytes, mode))
}

#[test]
fn an_owner_only_file_is_read_and_its_line_end_dropped() {
    for (name, mode) in [("m600", 0o600), ("m400", 0o400)] {
        assert!(load(name, b"abc123\n", mode).is_ok(), "{mode:o}");
    }
    for (name, bytes) in [
        ("crlf", &b"abc123\r\n"[..]),
        ("bare", b"abc123"),
        ("blank", b"abc123\n\n  \n"),
    ] {
        assert!(load(name, bytes, 0o600).is_ok(), "{name}");
    }
}

#[test]
fn a_file_others_can_read_or_write_is_refused() {
    for (name, mode) in [
        ("m644", 0o644),
        ("m640", 0o640),
        ("m604", 0o604),
        ("m660", 0o660),
        ("m602", 0o602),
        ("m777", 0o777),
        ("m610", 0o610),
    ] {
        let err = load(name, b"abc123\n", mode).unwrap_err();
        assert!(
            matches!(err, KeyError::Untrusted { mode: m, .. } if m == mode),
            "{mode:o}: {err:?}"
        );
    }
}

#[test]
fn the_refusal_names_the_file_and_leaves_the_key_out() {
    let err = load("named", b"TOPSECRETKEY\n", 0o644).unwrap_err();
    let shown = format!("{err} | {err:?}");
    assert!(
        shown.contains("named/key") && !shown.contains("TOPSECRET"),
        "{shown}"
    );
}

#[test]
fn a_file_that_is_not_a_single_token_is_refused_without_quoting_it() {
    let cases: [(&str, &[u8]); 5] = [
        ("inner-space", b"abc def\n"),
        ("two-lines", b"abc\ndef\n"),
        ("leading-space", b" abc\n"),
        ("control", b"abc\x07def\n"),
        ("non-ascii", "abc\u{e9}\n".as_bytes()),
    ];
    for (name, bytes) in cases {
        let err = load(name, bytes, 0o600).unwrap_err();
        assert!(matches!(err, KeyError::NotPlain { .. }), "{name}: {err:?}");
        assert!(!format!("{err} {err:?}").contains("abc"), "{name}");
    }
}

#[test]
fn an_empty_or_oversized_file_is_refused() {
    for (name, bytes) in [
        ("empty", &b""[..]),
        ("only-newline", b"\n"),
        ("spaces", b"   \n"),
    ] {
        let err = load(name, bytes, 0o600).unwrap_err();
        assert!(matches!(err, KeyError::Empty { .. }), "{name}: {err:?}");
    }
    let big = vec![b'a'; 4097];
    assert!(matches!(
        load("big", &big, 0o600).unwrap_err(),
        KeyError::TooLarge { .. }
    ));
    let edge = vec![b'a'; 4096];
    assert!(load("edge", &edge, 0o600).is_ok());
}

#[test]
fn a_missing_file_is_a_read_error_naming_it() {
    let dir = Dir::new("missing");
    let err = ApiKey::from_file(&dir.0.join("absent")).unwrap_err();
    assert!(
        matches!(&err, KeyError::Read { path, .. } if path.ends_with("absent")),
        "{err:?}"
    );
}

#[test]
fn a_directory_and_a_named_pipe_are_refused_without_waiting() {
    let dir = Dir::new("kinds");
    let err = ApiKey::from_file(&dir.0).unwrap_err();
    assert!(matches!(err, KeyError::Untrusted { .. }), "{err:?}");
    let pipe = dir.0.join("pipe");
    mknodat(CWD, &pipe, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
    let err = ApiKey::from_file(&pipe).unwrap_err();
    assert!(matches!(err, KeyError::Untrusted { .. }), "{err:?}");
}

#[test]
fn a_symbolic_link_to_an_owner_only_file_is_followed() {
    let dir = Dir::new("link");
    let target = dir.write(b"abc123\n", 0o600);
    let link = dir.0.join("link");
    symlink(&target, &link).unwrap();
    assert!(ApiKey::from_file(&link).is_ok());
}

#[test]
fn the_key_prints_as_redacted() {
    let key = load("debug", b"VERYSECRETVALUE\n", 0o600).unwrap();
    assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
    assert_eq!(format!("{:?}", key.clone()), "ApiKey(<redacted>)");
}
