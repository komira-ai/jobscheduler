//! `lease join-token mint`: the request it sends, where the token goes, and what it prints.
//! The server is a thread on 127.0.0.1 that records what it reads and answers a canned reply.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::{Never, Scratch, TOKEN, against, http, services};

/// A token as the server mints it.
pub(crate) const MINTED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
pub(crate) const EXPIRES: u64 = 1_800_000_000_000;

/// The 201 the server answers with.
pub(crate) fn created() -> String {
    created_with(&json!({ "token": MINTED, "expires_at_ms": EXPIRES }))
}

pub(crate) fn created_with(body: &Value) -> String {
    http("201 Created", &body.to_string())
}

pub(crate) fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// What the recording server read: the head (request line and headers) and the body of
/// each request.
pub(crate) type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A recording server for one test: the URL to reach it by, and its thread, which is stopped
/// and joined when the server is dropped at the end of the test, so no test leaves a listener
/// behind. It derefs to its URL.
pub(crate) struct Server {
    url: String,
    addr: std::net::SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::ops::Deref for Server {
    type Target = str;

    fn deref(&self) -> &str {
        &self.url
    }
}

impl std::fmt::Display for Server {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.url)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wakes the accept: the thread sees the flag and ends.
        let _ = std::net::TcpStream::connect(self.addr);
        // A thread that panicked has already failed its test; a panic here would hide that.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A server on an ephemeral port of the loopback interface that answers every request with
/// `reply` and records what it read.
pub(crate) fn recording(reply: String) -> (Server, Seen) {
    recording_with(reply, || {})
}

/// [`recording`] that also runs `on_request` when a request has been read, before the reply:
/// a test changes the world while the mint is in flight.
pub(crate) fn recording_with(
    reply: String,
    on_request: impl Fn() + Send + 'static,
) -> (Server, Seen) {
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let mut stream = stream.unwrap();
            let (head, body) = read(&mut stream);
            log.lock().unwrap().push((head, body));
            on_request();
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    let server = Server {
        url: format!("http://{addr}"),
        addr,
        stop,
        thread: Some(thread),
    };
    (server, seen)
}

/// Reads one request: its head and its body as JSON.
fn read(stream: &mut impl Read) -> (String, Value) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .find_map(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    let _ = stream.read_exact(&mut body);
    (head, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// The words of a mint for `burst:i-0abc` plus `extra`.
pub(crate) fn words<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec![
        "lease",
        "join-token",
        "mint",
        "--provider",
        "burst",
        "--instance",
        "i-0abc",
    ];
    words.extend(extra);
    words
}

/// The facts line on standard error.
pub(crate) fn facts() -> String {
    format!("join token sha256 {} expires {EXPIRES}\n", sha(MINTED))
}

/// Asserts the request: a POST of the mint route with the client token as the bearer and a
/// JSON body of exactly `provider`, `instance_id` and `ttl_seconds` (`--ttl 15m` is 900). It
/// catches a body of another shape (the server refuses an unknown field), a ttl in another
/// unit, and a token that is not sent.
#[test]
fn the_request_has_exactly_the_three_fields() {
    let scratch = Scratch::new("mint_request");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let out = scratch.path("minted");
    let got = js.go(&words(&["--ttl", "15m", "--out", out.to_str().unwrap()]));
    assert_eq!(got.code, 0, "{}", got.err);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let head = seen[0].0.to_ascii_lowercase();
    assert!(
        head.starts_with("post /v1/agent/join-tokens http/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&format!(
            "authorization: bearer {}",
            TOKEN.to_ascii_lowercase()
        )),
        "{head}"
    );
    assert!(head.contains("content-type: application/json"), "{head}");
    assert_eq!(
        seen[0].1,
        json!({ "provider": "burst", "instance_id": "i-0abc", "ttl_seconds": 900 })
    );
}

/// Asserts that without `--ttl` no ttl is sent, so the server's default applies, and that a
/// bare number is seconds. It catches a made-up default sent as if the server's.
#[test]
fn without_a_ttl_none_is_sent_and_a_bare_number_is_seconds() {
    let scratch = Scratch::new("mint_ttl");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let first = scratch.path("first");
    let second = scratch.path("second");
    assert_eq!(js.go(&words(&["--out", first.to_str().unwrap()])).code, 0);
    let got = js.go(&words(&["--ttl", "90", "--out", second.to_str().unwrap()]));
    assert_eq!(got.code, 0, "{}", got.err);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].1,
        json!({ "provider": "burst", "instance_id": "i-0abc" })
    );
    assert_eq!(seen[1].1["ttl_seconds"], json!(90));
}

