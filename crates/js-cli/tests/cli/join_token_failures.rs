//! `lease join-token mint` when things go wrong: answers that are cut or odd, a standard
//! output that fails half way, destinations that must be judged before any request, and
//! the failure format with `--json`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use crate::join_token::{EXPIRES, MINTED, created, facts, lost, recording, recording_with, words};
use crate::support::{Never, Scratch, against, http, services};

/// The sorted names in `dir`.
fn listing(dir: &Path) -> Vec<OsString> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    names
}

/// A directory only its owner can write, whatever the umask.
fn mkdir(path: &Path, mode: u32) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// What a mint says when its answer was lost or unusable, after the cause `cause`.
fn unconfirmed(cause: &str) -> String {
    format!(
        "js: {cause}; a join token may have been minted and lost: nothing was written, and \
         an unused token expires on the server by itself\n"
    )
}

/// Asserts that a 201 whose body is not a usable answer (not JSON, empty, an array, a string,
/// a missing, short, upper-case or non-string token, an expiry that is a string, negative, a
/// float or missing) is a failure (exit 1) with exactly the redacted text naming the field and
/// saying the token may have been minted, one request, nothing on standard output and no
/// file, to a file and to standard output alike. It catches a file written with whatever the
/// server said, a odd token echoed, and a message that implies nothing was minted.
#[test]
fn a_201_without_a_usable_answer_is_a_failure_that_says_a_token_may_exist() {
    let upper = MINTED.to_uppercase();
    let cases: Vec<(String, &str)> = vec![
        ("{\"token\": ".to_owned(), "token"),
        (String::new(), "token"),
        ("[1, 2]".to_owned(), "token"),
        ("\"just a string\"".to_owned(), "token"),
        (json!({ "expires_at_ms": EXPIRES }).to_string(), "token"),
        (
            json!({ "token": "short", "expires_at_ms": EXPIRES }).to_string(),
            "token",
        ),
        (
            json!({ "token": upper, "expires_at_ms": EXPIRES }).to_string(),
            "token",
        ),
        (
            json!({ "token": 7, "expires_at_ms": EXPIRES }).to_string(),
            "token",
        ),
        (json!({ "token": MINTED }).to_string(), "expires_at_ms"),
        (
            json!({ "token": MINTED, "expires_at_ms": "1800000000000" }).to_string(),
            "expires_at_ms",
        ),
        (
            json!({ "token": MINTED, "expires_at_ms": -5 }).to_string(),
            "expires_at_ms",
        ),
        (
            json!({ "token": MINTED, "expires_at_ms": 1.5 }).to_string(),
            "expires_at_ms",
        ),
        (
            format!(r#"{{"token":"{MINTED}","expires_at_ms":1800000000000.0}}"#),
            "expires_at_ms",
        ),
    ];
    for (body, field) in cases {
        let scratch = Scratch::new("mint_unusable");
        let (server, seen) = recording(http("201 Created", &body));
        let js = against(&server, &scratch);
        let out = scratch.path("minted");
        let want = unconfirmed(&format!("the server's answer has no usable {field:?}"));
        let got = js.go(&words(&["--out", out.to_str().unwrap()]));
        assert_eq!((got.code, got.out.as_str()), (1, ""), "{body}");
        assert_eq!(got.err, want, "{body}");
        assert!(!out.exists(), "{body}");
        let got = js.go(&words(&[]));
        assert_eq!((got.code, got.out.as_str()), (1, ""), "{body}");
        assert_eq!(got.err, want, "{body}");
        assert_eq!(seen.lock().unwrap().len(), 2, "{body}");
        assert_eq!(listing(&scratch.path("")), [OsString::from("token")]);
    }
}

/// Asserts that a 201 with fields next to the token and the expiry, whether strings, numbers
/// or objects, is accepted: the token goes to the file and the facts to standard error. It
/// catches a strict reader that would refuse a server that adds a field, which is a
/// compatible change.
#[test]
fn a_201_with_extra_fields_is_accepted() {
    let scratch = Scratch::new("mint_extra");
    let body = json!({
        "token": MINTED,
        "expires_at_ms": EXPIRES,
        "note": "x",
        "limits": { "uses": 1 },
        "list": [1, 2],
    });
    let (server, _) = recording(http("201 Created", &body.to_string()));
    let js = against(&server, &scratch);
    let out = scratch.path("minted");
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(out).unwrap(), format!("{MINTED}\n"));
}

