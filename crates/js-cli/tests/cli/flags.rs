//! The checks before any request: the flags, the server URL, the token file and the
//! certificate file.

use std::os::unix::fs::symlink;

use crate::support::{Harness, Js, REVOKE, Scratch, TOKEN, against, hangup_port, run, services};

/// Asserts that `--help` and `--version` print on stdout and exit 0, and that every
/// way of getting a command line wrong exits 2 with the usage on stderr: no
/// subcommand, an unknown flag, a missing required flag of a join-token command.
#[test]
fn help_version_and_usage_errors() {
    let h = Harness::new("flags_usage");
    let help = h.js.go(&["--help"]);
    assert_eq!(help.code, 0);
    assert!(help.out.contains("mints the join tokens"), "{}", help.out);
    assert!(help.out.contains("--token-file"), "{}", help.out);
    let version = h.js.go(&["--version"]);
    assert_eq!((version.code, version.out.starts_with("js ")), (0, true));

    for words in [
        vec![],
        vec!["join-token"],
        vec!["--no-such-flag", "join-token", "revoke"],
        vec!["join-token", "mint", "--provider", "p"],
    ] {
        let got = h.js.go(&words);
        assert_eq!(got.code, 2, "{words:?}: {}", got.err);
        assert!(
            got.err.contains("Usage") || got.err.contains("usage"),
            "{}",
            got.err
        );
    }
}

/// Asserts that a missing `--server` or `--token-file` is exit 2 naming the flag.
#[test]
fn the_server_and_the_token_file_are_required() {
    let h = Harness::new("flags_required");
    let token = h.js.token_file.to_str().unwrap();
    let no_server = run(
        ["js", "--token-file", token]
            .into_iter()
            .chain(REVOKE)
            .map(Into::into)
            .collect(),
        services(),
    );
    assert_eq!(
        (no_server.code, no_server.err.as_str()),
        (2, "js: --server: is required\n")
    );
    let no_token = run(
        ["js", "--server", &h.js.server]
            .into_iter()
            .chain(REVOKE)
            .map(Into::into)
            .collect(),
        services(),
    );
    assert_eq!(
        (no_token.code, no_token.err.as_str()),
        (2, "js: --token-file: is required\n")
    );
}

/// Asserts that a `--ttl` that is not a positive duration (zero, text, an unknown unit,
/// an overflow) is a usage error, and that each unit (`s`, `m`, `h`, none) is accepted.
#[test]
fn durations_are_checked() {
    let h = Harness::new("flags_values");
    for ttl in ["0", "soon", "5x", "99999999999999999999h"] {
        let got = h.js.go(&mint_words(ttl));
        assert_eq!(got.code, 2, "{ttl}: {}", got.err);
    }
    for ttl in ["90", "90s", "15m", "1h"] {
        let got = h.js.go(&mint_words(ttl));
        assert_eq!(got.code, 0, "{ttl}: {}", got.err);
    }
}

/// The words of a mint with this `--ttl`.
fn mint_words(ttl: &str) -> Vec<&str> {
    let mut words = vec!["join-token", "mint"];
    words.extend(&REVOKE[2..]);
    words.extend(["--ttl", ttl]);
    words
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
        let got = against(url, &scratch).go(&REVOKE);
        assert_eq!(got.code, 2, "{url}: {}", got.err);
        assert!(got.err.starts_with("js: --server: "), "{url}: {}", got.err);
        assert!(got.err.contains(url), "{url}: {}", got.err);
    }
    let plain = against("http://example.com", &scratch).go(&REVOKE);
    assert!(
        plain.err.contains("never crosses a network in clear"),
        "{}",
        plain.err
    );
}

/// Asserts that http to the machine itself is accepted by name and by address, v4 and
/// v6 (the request is then attempted and fails: the server hangs up), and that https
/// needs no loopback. Catches a loopback http address refused by the flag check, and,
/// by holding the port for the whole test, a port another process took between choosing
/// it and dialling it, which would have answered instead of failing.
#[test]
fn loopback_hosts_may_use_plain_http() {
    let scratch = Scratch::new("flags_loopback");
    let closed = hangup_port();
    for url in [
        format!("http://127.0.0.1:{closed}"),
        format!("http://127.9.9.9:{closed}"),
        format!("http://[::1]:{closed}"),
        format!("http://LocalHost:{closed}"),
    ] {
        let got = against(&url, &scratch).go(&REVOKE);
        assert_eq!(got.code, 1, "{url}: {}", got.err);
        assert!(
            got.err.starts_with("js: cannot talk to http://"),
            "{url}: {}",
            got.err
        );
    }
}

/// Asserts that a token file that is not the user's own private regular file is refused
/// naming the flag and the file: loose modes, a directory, a missing file; and that
/// empty content and a token with a space or a line break inside are refused too. The
/// token is in none of the messages.
#[test]
fn a_bad_token_file_is_refused() {
    let h = Harness::new("flags_tokens");
    let scratch = &h.scratch;
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
            server: h.js.server.clone(),
            token_file: path.clone(),
        };
        let got = js.go(&REVOKE);
        assert_eq!(got.code, 2, "{path:?}: {}", got.err);
        assert!(got.err.starts_with("js: --token-file: "), "{}", got.err);
        assert!(got.err.contains(&path.display().to_string()), "{}", got.err);
        assert!(got.err.contains(why), "{why}: {}", got.err);
    }
}

/// Asserts that a token file may be a symbolic link to a good file (secret mounts are
/// links), with its trailing newline dropped, and that a read-only 0400 file is fine.
#[test]
fn a_linked_or_read_only_token_file_works() {
    let h = Harness::new("flags_token_ok");
    let target = h
        .scratch
        .write("token-target", &format!("{TOKEN}\n"), 0o400);
    let link = h.scratch.path("token-link");
    symlink(&target, &link).unwrap();
    let js = Js {
        server: h.js.server.clone(),
        token_file: link,
    };
    assert_eq!(js.go(&REVOKE).code, 0);
}

/// Asserts that a token the server does not know is exit 1 (the server refused it with
/// 401), and that the message is the same for any wrong token and holds none.
#[test]
fn a_wrong_token_is_a_server_failure() {
    let h = Harness::new("flags_wrong_token");
    let other = h.scratch.write("other", "another-token", 0o600);
    let js = Js {
        server: h.js.server.clone(),
        token_file: other,
    };
    let got = js.go(&REVOKE);
    assert_eq!(
        (got.code, got.err.as_str()),
        (1, "js: the server refused the token (401)\n")
    );
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
        let got = js.go(&[&["--ca-cert", ca], &REVOKE[..]].concat());
        assert_eq!(got.code, 2, "{path:?}: {}", got.err);
        assert!(got.err.starts_with("js: --ca-cert: "), "{}", got.err);
        assert!(got.err.contains(ca), "{}", got.err);
    }
}

/// Asserts that the four global flags may stand after the subcommand as well as before it:
/// a revoke with `--server` and `--token-file` last is answered as one with them first.
#[test]
fn the_global_flags_may_follow_the_subcommand() {
    let h = Harness::new("flags_last");
    let first = h.js.go(&REVOKE);
    let last = h.js.go_last(&REVOKE);
    assert_eq!(first.code, 0, "{}", first.err);
    assert_eq!((last.code, last.out), (0, first.out));
}
