//! `lease join-token mint` against a server that is a value: the request it makes, where the
//! token goes and what it prints. No socket is opened; the server is a fake that records
//! the bodies it was given and answers a canned reply.

mod failures;

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use clap::Parser;
use js_client::{ClientError, Reply};
use js_testkit::dir::TestDir;
use serde_json::{Value, json};

use super::*;
use crate::api::mint_tests::{EXPIRES, MINTED, reply};

/// The arguments of `mint`, as clap reads them.
#[derive(Parser)]
struct Words {
    #[command(flatten)]
    args: MintArgs,
}

/// A server that records the bodies it was given and answers with `answer`; with `during`,
/// it also runs that when a request has been read, before the answer: a test changes the
/// world while the mint is in flight.
struct Fake {
    asked: RefCell<Vec<(String, Value)>>,
    answer: Box<dyn Fn() -> Result<Reply, ClientError>>,
    during: Option<Box<dyn Fn()>>,
}

impl Post for Fake {
    fn post(&self, path: &str, body: &Value) -> Result<Reply, ClientError> {
        self.asked
            .borrow_mut()
            .push((path.to_owned(), body.clone()));
        if let Some(during) = &self.during {
            during();
        }
        (self.answer)()
    }
}

impl Fake {
    fn new(answer: impl Fn() -> Result<Reply, ClientError> + 'static) -> Self {
        Self {
            asked: RefCell::default(),
            answer: Box::new(answer),
            during: None,
        }
    }

    /// The server of a mint that succeeds.
    fn minting() -> Self {
        Self::new(created)
    }

    /// The bodies of the requests it was given.
    fn bodies(&self) -> Vec<Value> {
        self.asked.borrow().iter().map(|(_, b)| b.clone()).collect()
    }

    /// How many requests it was given.
    fn requests(&self) -> usize {
        self.asked.borrow().len()
    }
}

/// The 201 the server answers with.
fn created() -> Result<Reply, ClientError> {
    reply(201, json!({ "token": MINTED, "expires_at_ms": EXPIRES }))
}

/// What a run printed and the status it ended with.
struct Ran {
    code: u8,
    out: String,
    err: String,
}

/// Runs `mint --provider burst --instance i-0abc` plus `extra` against `server`, with a
/// standard output that is not a terminal, and ends it as the program does: a failure is
/// printed on standard error and gives its exit status.
fn go(server: &Fake, extra: &[&str], json: bool) -> Ran {
    let mut out = Vec::new();
    let (code, err) = go_with(server, extra, json, false, &mut out);
    Ran {
        code,
        out: String::from_utf8(out).unwrap(),
        err,
    }
}

/// [`go`] on the standard output `out`, which is a terminal or not.
fn go_with(
    server: &Fake,
    extra: &[&str],
    json: bool,
    terminal: bool,
    out: &mut dyn Write,
) -> (u8, String) {
    let mut words = vec!["mint", "--provider", "burst", "--instance", "i-0abc"];
    words.extend(extra);
    let args = Words::try_parse_from(words).unwrap().args;
    let render = Render::new(json);
    let mut err = Vec::new();
    let code = match mint(server, render, &args, terminal, out, &mut err) {
        Ok(code) => code,
        Err(failure) => {
            err.extend(render.failure(&failure).bytes());
            failure.parts().0
        }
    };
    (code, String::from_utf8(err).unwrap())
}

/// A scratch directory only its owner can use, whatever the umask, removed when dropped.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(std::env::temp_dir().join("js-cli-mint"), name);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A directory of mode `mode`, whatever the umask.
fn mkdir(path: &Path, mode: u32) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Gives `path` the mode `mode`.
fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The sorted names in `dir`.
fn listing(dir: &Path) -> Vec<OsString> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    names
}

/// The mode bits of `path`.
fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The sha256 of `text`, in hex.
fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The facts line on standard error.
fn facts() -> String {
    format!("join token sha256 {} expires {EXPIRES}\n", sha(MINTED))
}

/// What is said when the token was minted and could not be written to standard output for
/// `why`.
fn lost(why: &str) -> String {
    format!(
        "js: the token was minted but could not be written to standard output: {why}; the \
         token is lost (it expires {EXPIRES} on the server's clock): mint another\n"
    )
}

/// Asserts that the request body is exactly `provider`, `instance_id` and `ttl_seconds`
/// (`--ttl 15m` is 900), and nothing else. It catches a body of another shape (the server
/// refuses an unknown field) and a ttl in another unit.
#[test]
fn the_request_has_exactly_the_three_fields() {
    let dir = scratch("request");
    let server = Fake::minting();
    let out = dir.join("minted");
    let got = go(
        &server,
        &["--ttl", "15m", "--out", out.to_str().unwrap()],
        false,
    );
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(
        server.bodies(),
        [json!({ "provider": "burst", "instance_id": "i-0abc", "ttl_seconds": 900 })]
    );
    assert_eq!(server.asked.borrow()[0].0, "/v1/agent/join-tokens");
}

