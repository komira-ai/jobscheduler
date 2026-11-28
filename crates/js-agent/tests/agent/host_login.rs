//! How the agent proves who it is: by its host certificate and key alone. An agent given them
//! connects with them, an agent without them does not start, and the flags of the earlier
//! logins (`--token-file`, `--instance-file`) are accepted, said to be ignored, and never read.

use std::cell::RefCell;
use std::path::Path;

use js_agent::{Connector, HEARTBEAT_PATH, HELLO_PATH, Login};
use js_client::{ClientError, HostIdentity};

use crate::jobs::private;
use crate::support::{
    CountingSleeper, Lowest, POOL, Ran, Reply, Scratch, Script, ScriptConnector, Sent, code,
    mac_host, run, welcome,
};

/// The message of the refusal to start without a host certificate and key.
const REQUIRED: &str = "js-agent: --host-cert and --host-key: required (the host's certificate \
     and key, which hostkeeper keeps); a host is identified by its certificate alone\n";

/// The flags of a run with no way to identify the host: a loopback server, a pool, a host id
/// and a state root.
fn bare(state: &str) -> Vec<String> {
    let flags = [
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        POOL,
        "--host-id",
        "h1",
        "--state-root",
        state,
    ];
    flags.map(str::to_owned).to_vec()
}

/// A connector that records the login of each connect.
struct Recording<'a> {
    script: ScriptConnector,
    logins: &'a RefCell<Vec<Login>>,
}

impl Connector for Recording<'_> {
    fn connect(
        &self,
        server: &str,
        login: &Login,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn js_agent::Transport>, ClientError> {
        self.logins.borrow_mut().push(login.clone());
        self.script.connect(server, login, ca_cert)
    }
}

