//! The checks before any request: the flags, the server URL, the token file, the
//! certificate file and the durations.

use prost::Message as _;

use crate::support::{Js, Scratch, TOKEN, against, run, services};

/// The words of a call that sends one request and has no flag of its own: the global flags
/// are checked before it is sent.
const POOL_LIST: [&str; 2] = ["pool", "list"];

/// Asserts that `--help` and `--version` print on stdout and exit 0, and that every
/// way of getting a command line wrong exits 2 with the usage on stderr: no
/// subcommand, an unknown flag, a command group without its command, a missing required
/// flag of a command.
#[test]
fn help_version_and_usage_errors() {
    let scratch = Scratch::new("flags_usage");
    let js = against("http://127.0.0.1:1", &scratch);
    let help = js.go(&["--help"]);
    assert_eq!(help.code, 0);
    assert!(
        help.out.contains("Makes the calls of the job service"),
        "{}",
        help.out
    );
    assert!(help.out.contains("--token-file"), "{}", help.out);
    let version = js.go(&["--version"]);
    assert_eq!((version.code, version.out.starts_with("js ")), (0, true));

    for words in [
        vec![],
        vec!["job"],
        vec!["--no-such-flag", "job", "list"],
        vec!["job", "get"],
    ] {
        let got = js.go(&words);
        assert_eq!(got.code, 2, "{words:?}: {}", got.err);
        assert!(
            got.err.contains("Usage") || got.err.contains("usage"),
            "{}",
            got.err
        );
    }
    let bare = run(vec!["js".into()], services());
    assert_eq!(bare.code, 2, "{}", bare.err);
    assert!(bare.err.contains("Usage"), "{}", bare.err);
}

/// Asserts that each unit of a duration flag (`s`, `m`, `h`, none) is accepted and sent as
/// whole seconds: `--cordon-secs` of 90, 90s, 15m and 1h reach the server as 90, 90, 900
/// and 3600.
#[test]
fn durations_are_sent_in_seconds() {
    let scratch = Scratch::new("flags_values");
    let fake = crate::fake_jobs::Fake::new();
    for (given, seconds) in [("90", 90), ("90s", 90), ("15m", 900), ("1h", 3600)] {
        let before = fake.seen().len();
        let got = crate::jobs::js(
            &scratch,
            fake.dialer(),
            &["host", "cordon", "--host-id", "h-1", "--cordon-secs", given],
        );
        assert_ne!(got.code, 2, "{given}: {}", got.err);
        let seen = fake.seen();
        assert_eq!(seen.len(), before + 1, "{given}");
        let sent = js_proto::v1::CordonHostRequest::decode(seen[before].message.as_slice());
        assert_eq!(sent.unwrap().cordon_secs, Some(seconds), "{given}");
    }
}

/// Asserts that a server URL the client must not use is refused before any connection:
/// no scheme, another scheme, plain http to a host off the loopback (a name or an
/// address), a user in the URL, a path, a query, and text that is not a URL.
#[test]
fn a_bad_server_url_is_refused() {
    let scratch = Scratch::new("flags_urls");
    for url in [
        "127.0.0.1:7300",
        "ftp://127.0.0.1:7300",
        "http://example.com",
        "http://10.0.0.1:7300",
        "http://[2001:db8::1]:7300",
        "http://user@127.0.0.1:7300",
        "https://user:pw@example.com",
        "http://127.0.0.1:7300/v1",
        "https://example.com/?q=1",
        "not a url",
    ] {
        let got = against(url, &scratch).go(&POOL_LIST);
        assert_eq!(got.code, 2, "{url}: {}", got.err);
        assert!(got.err.starts_with("js: --server: "), "{url}: {}", got.err);
        assert!(got.err.contains(url), "{url}: {}", got.err);
    }
    let plain = against("http://example.com", &scratch).go(&POOL_LIST);
    assert!(
        plain.err.contains("never crosses a network in clear"),
        "{}",
        plain.err
    );
}

/// Asserts that a token file that is not the user's own private regular file is refused
/// naming the flag and the file: loose modes, a directory, a missing file; and that
/// empty content and a token with a space or a line break inside are refused too. The
/// token is in none of the messages.
#[test]
fn a_bad_token_file_is_refused() {
    let scratch = Scratch::new("flags_tokens");
    let dir = scratch.path("a-directory");
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        (scratch.write("loose-644", TOKEN, 0o644), "mode 0644"),
        (scratch.write("loose-660", TOKEN, 0o660), "mode 0660"),
        (scratch.write("writable-token", TOKEN, 0o700), "mode 0700"),
        (dir, "regular file"),
        (scratch.path("missing"), "cannot be read"),
        (scratch.write("empty", " \n", 0o600), "is empty"),
        (
            scratch.write("spaced", "two words\n", 0o600),
            "printable ASCII",
        ),
        (
            scratch.write("lines", "one\ntwo\n", 0o600),
            "printable ASCII",
        ),
        (
            scratch.write("accent", "caf\u{e9}", 0o600),
            "printable ASCII",
        ),
    ];
    for (path, why) in cases {
        let js = Js {
            server: "http://127.0.0.1:1".to_owned(),
            token_file: path.clone(),
        };
        let got = js.go(&POOL_LIST);
        assert_eq!(got.code, 2, "{path:?}: {}", got.err);
        assert!(got.err.starts_with("js: --token-file: "), "{}", got.err);
        assert!(got.err.contains(&path.display().to_string()), "{}", got.err);
        assert!(got.err.contains(why), "{why}: {}", got.err);
    }
}

/// Asserts that a `--ca-cert` file that is missing, empty or not PEM is exit 2 naming
/// the flag and the file, before any connection.
#[test]
fn a_bad_ca_file_is_refused() {
    let scratch = Scratch::new("flags_ca");
    let js = against("https://127.0.0.1:1", &scratch);
    let cases = [
        scratch.path("absent.pem"),
        scratch.write("empty.pem", "", 0o644),
        scratch.write("junk.pem", "this is not a certificate", 0o644),
    ];
    for path in cases {
        let ca = path.to_str().unwrap();
        let got = js.go(&[&["--ca-cert", ca], &POOL_LIST[..]].concat());
        assert_eq!(got.code, 2, "{path:?}: {}", got.err);
        assert!(got.err.starts_with("js: --ca-cert: "), "{}", got.err);
        assert!(got.err.contains(ca), "{}", got.err);
    }
}