/// Asserts that without `--ttl` no ttl is sent, so the server's default applies, and that a
/// bare number is seconds. It catches a made-up default sent as if the server's.
#[test]
fn without_a_ttl_none_is_sent_and_a_bare_number_is_seconds() {
    let dir = scratch("ttl");
    let server = Fake::minting();
    let first = dir.join("first");
    let second = dir.join("second");
    assert_eq!(
        go(&server, &["--out", first.to_str().unwrap()], false).code,
        0
    );
    let got = go(
        &server,
        &["--ttl", "90", "--out", second.to_str().unwrap()],
        false,
    );
    assert_eq!(got.code, 0, "{}", got.err);
    let bodies = server.bodies();
    assert_eq!(
        bodies[0],
        json!({ "provider": "burst", "instance_id": "i-0abc" })
    );
    assert_eq!(bodies[1]["ttl_seconds"], json!(90));
}

/// Asserts that `--out` gets the token and a line break in a new file of mode 0600, nothing
/// else is left in the directory, standard output stays empty, and standard error has the
/// sha256 and the expiry and not the token. It catches a file that others can read, a staged
/// file left behind, and a token on a stream.
#[test]
fn out_gets_the_token_in_a_new_private_file() {
    let dir = scratch("out");
    let server = Fake::minting();
    let out = dir.join("minted");
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(&out).unwrap(), format!("{MINTED}\n"));
    assert_eq!(mode(&out), 0o600);
    assert_eq!(listing(dir.path()), [OsString::from("minted")]);
}

/// Asserts that without `--out` the token goes to standard output with a line break when it
/// is not a terminal, and the facts to standard error. It catches a token that goes to the
/// wrong stream, where a pipe to `js-agent enroll` would miss it.
#[test]
fn without_out_the_token_goes_to_a_standard_output_that_is_not_a_terminal() {
    let server = Fake::minting();
    let got = go(&server, &[], false);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, format!("{MINTED}\n"));
    assert_eq!(got.err, facts());
}

/// Asserts that `--json` prints the facts as one JSON object on standard error and leaves
/// the token as it was on standard output. It catches a JSON mode that moves the token.
#[test]
fn json_prints_the_facts_as_an_object() {
    let server = Fake::minting();
    let got = go(&server, &[], true);
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, format!("{MINTED}\n"));
    let facts: Value = serde_json::from_str(&got.err).unwrap();
    assert_eq!(
        facts,
        json!({ "sha256": sha(MINTED), "expires_at_ms": EXPIRES })
    );
}

/// Asserts that a terminal on standard output is refused (exit 2) before any request is
/// made, so no token is minted to be thrown away, with nothing printed to standard output;
/// and that the same terminal is fine when `--out` names the file. It catches a token shown
/// on a screen (scrollback, a shared terminal) and a mint that happens first.
#[test]
fn a_terminal_gets_no_token_and_no_request() {
    let dir = scratch("terminal");
    let server = Fake::minting();
    let mut out = Vec::new();
    let (code, err) = go_with(&server, &[], false, true, &mut out);
    assert_eq!((code, out.as_slice()), (2, b"".as_slice()), "{err}");
    assert!(
        err.starts_with("js: standard output is a terminal") && err.contains("--out"),
        "{err}"
    );
    assert_eq!(server.requests(), 0, "a token was minted for a terminal");

    let file = dir.join("minted");
    let (code, err) = go_with(
        &server,
        &["--out", file.to_str().unwrap()],
        false,
        true,
        &mut out,
    );
    assert_eq!((code, out.as_slice()), (0, b"".as_slice()), "{err:?}");
    assert_eq!(fs::read_to_string(file).unwrap(), format!("{MINTED}\n"));
    assert_eq!(server.requests(), 1);
}