/// Runs the program with `args` over a script of `replies`, stopping at its first wait;
/// returns the run, what was sent and the logins the connector was given.
fn run_recording(args: &[String], replies: Vec<Reply>) -> (Ran, Sent, Vec<Login>) {
    let (script, sent) = Script::new(replies);
    let logins = RefCell::new(Vec::new());
    let connector = Recording {
        script: ScriptConnector::of(script),
        logins: &logins,
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run(&args, &mac_host(), &connector, &mut sleeper, &mut Lowest);
    (ran, sent, logins.into_inner())
}

/// The paths of what was sent.
fn paths(sent: &Sent) -> Vec<String> {
    sent.borrow().iter().map(|(path, _)| path.clone()).collect()
}

/// Adds `--host-cert` and `--host-key` for two files of `dir` to `args`.
fn with_host_files(args: &[String], dir: &Path) -> (Vec<String>, HostIdentity) {
    let (cert, key) = (dir.join("host.pem"), dir.join("host.key"));
    let mut named = args.to_vec();
    for (flag, path) in [("--host-cert", &cert), ("--host-key", &key)] {
        named.extend([flag.to_owned(), path.display().to_string()]);
    }
    (named, HostIdentity::new(cert, key))
}

/// A private scratch directory and a private `state` directory in it.
fn state_dir(name: &str) -> (Scratch, std::path::PathBuf) {
    let scratch = Scratch::new(name);
    let dir = private(&scratch, "state");
    (scratch, dir)
}

/// Asserts that an agent given `--host-cert` and `--host-key` connects with them (the
/// connector is told so), registers, and sends a hello that names no host credential. It
/// catches an agent that connects with another login, and a hello that still asks for a
/// credential.
#[test]
fn an_agent_given_its_host_certificate_and_key_connects_with_them() {
    let (_scratch, dir) = state_dir("host-login-cert");
    let (args, identity) = with_host_files(&bare(&dir.display().to_string()), &dir);

    let (ran, sent, logins) = run_recording(&args, vec![welcome(1)]);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(logins, [Login::HostCertificate(identity)]);
    assert_eq!(paths(&sent), [HELLO_PATH, HEARTBEAT_PATH]);
    assert!(sent.borrow()[0].1.get("accepts_host_credential").is_none());
}

/// Asserts that an agent given neither `--host-cert` nor `--host-key` ends with the usage
/// status and the one message that names both flags, before it connects or sends anything,
/// also when a token file or a state root is given beside: they identify no host. It catches
/// an agent that starts with no identity, and a refusal that names one flag only.
#[test]
fn an_agent_without_host_cert_and_key_refuses_to_start_naming_both() {
    let (scratch, dir) = state_dir("host-login-none");
    let args = bare(&dir.display().to_string());
    let token = scratch.write("token", "tok-1\n", 0o600);
    let mut beside = args.clone();
    beside.extend(["--token-file".to_owned(), token.display().to_string()]);
    for given in [args, beside] {
        let (ran, sent, logins) = run_recording(&given, vec![welcome(1)]);
        assert_eq!(ran.status(), code(2), "{}", ran.err);
        assert!(ran.err.ends_with(REQUIRED), "{}", ran.err);
        assert!(sent.borrow().is_empty(), "{:?}", sent);
        assert!(logins.is_empty(), "{logins:?}");
    }
}

/// Asserts that one of the two host flags without the other stops the agent with the usage
/// status before it connects or says anything, naming the flag that is missing, in both
/// directions.
#[test]
fn exactly_one_host_flag_stops_the_agent_naming_the_other() {
    for (name, given, missing) in [
        ("host-login-no-key", "--host-cert", "--host-key"),
        ("host-login-no-cert", "--host-key", "--host-cert"),
    ] {
        let (_scratch, dir) = state_dir(name);
        let mut args = bare(&dir.display().to_string());
        args.extend([
            given.to_owned(),
            dir.join("host.file").display().to_string(),
        ]);
        let (ran, sent, logins) = run_recording(&args, vec![welcome(1)]);
        assert_eq!(ran.status(), code(2), "{name}: {}", ran.err);
        assert!(
            ran.err.contains(&format!(
                "the following required arguments were not provided:\n  {missing} <PATH>"
            )),
            "{name}: {}",
            ran.err
        );
        assert!(sent.borrow().is_empty(), "{name}: {:?}", sent);
        assert!(logins.is_empty(), "{name}: {logins:?}");
    }
}

/// Asserts that `--token-file` and `--instance-file` are accepted and ignored: given paths
/// that are not there, the agent starts, says once for each flag, on standard error, that it
/// is ignored because agents are admitted by host certificate only, connects with its host
/// certificate, and its hello names no instance. A run that gives neither says nothing of
/// them. It catches a flag that is read (the missing file would stop the start), one that is
/// said twice or not at all, and one that is refused.
#[test]
fn a_token_file_and_an_instance_file_are_accepted_ignored_and_warned_of_once_each() {
    let (_scratch, dir) = state_dir("host-login-ignored");
    let (mut args, identity) = with_host_files(&bare(&dir.display().to_string()), &dir);
    let (ran, _, _) = run_recording(&args, vec![welcome(1)]);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran.err.contains("accepted and ignored"), "{}", ran.err);

    args.extend([
        "--token-file".to_owned(),
        "/no/such/token".to_owned(),
        "--instance-file".to_owned(),
        "/no/such/instance".to_owned(),
    ]);
    let (ran, sent, logins) = run_recording(&args, vec![welcome(1)]);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(logins, [Login::HostCertificate(identity)]);
    let ignored = |flag: &str| {
        format!(
            "js-agent: warning: {flag} is accepted and ignored: agents are admitted by host \
             certificate only (--host-cert and --host-key)\n"
        )
    };
    for flag in ["--token-file", "--instance-file"] {
        assert_eq!(ran.err.matches(&ignored(flag)).count(), 1, "{}", ran.err);
    }
    assert_eq!(
        ran.err.matches("accepted and ignored").count(),
        2,
        "{}",
        ran.err
    );
    assert!(sent.borrow()[0].1.get("instance").is_none());
}