/// Asserts what `--json` does on failure: standard output stays empty and standard error gets
/// one JSON object line with the failure's kind and the same message the text form prints,
/// with the exit status of the failure (4 for a rejected request, 1 for an unusable answer,
/// 3 for a refusal). It catches a failure that goes to standard output, where a pipe to
/// `js-agent enroll` would take it for a token, and a kind or status that changes with the
/// format.
#[test]
fn json_failures_go_to_standard_error_as_one_object() {
    let cases = [
        (
            "400 Bad Request",
            r#"{"error":"bad-request"}"#,
            4,
            "rejected",
            "join token not minted: the server answered 400 (bad-request)".to_owned(),
        ),
        (
            "429 Too Many Requests",
            r#"{"error":"too-many-join-tokens"}"#,
            3,
            "refused",
            "join token not minted: the server answered 429 (too-many-join-tokens)".to_owned(),
        ),
        (
            "201 Created",
            "[]",
            1,
            "mint-unconfirmed",
            unconfirmed("the server's answer has no usable \"token\"")
                .trim_start_matches("js: ")
                .trim_end()
                .to_owned(),
        ),
    ];
    for (status, body, code, kind, message) in cases {
        let scratch = Scratch::new("mint_json_failure");
        let (server, _) = recording(http(status, body));
        let js = against(&server, &scratch);
        let got = js.go(&words(&["--json"]));
        assert_eq!((got.code, got.out.as_str()), (code, ""), "{status}");
        let want = format!("{}\n", json!({ "error": kind, "message": message }));
        assert_eq!(got.err, want, "{status}");
    }
}

/// Asserts that a request that gets no answer (the connection is refused, the server reads
/// the request and closes without answering, the answer is cut short) is a failure (exit 1)
/// after at most one request, with no retry, no token on any stream, no `--out` file, nothing
/// else in the directory, and a message that says a token may have been minted. It catches a
/// retry that mints a second token, and a message that tells the operator nothing was minted
/// when the server may have done so.
#[test]
fn a_request_without_an_answer_is_not_repeated_and_says_a_token_may_exist() {
    let gone = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (closes, closed_seen) = recording(String::new());
    let cut_body = format!(r#"{{"token":"{MINTED}","expires_at_ms":{EXPIRES},"pad":"#);
    let (cuts, cut_seen) = recording(format!(
        "HTTP/1.1 201 Created\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{cut_body}"
    ));
    let cases = [
        ("refused", gone, None),
        ("closed", closes.to_string(), Some(closed_seen)),
        ("cut", cuts.to_string(), Some(cut_seen)),
    ];
    for (name, server, seen) in cases {
        let scratch = Scratch::new("mint_no_answer");
        let js = against(&server, &scratch);
        let out = scratch.path("minted");
        let before = listing(&scratch.path(""));
        let got = js.go(&words(&["--out", out.to_str().unwrap()]));
        let head = format!("js: cannot talk to {server}: ");
        let tail = unconfirmed("");
        let tail = tail.trim_start_matches("js: ").to_owned();
        assert_eq!((got.code, got.out.as_str()), (1, ""), "{name}: {}", got.err);
        assert!(got.err.starts_with(&head), "{name}: {}", got.err);
        assert!(got.err.ends_with(&tail), "{name}: {}", got.err);
        assert_eq!(got.err.matches('\n').count(), 1, "{name}: {}", got.err);
        assert!(!got.err.contains(MINTED), "{name}");
        assert_eq!(listing(&scratch.path("")), before, "{name}");

        let got = js.go(&words(&[]));
        assert_eq!((got.code, got.out.as_str()), (1, ""), "{name}: {}", got.err);
        assert!(got.err.ends_with(&tail), "{name}: {}", got.err);
        assert!(!got.err.contains(MINTED), "{name}");
        if let Some(seen) = seen {
            assert_eq!(seen.lock().unwrap().len(), 2, "{name}: one request per run");
        }
    }
}

/// A standard output that takes the first three bytes and then fails.
struct Partial {
    got: Vec<u8>,
}

impl Write for Partial {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.got.is_empty() {
            let taken = bytes.len().min(3);
            self.got.extend_from_slice(&bytes[..taken]);
            Ok(taken)
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the pipe broke after three bytes",
            ))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A standard output that takes everything and fails to flush it.
struct FlushFails {
    got: Vec<u8>,
}

impl Write for FlushFails {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.got.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("the flush was refused"))
    }
}