/// Asserts that an `--out` that exists (a file, or a link to nothing) and one whose
/// directory is not there are refused (exit 2) before any request is made, leaving the file
/// as it was. It catches a no-clobber that replaces a file, and a token minted that has
/// nowhere to go.
#[test]
fn an_out_that_cannot_be_made_is_refused_before_any_request() {
    let dir = scratch("refused");
    let server = Fake::minting();
    let taken = dir.join("taken");
    fs::write(&taken, "keep me").unwrap();
    std::os::unix::fs::symlink(dir.join("absent"), dir.join("dangling")).unwrap();
    let nowhere = dir.join("nowhere").join("minted");
    for (path, why) in [
        (taken.clone(), "exists"),
        (dir.join("dangling"), "exists"),
        (nowhere, "directory"),
    ] {
        let got = go(&server, &["--out", path.to_str().unwrap()], false);
        assert_eq!(got.code, 2, "{}", got.err);
        assert!(
            got.err.contains("--out") && got.err.contains(why),
            "{}",
            got.err
        );
        assert_eq!(got.out, "");
    }
    assert_eq!(fs::read_to_string(taken).unwrap(), "keep me");
    assert_eq!(server.requests(), 0);
}

/// Asserts that a directory the token cannot be made in (read-only) is refused (exit 2)
/// before any request, so no token is minted for it; and that a directory that can be
/// written and searched but not read (0300), where only the sync of the directory fails,
/// still gets the file and exit 0. It catches a token minted for a destination that was
/// never usable, and a good token file reported as lost.
#[test]
fn the_out_directory_is_probed_before_the_request_and_a_sync_failure_is_not_a_loss() {
    let dir = scratch("dirs");
    let server = Fake::minting();
    for (name, mode) in [("readonly", 0o500), ("unreadable", 0o300)] {
        mkdir(&dir.join(name), mode);
    }
    let refused = dir.join("readonly").join("minted");
    let got = go(&server, &["--out", refused.to_str().unwrap()], false);
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(
        got.err.contains("--out") && got.err.contains("directory"),
        "{}",
        got.err
    );
    assert_eq!(
        server.requests(),
        0,
        "a token was minted for a read-only directory"
    );

    let kept = dir.join("unreadable").join("minted");
    let got = go(&server, &["--out", kept.to_str().unwrap()], false);
    for name in ["readonly", "unreadable"] {
        chmod(&dir.join(name), 0o700);
    }
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(kept).unwrap(), format!("{MINTED}\n"));
}

/// Asserts that a destination whose name is 255 bytes, the most a name can be, gets the
/// token in a file of mode 0600 with one request and nothing else left in the directory. It
/// catches a staging name built from the destination's, which is too long when the
/// destination is at the limit.
#[test]
fn a_destination_name_of_255_bytes_works() {
    let dir = scratch("name_255");
    let server = Fake::minting();
    let name = "n".repeat(255);
    let out = dir.join(&name);
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(&out).unwrap(), format!("{MINTED}\n"));
    assert_eq!(mode(&out), 0o600);
    assert_eq!(listing(dir.path()), [OsString::from(&name)]);
    assert_eq!(server.requests(), 1);
}

/// Asserts that a destination that names no file or cannot be looked at is refused (exit 2)
/// before any request with the exact message and nothing changed in the directory: a path
/// with a trailing separator (to a directory that is there and to one that is not), a path
/// ending in `/.`, and a name of 256 bytes, which no directory can hold. It catches a token
/// minted for a path that can never be made. (An empty `--out` is clap's refusal, tested on
/// the command line.)
#[test]
fn a_destination_that_names_no_file_is_refused_before_any_request() {
    let dir = scratch("names");
    let server = Fake::minting();
    mkdir(&dir.join("sub"), 0o700);
    let before = listing(dir.path());
    let base = dir
        .path()
        .to_str()
        .unwrap()
        .trim_end_matches('/')
        .to_owned();
    let too_long = format!("{base}/{}", "x".repeat(256));
    let exact = [
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
        let got = go(&server, &["--out", &path], false);
        assert_eq!((got.code, got.out.as_str()), (2, ""), "{path:?}");
        assert_eq!(got.err, want, "{path:?}");
        assert_eq!(listing(dir.path()), before, "{path:?}");
    }
    let got = go(&server, &["--out", &too_long], false);
    assert_eq!((got.code, got.out.as_str()), (2, ""), "{}", got.err);
    assert!(
        got.err
            .starts_with(&format!("js: --out: {too_long} cannot be examined (")),
        "{}",
        got.err
    );
    assert_eq!(listing(dir.path()), before);
    assert!(listing(&dir.join("sub")).is_empty());
    assert_eq!(server.requests(), 0, "a token was minted");
}

