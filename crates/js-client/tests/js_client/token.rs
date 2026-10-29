//! The token file: owner-only, one printable token.

use js_client::{bearer, bearer_of};

use crate::support::Scratch;

/// Asserts that a token file of mode 0600 or 0400 gives the header value, with the
/// trailing line break dropped.
#[test]
fn an_owner_only_file_gives_the_bearer_value() {
    let dir = Scratch::new("token_ok");
    for (name, mode) in [("rw", 0o600), ("r", 0o400)] {
        let path = dir.write(name, b"abc-123_XYZ\n", mode);
        assert_eq!(
            bearer(&path).unwrap(),
            "Bearer abc-123_XYZ",
            "mode {mode:o}"
        );
    }
}

/// Asserts that a file other users can read, write or execute is refused with its mode
/// in the message, and that so are a missing file and a directory.
#[test]
fn a_file_others_can_use_or_one_that_is_not_a_file_is_refused() {
    let dir = Scratch::new("token_modes");
    for (name, mode) in [
        ("group", 0o640),
        ("world", 0o604),
        ("exec", 0o700),
        ("rw", 0o660),
    ] {
        let path = dir.write(name, b"abc", mode);
        let said = bearer(&path).unwrap_err().to_string();
        let want = format!(
            "--token-file: {} must be a regular file owned by you and readable only by you (mode 0600 or 0400), not mode {mode:04o} owned by uid ",
            path.display()
        );
        assert!(said.starts_with(&want), "{said}");
    }

    let missing = dir.path("missing");
    let said = bearer(&missing).unwrap_err().to_string();
    let want = format!("--token-file: {} cannot be read: ", missing.display());
    assert!(said.starts_with(&want), "{said}");

    let folder = dir.path("folder");
    std::fs::create_dir(&folder).unwrap();
    let said = bearer(&folder).unwrap_err().to_string();
    assert!(
        said.contains("must be a regular file owned by you"),
        "{said}"
    );
}

/// Asserts that an empty file, a token with a space or a line break inside it, and bytes
/// that are not ASCII are each refused with a reason that names the file.
#[test]
fn a_file_without_one_printable_token_is_refused() {
    let dir = Scratch::new("token_content");
    let empty = dir.write("empty", b" \n", 0o600);
    assert_eq!(
        bearer(&empty).unwrap_err().to_string(),
        format!("--token-file: {} is empty", empty.display())
    );
    for (name, bytes) in [
        ("space", &b"two words"[..]),
        ("lines", &b"one\ntwo"[..]),
        ("binary", &[0xff, 0xfe][..]),
    ] {
        let path = dir.write(name, bytes, 0o600);
        assert_eq!(
            bearer(&path).unwrap_err().to_string(),
            format!(
                "--token-file: {} must hold one token of printable ASCII with no space or line break inside it",
                path.display()
            ),
            "{name}"
        );
    }
}

/// Asserts that a secret held in memory gives the header value, and that an empty secret
/// and ones with a space, a line break or a byte above 127 are refused with a message that
/// never repeats them.
#[test]
fn a_secret_in_memory_gives_the_bearer_value() {
    assert_eq!(bearer_of("abc-123_XYZ").unwrap(), "Bearer abc-123_XYZ");
    for odd in ["", "a b", "a\nb", "caf\u{e9}"] {
        let said = bearer_of(odd).unwrap_err().to_string();
        assert!(said.starts_with("--token-file: the credential "), "{said}");
        assert!(odd.is_empty() || !said.contains(odd), "{said}");
    }
}