/// Asserts that a standard output that writes part of the token and then fails, and one whose
/// flush fails, are both a failure (exit 1) with exactly the message that the token is lost and
/// when it expires, and with no part of the token on standard error. It catches a partial
/// write taken for a delivery, a flush error that is ignored, and a token echoed in the
/// message.
#[test]
fn a_standard_output_that_fails_part_way_reports_the_token_lost() {
    let scratch = Scratch::new("mint_partial");
    let (server, _) = recording(created());
    let js = against(&server, &scratch);

    let mut partial = Partial { got: Vec::new() };
    let mut err = Vec::new();
    let code = js_cli::run_with(
        js.args(&words(&[])),
        &mut partial,
        &mut err,
        services(Arc::new(Never)),
    );
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 1, "{err}");
    assert_eq!(err, lost("the pipe broke after three bytes"));
    assert_eq!(partial.got, MINTED.as_bytes()[..3]);
    assert!(!err.contains(&MINTED[..16]));

    let mut flush = FlushFails { got: Vec::new() };
    let mut err = Vec::new();
    let code = js_cli::run_with(
        js.args(&words(&[])),
        &mut flush,
        &mut err,
        services(Arc::new(Never)),
    );
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 1, "{err}");
    assert_eq!(err, lost("the flush was refused"));
    assert!(!err.contains(&MINTED[..16]));
}

/// Asserts that a destination that names no file or cannot be looked at is refused (exit 2)
/// before any request with the exact message and nothing changed in the directory: `--out ""`,
/// a path with a trailing separator (to a directory that is there and to one that is not), a
/// path ending in `/.`, and a name of 256 bytes, which no directory can hold. It catches a
/// token minted for a path that can never be made, or an empty path taken for the current
/// directory.
#[test]
fn a_destination_that_names_no_file_is_refused_before_any_request() {
    let scratch = Scratch::new("mint_out_names");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    mkdir(&scratch.path("sub"), 0o700);
    let before = listing(&scratch.path(""));
    let base = scratch.path("");
    let base = base.to_str().unwrap().trim_end_matches('/').to_owned();
    let too_long = format!("{base}/{}", "x".repeat(256));
    let exact = [
        (
            String::new(),
            // The command line parser refuses an empty value before `js` looks at it.
            "error: a value is required for '--out <PATH>' but none was supplied\n\n\
             For more information, try '--help'.\n"
                .to_owned(),
        ),
        (
            format!("{base}/sub/"),
            format!("js: --out: cannot use \"{base}/sub/\": it ends in a separator\n"),
        ),
        (
            format!("{base}/absent/"),
            format!("js: --out: cannot use \"{base}/absent/\": it ends in a separator\n"),
        ),
        (
            format!("{base}/sub/."),
            format!("js: --out: cannot use \"{base}/sub/.\": it ends in `.`\n"),
        ),
    ];
    for (path, want) in exact {
        let got = js.go(&words(&["--out", &path]));
        assert_eq!((got.code, got.out.as_str()), (2, ""), "{path:?}");
        assert_eq!(got.err, want, "{path:?}");
        assert_eq!(listing(&scratch.path("")), before, "{path:?}");
    }
    let got = js.go(&words(&["--out", &too_long]));
    assert_eq!((got.code, got.out.as_str()), (2, ""), "{}", got.err);
    assert!(
        got.err
            .starts_with(&format!("js: --out: {too_long} cannot be examined (")),
        "{}",
        got.err
    );
    assert_eq!(listing(&scratch.path("")), before);
    assert!(listing(&scratch.path("sub")).is_empty());
    assert!(seen.lock().unwrap().is_empty(), "a token was minted");
}