/// Asserts that `--out` gets the token and a line break in a new file of mode 0600, nothing
/// else is left in the directory, standard output stays empty, and standard error has the
/// sha256 and the expiry and not the token. It catches a file that others can read, a staged
/// file left behind, and a token on a stream.
#[test]
fn out_gets_the_token_in_a_new_private_file() {
    let scratch = Scratch::new("mint_out");
    let (server, _) = recording(created());
    let js = against(&server, &scratch);
    let out = scratch.path("minted");
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(&out).unwrap(), format!("{MINTED}\n"));
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    let mut names: Vec<_> = fs::read_dir(scratch.path(""))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["minted", "token"].map(OsString::from));
}

/// Asserts that without `--out` the token goes to standard output with a line break when it
/// is not a terminal, and the facts to standard error. It catches a token that goes to the
/// wrong stream, where a pipe to `js-agent enroll` would miss it.
#[test]
fn without_out_the_token_goes_to_a_standard_output_that_is_not_a_terminal() {
    let scratch = Scratch::new("mint_stdout");
    let (server, _) = recording(created());
    let js = against(&server, &scratch);
    let got = js.go(&words(&[]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, format!("{MINTED}\n"));
    assert_eq!(got.err, facts());
}

/// Asserts that a terminal on standard output is refused (exit 2) before any request is
/// sent, so no token is minted to be thrown away, with nothing printed to standard output;
/// and that the same terminal is fine when `--out` names the file. It catches a token shown
/// on a screen (scrollback, a shared terminal) and a mint that happens first.
#[test]
fn a_terminal_gets_no_token_and_no_request() {
    let scratch = Scratch::new("mint_terminal");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let args = js.args(&words(&[]));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let on_terminal = services(Arc::new(Never)).with_terminal_stdout(true);
    let code = js_cli::run_with(args, &mut out, &mut err, on_terminal);
    let err = String::from_utf8(err).unwrap();
    assert_eq!((code, out.as_slice()), (2, b"".as_slice()), "{err}");
    assert!(
        err.contains("standard output is a terminal") && err.contains("--out"),
        "{err}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "a token was minted for a terminal"
    );

    let file = scratch.path("minted");
    let args = js.args(&words(&["--out", file.to_str().unwrap()]));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let on_terminal = services(Arc::new(Never)).with_terminal_stdout(true);
    let code = js_cli::run_with(args, &mut out, &mut err, on_terminal);
    assert_eq!((code, out.as_slice()), (0, b"".as_slice()), "{err:?}");
    assert_eq!(fs::read_to_string(file).unwrap(), format!("{MINTED}\n"));
}

/// Asserts that an `--out` that exists (a file, or a link to one) and one whose directory is
/// not there are refused (exit 2) before any request is sent, leaving the file as it was. It
/// catches a no-clobber that replaces a file, and a token minted that has nowhere to go.
#[test]
fn an_out_that_cannot_be_made_is_refused_before_any_request() {
    let scratch = Scratch::new("mint_out_refused");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let taken = scratch.write("taken", "keep me", 0o600);
    std::os::unix::fs::symlink(scratch.path("absent"), scratch.path("dangling")).unwrap();
    let nowhere = scratch.path("nowhere").join("minted");
    for (path, why) in [
        (taken.clone(), "exists"),
        (scratch.path("dangling"), "exists"),
        (nowhere, "directory"),
    ] {
        let got = js.go(&words(&["--out", path.to_str().unwrap()]));
        assert_eq!(got.code, 2, "{}", got.err);
        assert!(
            got.err.contains("--out") && got.err.contains(why),
            "{}",
            got.err
        );
        assert_eq!(got.out, "");
    }
    assert_eq!(fs::read_to_string(taken).unwrap(), "keep me");
    assert!(seen.lock().unwrap().is_empty());
}

/// Asserts that a refusal prints the status and the server's error code and nothing else of
/// the body (a reason, a page), with the exit status of its kind, and writes nothing: 400 is
/// a rejected request (4), 404 and 429 are refusals (3), anything else a server failure (1);
/// a code is cut to 64 characters and loses its control characters. It catches a body
/// printed whole, which a proxy's error page could fill with secrets.
#[test]
fn a_refusal_prints_the_status_and_the_code_only() {
    let long = "c".repeat(300);
    let cases: [(&str, String, u8, String); 6] = [
        (
            "400 Bad Request",
            r#"{"error":"bad-request","reason":"provider must be the pool's name, SECRET-REASON"}"#
                .to_owned(),
            4,
            "400 (bad-request)".to_owned(),
        ),
        (
            "401 Unauthorized",
            r#"{"error":"unauthorized"}"#.to_owned(),
            1,
            "401 (unauthorized)".to_owned(),
        ),
        (
            "429 Too Many Requests",
            r#"{"error":"too-many-join-tokens"}"#.to_owned(),
            3,
            "429 (too-many-join-tokens)".to_owned(),
        ),
        (
            "404 Not Found",
            r#"{"error":"not-found"}"#.to_owned(),
            3,
            "404 (not-found)".to_owned(),
        ),
        (
            "503 Service Unavailable",
            "<html>SECRET-PAGE</html>".to_owned(),
            1,
            "503 (none)".to_owned(),
        ),
        (
            "500 Internal Server Error",
            format!(r#"{{"error":"\u001b[31m{long}"}}"#),
            1,
            format!("500 ([31m{})", "c".repeat(60)),
        ),
    ];
    for (status, body, code, said) in cases {
        let scratch = Scratch::new("mint_refused");
        let (server, _) = recording(http(status, &body));
        let js = against(&server, &scratch);
        let out = scratch.path("minted");
        let got = js.go(&words(&["--out", out.to_str().unwrap()]));
        assert_eq!(got.code, code, "{status}: {}", got.err);
        assert_eq!(got.out, "");
        let want = format!("js: join token not minted: the server answered {said}\n");
        assert_eq!(got.err, want, "{status}");
        assert!(!out.exists());
    }
}

/// A standard output that fails.
pub(crate) struct Closed;

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

/// What is said when the token was minted and could not be written to standard output for
/// `why`.
pub(crate) fn lost(why: &str) -> String {
    format!(
        "js: the token was minted but could not be written to standard output: {why}; the \
         token is lost (it expires {EXPIRES} on the server's clock): mint another\n"
    )
}

/// Asserts that a token that was minted and cannot be delivered (standard output closed; the
/// `--out` directory made read-only after the request) is a failure (exit 1) that says the token is
/// lost and when it expires, and shows no token. It catches a silent loss that leaves the
/// operator waiting for a host that can never join.
#[test]
fn a_minted_token_that_cannot_be_delivered_is_reported_lost() {
    let scratch = Scratch::new("mint_undelivered");
    let (server, _) = recording(created());
    let js = against(&server, &scratch);

    let (mut err, mut closed) = (Vec::new(), Closed);
    let code = js_cli::run_with(
        js.args(&words(&[])),
        &mut closed,
        &mut err,
        services(Arc::new(Never)),
    );
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, 1, "{err}");
    assert_eq!(err, lost("the pipe is closed"));

    let outdir = scratch.path("outdir");
    fs::create_dir(&outdir).unwrap();
    fs::set_permissions(&outdir, fs::Permissions::from_mode(0o700)).unwrap();
    let (server, seen) = {
        let locked = outdir.clone();
        recording_with(created(), move || {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        })
    };
    let js = against(&server, &scratch);
    let out = outdir.join("minted");
    let got = js.go(&words(&["--out", out.to_str().unwrap()]));
    fs::set_permissions(&outdir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(got.code, 1, "{}", got.err);
    assert!(
        got.err.contains("could not be written to")
            && got.err.contains("minted")
            && got.err.contains("lost"),
        "{}",
        got.err
    );
    assert!(!out.exists());
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Asserts that a directory the token cannot be made in (read-only) is refused (exit 2)
/// before any request, so no token is minted for it; and that a directory that can be written
/// and searched but not read (0300), where only the sync of the directory fails, still gets
/// the file and exit 0. It catches a token minted for a destination that was never usable,
/// and a good token file reported as lost.
#[test]
fn the_out_directory_is_probed_before_the_request_and_a_sync_failure_is_not_a_loss() {
    let scratch = Scratch::new("mint_out_dirs");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    for (name, mode) in [("readonly", 0o500), ("unreadable", 0o300)] {
        fs::create_dir(scratch.path(name)).unwrap();
        fs::set_permissions(scratch.path(name), fs::Permissions::from_mode(mode)).unwrap();
    }
    let refused = scratch.path("readonly").join("minted");
    let got = js.go(&words(&["--out", refused.to_str().unwrap()]));
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(
        got.err.contains("--out") && got.err.contains("directory"),
        "{}",
        got.err
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "a token was minted for a read-only directory"
    );

    let kept = scratch.path("unreadable").join("minted");
    let got = js.go(&words(&["--out", kept.to_str().unwrap()]));
    for name in ["readonly", "unreadable"] {
        fs::set_permissions(scratch.path(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!((got.code, got.out.as_str()), (0, ""), "{}", got.err);
    assert_eq!(got.err, facts());
    assert_eq!(fs::read_to_string(kept).unwrap(), format!("{MINTED}\n"));
}

/// Asserts that the help says where the JSON and the token go with `--json`, and that the
/// server is what refuses a ttl over a day. It catches a help that leaves a user to find out
/// by losing a token.
#[test]
fn the_help_says_where_things_go_and_who_limits_the_ttl() {
    let scratch = Scratch::new("mint_help");
    let (server, _) = recording(created());
    let js = against(&server, &scratch);
    let got = js.go(&["lease", "join-token", "mint", "--help"]);
    assert_eq!(got.code, 0, "{}", got.err);
    // The help is wrapped to the width of the terminal: compare it as words.
    let words = got.out.split_whitespace().collect::<Vec<_>>().join(" ");
    for want in [
        "JSON object on standard error",
        "token stays on standard output",
        "refuses more than a day",
    ] {
        assert!(words.contains(want), "{want}: {}", got.out);
    }
}

/// Asserts that `--json` prints the facts as one JSON object on standard error and leaves
/// the token as it was on standard output. It catches a JSON mode that moves the token.
#[test]
fn json_prints_the_facts_as_an_object() {
    let scratch = Scratch::new("mint_json");
    let (server, _) = recording(created());
    let js = against(&server, &scratch);
    let got = js.go(&words(&["--json"]));
    assert_eq!(got.code, 0, "{}", got.err);
    assert_eq!(got.out, format!("{MINTED}\n"));
    let facts: Value = serde_json::from_str(&got.err).unwrap();
    assert_eq!(
        facts,
        json!({ "sha256": sha(MINTED), "expires_at_ms": EXPIRES })
    );
}

/// Asserts that the command line takes no token, requires both ids and refuses a zero ttl
/// (exit 2) without a request. It catches a flag that would put a secret in `ps`.
#[test]
fn the_command_line_has_no_token_and_needs_both_ids() {
    let scratch = Scratch::new("mint_flags");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let refused: [&[&str]; 4] = [
        &["--token", "x"],
        &["lease", "join-token", "mint", "--provider", "burst"],
        &["lease", "join-token", "mint", "--instance", "i-0abc"],
        &["--ttl", "0"],
    ];
    for extra in refused {
        let all = match extra[0] {
            "lease" => extra.to_vec(),
            _ => words(extra),
        };
        let got = js.go(&all);
        assert_eq!(got.code, 2, "{extra:?}: {}", got.err);
    }
    assert!(seen.lock().unwrap().is_empty());
}

/// Asserts that a client token file others can read or write is refused
/// (status 2) with its path and mode before any request, and that nothing is minted or
/// written to `--out`. It catches a mint that sends a client token other users could have
/// read, which would let them mint join tokens of their own.
#[test]
fn a_client_token_file_others_can_read_is_refused_before_any_request() {
    let scratch = Scratch::new("mint_client_token");
    let (server, seen) = recording(created());
    let js = against(&server, &scratch);
    let out = scratch.path("minted");
    for mode in [0o644, 0o640, 0o604, 0o660, 0o606] {
        fs::set_permissions(&js.token_file, fs::Permissions::from_mode(mode)).unwrap();
        let got = js.go(&words(&["--out", out.to_str().unwrap()]));
        assert_eq!(got.code, 2, "{mode:o}: {}", got.err);
        assert!(
            got.err.contains(js.token_file.to_str().unwrap())
                && got.err.contains(&format!("mode {mode:04o}")),
            "{mode:o}: {}",
            got.err
        );
        assert_eq!(got.out, "");
        let got = js.go(&words(&[]));
        assert_eq!(
            (got.code, got.out.as_str()),
            (2, ""),
            "{mode:o}: {}",
            got.err
        );
    }
    assert!(!out.exists());
    assert!(seen.lock().unwrap().is_empty());
}