/// Asserts that an `--out` in a directory that group or others can write without the sticky
/// bit (0777) is refused (exit 2) before any request with the exact text naming the
/// directory and the mode, and nothing made in it, and that the same directory with the
/// sticky bit (01777) gets the token in a 0600 file. It catches a token staged where
/// another user could swap the file before it is linked, and a refusal of sticky
/// directories like `/tmp`.
#[test]
fn a_directory_others_can_write_is_refused_before_any_request_unless_sticky() {
    let dir = scratch("open_dir");
    let server = Fake::minting();
    let open = dir.join("open");
    mkdir(&open, 0o777);
    let out = open.join("minted");
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
    let want = format!(
        "js: --out: {} is in a directory that others could change files in (cannot use \"{}\": \
         its mode 0777 lets group or others write there and it is not sticky)\n",
        out.display(),
        open.display()
    );
    assert_eq!((got.code, got.out.as_str()), (2, ""), "{}", got.err);
    assert_eq!(got.err, want);
    assert!(listing(&open).is_empty());
    assert_eq!(server.requests(), 0, "a token was minted");

    chmod(&open, 0o1777);
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(fs::read_to_string(&out).unwrap(), format!("{MINTED}\n"));
    assert_eq!(mode(&out), 0o600);
    assert_eq!(listing(&open), [OsString::from("minted")]);
    assert_eq!(server.requests(), 1);
}

/// A standard output that fails.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the pipe is closed",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

/// Asserts that a token that was minted and cannot be delivered (standard output closed;
/// the `--out` directory made read-only after the request) is a failure (exit 1) that says
/// the token is lost and when it expires, and shows no token. It catches a silent loss that
/// leaves the operator waiting for a host that can never join.
#[test]
fn a_minted_token_that_cannot_be_delivered_is_reported_lost() {
    let server = Fake::minting();
    let (code, err) = go_with(&server, &[], false, false, &mut Closed);
    assert_eq!(code, 1, "{err}");
    assert_eq!(err, lost("the pipe is closed"));

    let dir = scratch("undelivered");
    let outdir = dir.join("outdir");
    mkdir(&outdir, 0o700);
    let mut server = Fake::minting();
    let locked = outdir.clone();
    server.during = Some(Box::new(move || chmod(&locked, 0o500)));
    let out = outdir.join("minted");
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
    chmod(&outdir, 0o700);
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(
        got.err.contains("could not be written to")
            && got.err.contains("minted")
            && got.err.contains("lost"),
        "{}",
        got.err
    );
    assert!(!out.exists());
    assert_eq!(server.requests(), 1);
}

/// Asserts that a standard output that writes part of the token and then fails, and one
/// whose flush fails, are both a failure (exit 1) with exactly the message that the token
/// is lost and when it expires, and with no part of the token on standard error. It catches
/// a partial write taken for a delivery, a flush error that is ignored, and a token echoed
/// in the message.
#[test]
fn a_standard_output_that_fails_part_way_reports_the_token_lost() {
    let server = Fake::minting();
    let mut partial = Partial { got: Vec::new() };
    let (code, err) = go_with(&server, &[], false, false, &mut partial);
    assert_eq!(code, 1, "{err}");
    assert_eq!(err, lost("the pipe broke after three bytes"));
    assert_eq!(partial.got, MINTED.as_bytes()[..3]);
    assert!(!err.contains(&MINTED[..16]));

    let mut flush = FlushFails { got: Vec::new() };
    let (code, err) = go_with(&server, &[], false, false, &mut flush);
    assert_eq!(code, 1, "{err}");
    assert_eq!(err, lost("the flush was refused"));
    assert_eq!(flush.got, format!("{MINTED}\n").as_bytes());
    assert!(!err.contains(&MINTED[..16]));
}

/// Asserts that a directory swapped in for the `--out` directory while the mint is in flight
/// is judged again and nothing is published in it: `--out` is reached through a link to a
/// private directory, which passes the check before the request; while the server holds the
/// request the link is pointed at a directory others can write, without the sticky bit. The
/// mint then fails (exit 1, the token said to be lost, the directory and the reason named),
/// no file is in either directory, and the token is on no stream. It catches a delivery
/// that trusts the check made before the request and writes the secret where another user
/// could swap the staged file for one of their own.
#[test]
fn a_directory_swapped_in_after_the_check_is_judged_again_and_gets_nothing() {
    let dir = scratch("swapped");
    let private = dir.join("private");
    let open = dir.join("open");
    mkdir(&private, 0o700);
    mkdir(&open, 0o777);
    let link = dir.join("out-dir");
    std::os::unix::fs::symlink(&private, &link).unwrap();
    let mut server = Fake::minting();
    let (repoint, target) = (link.clone(), open.clone());
    server.during = Some(Box::new(move || {
        fs::remove_file(&repoint).unwrap();
        std::os::unix::fs::symlink(&target, &repoint).unwrap();
    }));
    let out = link.join("minted");
    let got = go(&server, &["--out", out.to_str().unwrap()], false);
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
    assert_eq!(server.requests(), 1);
    assert!(listing(&private).is_empty());
    assert!(listing(&open).is_empty());
}