/// Asserts that a destination whose name is 255 bytes, the most a name can be, gets the token
/// in a file of mode 0600 with one request and nothing else left in the directory. It catches
/// a staging name built from the destination's, which is too long when the destination is at
/// the limit.
#[test]
fn a_destination_name_of_255_bytes_works() {
    let scratch = Scratch::new("mint_out_255");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let name = "n".repeat(255);
    let out = scratch.path(&name);
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(&out).unwrap(), format!("{MINTED}\n"));
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    let mut want = vec![OsString::from(&name), OsString::from("token")];
    want.sort();
    assert_eq!(listing(&scratch.path("")), want);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Asserts that an `--out` in a directory that group or others can write without the sticky
/// bit (0777) is refused (exit 2) before any request with the exact text naming the
/// directory and the mode, and nothing made in it, and that the same directory with the
/// sticky bit (01777) gets the token in a 0600 file. It catches a token staged where another
/// user could swap the file before it is linked, and a refusal of sticky directories like
/// `/tmp`.
#[test]
fn a_directory_others_can_write_is_refused_before_any_request_unless_sticky() {
    let scratch = Scratch::new("mint_out_open_dir");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let dir = scratch.path("open");
    mkdir(&dir, 0o777);
    let out = dir.join("minted");
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    let want = format!(
        "js: --out: {} is in a directory that others could change files in (cannot use \"{}\": \
         its mode 0777 lets group or others write there and it is not sticky)\n",
        out.display(),
        dir.display()
    );
    assert_eq!((got.code, got.out.as_str()), (2, ""), "{}", got.err);
    assert_eq!(got.err, want);
    assert!(listing(&dir).is_empty());
    assert!(seen.lock().unwrap().is_empty(), "a token was minted");

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o1777)).unwrap();
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(fs::read_to_string(&out).unwrap(), format!("{MINTED}\n"));
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(listing(&dir), [OsString::from("minted")]);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Asserts that a directory swapped in for the `--out` directory while the mint is in flight
/// is judged again and nothing is published in it: `--out` is reached through a link to a
/// private directory, which passes the check before the request; while the server holds the
/// request the link is pointed at a directory others can write, without the sticky bit.
/// The mint then fails (exit 1, the token said to be lost, the directory and the reason
/// named), no file is in either directory, and the token is on no stream. It catches a
/// delivery that trusts the check made before the request and writes the secret where
/// another user could swap the staged file for one of their own.
#[test]
fn a_directory_swapped_in_after_the_check_is_judged_again_and_gets_nothing() {
    let scratch = Scratch::new("mint_swapped_dir");
    let private = scratch.path("private");
    let open = scratch.path("open");
    mkdir(&private, 0o700);
    mkdir(&open, 0o777);
    let link = scratch.path("out-dir");
    std::os::unix::fs::symlink(&private, &link).unwrap();
    let (repoint, target) = (link.clone(), open.clone());
    let (server, seen) = recording_with(created(), move || {
        fs::remove_file(&repoint).unwrap();
        std::os::unix::fs::symlink(&target, &repoint).unwrap();
    });
    let js = against(&server, &scratch);
    let out = link.join("minted");
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    assert_eq!((got.code, got.out.as_str()), (1, ""), "{}", got.err);
    assert!(
        got.err.contains("the token is lost")
            && got.err.contains(&format!("\"{}\"", link.display()))
            && got
                .err
                .contains("lets group or others write there and it is not sticky"),
        "{}",
        got.err
    );
    assert!(!got.err.contains(MINTED));
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(listing(&private).is_empty());
    assert!(listing(&open).is_empty());
}
