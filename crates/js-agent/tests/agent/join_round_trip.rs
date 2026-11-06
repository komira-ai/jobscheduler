//! The join round trip as a hostkeeper-packaged unit runs it: a token minted by `js
//! join-token mint --out` into a private file, an agent started with `--flags-file` (the
//! server, the pool, the host's id, its instance file and its state root) and `--token-file`
//! (the minted file), against a real lease server. The agent registers and holds the
//! credential in place of the token; a wrong or a revoked token is refused; a missing token
//! file is a usage failure.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::{CREDENTIAL_FILE, Dialer};
use js_cli::{Services, run_with};

use crate::jobs::private;
use crate::server::{Agent, Server};
use crate::support::{Host, Scratch, code, mac_host};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A Mac whose instance file says it runs on `lambdalabs:i-1`.
fn launched() -> Host {
    let mut host = mac_host();
    host.files = host.files.with("/instance", "lambdalabs:i-1\n");
    host
}

/// What `js` printed and returned.
struct Js {
    code: u8,
    out: String,
    err: String,
}

/// Runs `js` against `server` with its client token, through the library entry point the
/// program's `main` calls.
fn js(server: &Server, words: &[&str]) -> Js {
    let client = server.scratch.path("client-token");
    let mut args: Vec<std::ffi::OsString> = vec!["js".into(), "--server".into()];
    args.push(server.url().into());
    args.extend(["--token-file".into(), client.into()]);
    args.extend(words.iter().map(std::ffi::OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(args, &mut out, &mut err, Services::system());
    Js {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

/// Mints a token for `lambdalabs:i-1` into the new file `name` of `files` and returns the
/// file's path.
fn mint_into(server: &Server, files: &Scratch, name: &str) -> PathBuf {
    let path = files.path(name);
    let words = [
        "join-token",
        "mint",
        "--provider",
        "lambdalabs",
        "--instance-id",
        "i-1",
        "--out",
        path.to_str().unwrap(),
    ];
    let minted = js(server, &words);
    assert_eq!(minted.code, 0, "{}", minted.err);
    path
}

/// The flags file of the host: everything but the token file's path.
fn flags_file(server: &Server, files: &Scratch, state: &Path) -> PathBuf {
    let text = format!(
        "# this host\n--server={}\n--pool=macs\n--host-id=mac-1\n--instance-file=/instance\n--state-root={}\n",
        server.url(),
        state.display()
    );
    files.write("flags", &text, 0o600)
}

/// Asserts the whole path of a launched host. `js join-token mint --out` writes a file of
/// mode 0600 holding the token and a newline and prints the file and the expiry, never the
/// token. An agent started with `--flags-file` for its server, pool, host id, instance file
/// and state root, and `--token-file` for the minted file, says hello and is live; the
/// credential lands in the state root with mode 0600 and differs from the token; the
/// heartbeats that follow are accepted (the token is retired, so one that still carried it
/// would end the agent with a 401); and neither secret is on the agent's output. It catches
/// a flags file whose words do not reach the agent, a token file the agent cannot take,
/// and an agent that goes on with the token after the credential was issued.
#[test]
fn a_minted_token_and_a_flags_file_register_the_host_and_the_credential_replaces_the_token() {
    let server = Server::start("join_round_trip", &[]);
    let files = Scratch::new("join_round_trip-files");
    let token_path = files.path("join-token-file");
    let words = [
        "join-token",
        "mint",
        "--provider",
        "lambdalabs",
        "--instance-id",
        "i-1",
        "--out",
        token_path.to_str().unwrap(),
    ];
    let minted = js(&server, &words);
    assert_eq!(minted.code, 0, "{}", minted.err);
    let token_text = fs::read_to_string(&token_path).unwrap();
    let token = token_text.strip_suffix('\n').unwrap();
    assert_eq!(token.len(), 64);
    assert_eq!(
        fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!minted.out.contains(token) && !minted.err.contains(token));
    assert!(minted.out.contains("lambdalabs:i-1"), "{}", minted.out);

    let state = private(&files, "state");
    let flags = flags_file(&server, &files, &state);
    let agent = Agent::spawn_flags_file(&flags, Some(&token_path), launched, Dialer);
    assert_eq!(agent.next_wait(), secs(5), "the hello");
    let credential_file = state.join(CREDENTIAL_FILE);
    let credential = fs::read_to_string(&credential_file).unwrap();
    assert_eq!(credential.len(), 64);
    assert_ne!(credential, token);
    assert_eq!(
        fs::metadata(&credential_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    agent.elapse();
    assert_eq!(
        agent.next_wait(),
        secs(5),
        "a heartbeat with the credential"
    );
    assert_eq!(server.host("mac-1")["state"], "live");
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    for text in [&ran.out, &ran.err] {
        assert!(
            !text.contains(&credential) && !text.contains(token),
            "{text}"
        );
    }
}

/// Asserts that an agent given a token the server never minted is refused: it ends with the
/// usage status after one request, the message is the server's refusal of the hello, the
/// host is not registered, and the token is not on its output. It catches an agent that
/// retries a refusal that cannot change, and one that registers without a valid token.
#[test]
fn a_wrong_token_is_refused_and_the_host_does_not_go_live() {
    let server = Server::start("join_wrong", &[]);
    let files = Scratch::new("join_wrong-files");
    let wrong = "wrong-token-0123456789-abcdefghijklmnop";
    let token = files.write("wrong", wrong, 0o600);
    let state = private(&files, "state");
    let flags = flags_file(&server, &files, &state);
    let ran = Agent::spawn_flags_file(&flags, Some(&token), launched, Dialer).finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(
        ran.err,
        "js-agent: the server refused the hello with status 401 (unreadable); not retrying\n"
    );
    assert!(!ran.err.contains(wrong));
    assert!(!state.join(CREDENTIAL_FILE).exists());
    let listing = server.get("/v1/hosts");
    assert!(listing["hosts"].as_array().unwrap().is_empty(), "{listing}");
}

/// Asserts that a token revoked with `js join-token revoke` is refused at the hello like a
/// token that never existed: the revoke prints its line and exits 0, the agent ends with
/// the usage status and the host is not registered. It catches a revoke that leaves the
/// token good.
#[test]
fn a_revoked_token_is_refused_at_the_hello() {
    let server = Server::start("join_revoked", &[]);
    let files = Scratch::new("join_revoked-files");
    let token = mint_into(&server, &files, "token");
    let revoked = js(
        &server,
        &[
            "join-token",
            "revoke",
            "--provider",
            "lambdalabs",
            "--instance-id",
            "i-1",
        ],
    );
    assert_eq!(
        (revoked.code, revoked.out.as_str()),
        (0, "join tokens for lambdalabs:i-1 revoked\n"),
        "{}",
        revoked.err
    );
    let state = private(&files, "state");
    let flags = flags_file(&server, &files, &state);
    let ran = Agent::spawn_flags_file(&flags, Some(&token), launched, Dialer).finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("the server refused the hello with status 401"),
        "{}",
        ran.err
    );
    let listing = server.get("/v1/hosts");
    assert!(listing["hosts"].as_array().unwrap().is_empty(), "{listing}");
}

/// Asserts that a unit whose token file has not been placed yet fails as a usage error
/// (exit 2) naming `--token-file`, before any hello: the host is not registered. It catches
/// an agent that starts without a token or reports a missing file as a server failure.
#[test]
fn a_missing_token_file_is_a_usage_failure() {
    let server = Server::start("join_missing", &[]);
    let files = Scratch::new("join_missing-files");
    let state = private(&files, "state");
    let flags = flags_file(&server, &files, &state);
    let missing = files.path("not-placed");
    let ran = Agent::spawn_flags_file(&flags, Some(&missing), launched, Dialer).finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("--token-file"), "{}", ran.err);
    let listing = server.get("/v1/hosts");
    assert!(listing["hosts"].as_array().unwrap().is_empty(), "{listing}");
}
