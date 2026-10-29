//! The host credential against a real lease server in the test process: an agent with a
//! state root is given a credential in place of its one-time token, goes on with it, and
//! finds it again after a restart; an agent without a state root, and an agent in a pool
//! joined by the shared token, keep the token path; a welcome that is lost leaves the host
//! with no way in.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::{Answer, CREDENTIAL_FILE, Connector, Dialer, HELLO_PATH, Transport};
use js_client::ClientError;
use serde_json::{Value, json};

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

/// A one-time token for `lambdalabs:i-1`, minted behind the client token, in a file of
/// mode 0600.
fn one_time_token(server: &Server, files: &Scratch) -> PathBuf {
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    let minted = server.post_body("/v1/agent/join-tokens", &body);
    assert_eq!(minted.status, 201, "{}", minted.body);
    let token = minted.body["token"].as_str().unwrap();
    files.write("one-time-token", token, 0o600)
}

fn flags<'a>(state: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec!["--host-id", "mac-1", "--instance-file", "/instance"];
    all.extend(["--state-root", state]);
    all.extend_from_slice(extra);
    all
}

/// Asserts the path of a launched host: its first hello is given a credential, which lands
/// in the state root with mode 0600 before the agent's next call; its heartbeats go on
/// (the server has retired the token, so a heartbeat that still carried it would end the
/// agent with a 401); a second agent over the same state root and token file, started after
/// the first session lapsed on the server, registers with the file's credential; and
/// neither the credential nor the token is on any output. It catches an agent that
/// ignores the credential and is cut off at its first heartbeat, one that does not
/// survive a restart, and one that prints a secret.
#[test]
fn a_launched_agent_goes_on_with_its_credential_and_finds_it_after_a_restart() {
    let server = Server::start("cred_agent", &[]);
    let files = Scratch::new("cred_agent-files");
    let token = one_time_token(&server, &files);
    let token_text = fs::read_to_string(&token).unwrap();
    let state = private(&files, "state");
    let state_flag = state.display().to_string();
    let agent = Agent::spawn(
        server.url(),
        token.clone(),
        &flags(&state_flag, &[]),
        launched,
        Dialer,
    );
    assert_eq!(agent.next_wait(), secs(5), "the hello");
    let file = state.join(CREDENTIAL_FILE);
    let credential = fs::read_to_string(&file).unwrap();
    assert_eq!(credential.len(), 64);
    assert_ne!(credential, token_text);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for _ in 0..2 {
        agent.elapse();
        assert_eq!(
            agent.next_wait(),
            secs(5),
            "a heartbeat with the credential"
        );
    }
    assert_eq!(server.host("mac-1")["state"], "live");
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err.contains("the host credential is saved"),
        "{}",
        ran.err
    );
    for text in [&ran.err, &ran.out] {
        assert!(
            !text.contains(&credential) && !text.contains(&token_text),
            "{text}"
        );
    }

    server.clock.advance(secs(60));
    let again = Agent::spawn(
        server.url(),
        token,
        &flags(&state_flag, &[]),
        launched,
        Dialer,
    );
    assert_eq!(
        again.next_wait(),
        secs(5),
        "the hello with the file's credential"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), credential);
    let ran = again.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran.err.contains("host credential"), "{}", ran.err);
}

/// Asserts that an agent with no state root (it says nothing about a credential) keeps its
/// one-time token for every call, and that a new agent with a state root in a pool joined by
/// the shared token is welcomed without a credential and writes no file: the older-agent and
/// the older-server halves of the wire change. It catches a server that retires the token
/// of an agent that cannot keep a credential, and an agent that makes a file nobody gave it.
#[test]
fn without_a_state_root_or_a_credential_the_token_is_what_the_agent_uses() {
    let server = Server::start("cred_older", &[]);
    let files = Scratch::new("cred_older-files");
    let token = one_time_token(&server, &files);
    let bare = Agent::spawn(
        server.url(),
        token,
        &["--host-id", "mac-1", "--instance-file", "/instance"],
        launched,
        Dialer,
    );
    assert_eq!(bare.next_wait(), secs(5));
    bare.elapse();
    assert_eq!(bare.next_wait(), secs(5), "a heartbeat with the token");
    let ran = bare.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);

    let state = private(&files, "pooled-state");
    let state_flag = state.display().to_string();
    let pooled = Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-2", "--state-root", &state_flag],
        mac_host,
        Dialer,
    );
    assert_eq!(pooled.next_wait(), secs(5));
    pooled.elapse();
    assert_eq!(
        pooled.next_wait(),
        secs(5),
        "a heartbeat with the shared token"
    );
    let ran = pooled.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!state.join(CREDENTIAL_FILE).exists());
}

/// Answers the hello for real and then reports the connection lost, once, so the server has
/// admitted the host and the agent never sees its welcome.
struct LoseWelcome;

struct Losing {
    inner: Box<dyn Transport>,
    lost: bool,
}

impl Connector for LoseWelcome {
    fn connect(
        &self,
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, token_file, ca_cert)?;
        Ok(Box::new(Losing { inner, lost: false }))
    }
}

impl Transport for Losing {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let answer = self.inner.post(path, body)?;
        let first_hello = path == HELLO_PATH && !self.lost;
        self.lost |= first_hello;
        match first_hello {
            true => Err("the connection was lost".to_owned()),
            false => Ok(answer),
        }
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// Asserts the lost-welcome case from the agent's side: the server admitted the host and
/// retired the token, the welcome never arrived, so the agent has no credential; its second
/// hello with the token is refused with a 401 and the agent ends with the usage status,
/// naming the refusal, with no credential file written. The host cannot register again; the
/// instance's release (the scaler's boot timeout) is what frees it. It catches a token that
/// stays good after the credential was issued, and an agent that loops on a refusal that
/// cannot change.
#[test]
fn a_lost_welcome_leaves_the_agent_refused_for_good() {
    let server = Server::start("cred_lost", &[]);
    let files = Scratch::new("cred_lost-files");
    let token = one_time_token(&server, &files);
    let state = private(&files, "state");
    let state_flag = state.display().to_string();
    let agent = Agent::spawn(
        server.url(),
        token,
        &flags(&state_flag, &[]),
        launched,
        LoseWelcome,
    );
    agent.next_wait();
    agent.elapse();
    let ran = agent.finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("the server refused the hello with status 401"),
        "{}",
        ran.err
    );
    assert!(!state.join(CREDENTIAL_FILE).exists());
    assert_eq!(server.host("mac-1")["source"], "registered");
}
