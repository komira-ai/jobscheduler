//! The OAuth client file: read from a path the configuration names, under the posture of
//! every secret file of the service (a regular file, owner-only, owned by the user
//! running it), and never quoted back in an error.

use js_lease_server::network::{NetworkError, OAUTH_FILE_LIMIT, OAuthClient};

use crate::fake::CLIENT_SECRET;
use crate::support::Scratch;

fn refusal(result: Result<OAuthClient, NetworkError>) -> String {
    match result {
        Err(NetworkError::ClientFile(why)) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Asserts that a 0600 file and a 0400 file with both fields are read, and that the
/// client prints its id and `***`.
#[test]
fn an_owner_only_file_is_read() {
    let scratch = Scratch::new("oauth-ok");
    let text = format!("client_id = \"cid-7\"\nclient_secret = \"{CLIENT_SECRET}\"\n");
    for mode in [0o600, 0o400] {
        let path = scratch.write(&format!("client-{mode:o}"), text.as_bytes(), mode);
        let client = OAuthClient::from_file(&path).unwrap();
        let shown = format!("{client:?}");
        assert!(shown.contains("cid-7") && shown.contains("***"), "{shown}");
        assert!(!shown.contains(CLIENT_SECRET), "{shown}");
    }
}

/// Asserts that a file others can read or write, one in a mode with no owner bits, a
/// missing file, and a directory are refused, and that the message names the path and
/// holds nothing the file says.
#[test]
fn a_file_others_can_use_is_refused() {
    let scratch = Scratch::new("oauth-mode");
    let text = format!("client_id = \"cid\"\nclient_secret = \"{CLIENT_SECRET}\"\n");
    for mode in [0o644, 0o640, 0o604, 0o660, 0o700] {
        let path = scratch.write(&format!("client-{mode:o}"), text.as_bytes(), mode);
        let why = refusal(OAuthClient::from_file(&path));
        assert!(
            why.contains("readable only by that user"),
            "{mode:o}: {why}"
        );
        assert!(why.contains(path.to_str().unwrap()), "{why}");
        assert!(!why.contains(CLIENT_SECRET), "{why}");
    }
    let missing = scratch
        .write("present", b"", 0o600)
        .with_file_name("absent");
    assert!(refusal(OAuthClient::from_file(&missing)).contains("absent"));
    let dir = missing.parent().unwrap();
    assert!(refusal(OAuthClient::from_file(dir)).contains("regular file"));
}

/// Asserts that the size limit holds to the byte: a file of exactly the limit is read and
/// one byte more is refused.
#[test]
fn the_file_may_hold_at_most_the_limit() {
    let scratch = Scratch::new("oauth-size");
    let base = format!("client_id = \"cid\"\nclient_secret = \"{CLIENT_SECRET}\"\n# ");
    let fill = |total: usize| {
        let mut text = base.clone();
        text.push_str(&"x".repeat(total - base.len()));
        text
    };
    let at = scratch.write("at", fill(OAUTH_FILE_LIMIT).as_bytes(), 0o600);
    OAuthClient::from_file(&at).unwrap();
    let over = scratch.write("over", fill(OAUTH_FILE_LIMIT + 1).as_bytes(), 0o600);
    assert!(refusal(OAuthClient::from_file(&over)).contains("longer than the limit"));
}

/// Asserts that content that is not a client is refused without quoting it: invalid
/// UTF-8, TOML that does not parse (with the secret on the failing line), an unknown
/// field, a missing field, and an empty id or secret.
#[test]
fn content_that_is_not_a_client_is_refused_without_quoting_it() {
    let scratch = Scratch::new("oauth-content");
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("utf8", vec![0xff, 0xfe], "not UTF-8"),
        (
            "syntax",
            format!("client_id = \"cid\"\nclient_secret = {CLIENT_SECRET}\n").into_bytes(),
            "",
        ),
        (
            "unknown",
            format!("client_id = \"c\"\nclient_secret = \"{CLIENT_SECRET}\"\nextra = 1\n")
                .into_bytes(),
            "extra",
        ),
        (
            "missing",
            format!("client_secret = \"{CLIENT_SECRET}\"\n").into_bytes(),
            "client_id",
        ),
        (
            "empty-id",
            format!("client_id = \"\"\nclient_secret = \"{CLIENT_SECRET}\"\n").into_bytes(),
            "must not be empty",
        ),
        (
            "empty-secret",
            b"client_id = \"cid\"\nclient_secret = \"\"\n".to_vec(),
            "must not be empty",
        ),
    ];
    for (name, bytes, expect) in cases {
        let path = scratch.write(name, &bytes, 0o600);
        let why = refusal(OAuthClient::from_file(&path));
        assert!(why.contains(expect), "{name}: {why}");
        assert!(!why.contains(CLIENT_SECRET), "{name}: {why}");
    }
}
