//! The flags that turn registration on, the pool file behind them, and the flags and keys that
//! are accepted and no longer read.

use std::ffi::OsString;
use std::process::ExitCode;

use js_lease_server::{Cli, run};

use crate::agents_support::{agent_args, hello_as};
use crate::support::{
    Harness, LogCapture, Scratch, base_args, config, replace, without_agent_pool,
};

/// The flag a refusal names, with a reason that is not empty.
fn flag_of(args: Vec<OsString>) -> (&'static str, String) {
    let err = config(args).err().expect("the start should be refused");
    assert!(!err.reason().is_empty());
    (err.flag(), err.reason().to_string())
}

/// `args` with `flag` and its `value` added.
fn with_flag(mut args: Vec<OsString>, flag: &str, value: impl Into<OsString>) -> Vec<OsString> {
    args.push(flag.into());
    args.push(value.into());
    args
}

/// The refusal of the removed `--pool-config`, as the program prints it: the flag, then the
/// migration.
const POOL_CONFIG_REFUSED: &str = "--pool-config: the agentless hosts-file pool was removed: run js-agent on each listed host and use --agent-pool-config, or a [[pool]] of kind = \"joined\" (with its shape and max_machines)";

/// Asserts that a server with no agent registration is refused, naming
/// `--agent-pool-config` with the exact reason, and that the program ends with status 2 and
/// says so on one line; and that the base flags (which name the pool file) start. It catches
/// a server that starts with no host to serve, and a refusal that names no flag.
#[test]
fn a_start_without_agent_registration_is_refused_naming_the_pool_flag() {
    let scratch = Scratch::new("agents_neither");
    let refused = config(without_agent_pool(&scratch)).err().unwrap();
    assert_eq!(
        refused.to_string(),
        "--agent-pool-config: required: a server serves the hosts that run js-agent and register"
    );

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(without_agent_pool(&scratch), &mut out, &mut err);
    assert!(code == ExitCode::from(2));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "js-lease-server: --agent-pool-config: required: a server serves the hosts that run js-agent and register\n"
    );
    assert!(out.is_empty());

    assert!(config(base_args(&scratch)).is_ok());
    assert!(config(agent_args(&scratch)).is_ok());
}

/// Asserts that the removed `--pool-config` is refused naming the flag, with the migration
/// to `--agent-pool-config` (or a joined pool) as the whole message, before any other check:
/// beside a valid start, beside a start with no agent pool (so the refusal is not the one of
/// the missing pool), for a path that does not exist and for a directory, and through the
/// program, which ends with status 2, prints the message on one line and nothing on standard
/// output. The path is never repeated. It catches the flag accepted and ignored (the
/// operator would think the listed hosts are served), a refusal behind another one, a
/// refusal that names no flag or no replacement, and one that echoes the path.
#[test]
fn the_pool_config_flag_is_refused_naming_it_with_the_replacement() {
    let scratch = Scratch::new("agents_pool_config");
    let hosts = scratch.write("hosts.toml", "[[host]]\nid = \"gpu-01\"\n", 0o644);
    let missing = scratch.path("no-such-hosts.toml");
    let directory = scratch.path("");
    for path in [&hosts, &missing, &directory] {
        for base in [base_args(&scratch), without_agent_pool(&scratch)] {
            let refused = config(with_flag(base, "--pool-config", path))
                .err()
                .unwrap();
            assert_eq!(refused.flag(), "--pool-config");
            assert_eq!(refused.to_string(), POOL_CONFIG_REFUSED);
            assert!(!refused.to_string().contains("hosts.toml"));
        }
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = with_flag(base_args(&scratch), "--pool-config", path);
        let code = run(args, &mut out, &mut err);
        assert!(code == ExitCode::from(2));
        let said = String::from_utf8(err).unwrap();
        assert_eq!(said, format!("js-lease-server: {POOL_CONFIG_REFUSED}\n"));
        assert!(out.is_empty());
    }
    let beside = vec![
        OsString::from("js-lease-server"),
        "--config".into(),
        scratch.path("js.toml").into_os_string(),
        "--pool-config".into(),
        hosts.into_os_string(),
    ];
    let refused = config(beside).err().unwrap();
    assert_eq!(refused.to_string(), POOL_CONFIG_REFUSED);
}

/// Asserts that the pool file alone is a valid start, and that the hello limit flags are
/// accepted only beside the pool file and only above zero. It catches a hello limit that is
/// accepted with registration off.
#[test]
fn agent_flags_come_together() {
    let scratch = Scratch::new("agents_together");
    let full = agent_args(&scratch);
    assert!(config(base_args(&scratch)).is_ok());

    let burst = with_flag(without_agent_pool(&scratch), "--agent-hello-burst", "5");
    assert_eq!(
        config(burst).err().unwrap().to_string(),
        "--agent-hello-burst: requires --agent-pool-config"
    );
    let interval = with_flag(without_agent_pool(&scratch), "--agent-hello-interval", "5s");
    assert_eq!(
        config(interval).err().unwrap().to_string(),
        "--agent-hello-interval: requires --agent-pool-config"
    );
    let zero = with_flag(full.clone(), "--agent-hello-burst", "0");
    assert!(Cli::try_parse_from(zero).is_err());
    let limited = with_flag(full, "--agent-hello-burst", "5");
    assert!(Cli::try_parse_from(limited).is_ok());
}

/// Asserts that a pool file that is missing, is not TOML, names a setting that does not
/// exist, or breaks a rule of the wire (here: a grace not above two intervals) is refused
/// naming `--agent-pool-config`, with the path and the cause. It catches a typo that
/// starts a pool with defaults.
#[test]
fn agent_pool_file_errors_name_the_flag() {
    let scratch = Scratch::new("agents_pool_file");
    let args = agent_args(&scratch);
    let put = |name: &str, text: &str| {
        let path = scratch.write(name, text, 0o644);
        replace(args.clone(), "--agent-pool-config", path)
    };
    let typo = crate::agents_support::POOL.replace("max_machines", "max_machine");
    let grace =
        crate::agents_support::POOL.replace("max_machines = 2", "grace = \"5s\"\nmax_machines = 2");
    for (name, text) in [
        ("garbage.toml", "this = is = not toml".to_string()),
        ("typo.toml", typo),
        ("grace.toml", grace),
    ] {
        let (flag, reason) = flag_of(put(name, &text));
        assert_eq!(flag, "--agent-pool-config", "{name}");
        assert!(reason.contains(name), "{reason}");
    }
    let missing = scratch.path("absent.toml");
    let (flag, reason) = flag_of(replace(args, "--agent-pool-config", missing));
    assert_eq!(flag, "--agent-pool-config");
    assert!(reason.contains("absent.toml"), "{reason}");
}

/// The line a retired flag or key leaves at start.
fn ignored(item: &str) -> String {
    format!("{item} is ignored: agents are admitted by host certificate only (drop it)")
}

/// Starts a server on `args` and says hello as `host`; returns what it logged and the status
/// of the hello.
async fn started_and_welcomed(scratch: Scratch, args: Vec<OsString>, host: &str) -> (String, u16) {
    let logs = LogCapture::start();
    let h = Harness::with(scratch, args).await;
    let welcome = hello_as(&h, host).await;
    (logs.text(), welcome.status)
}

/// Asserts that `--agent-token-file` naming a path that does not exist still starts the
/// server, that the server warns once, naming the flag and saying that agents are admitted by
/// host certificate only, and that a host with a certificate is welcomed. It catches a flag
/// that is read (a missing file would refuse the start), one that is dropped without a line,
/// and a line given more than once.
#[tokio::test]
async fn an_agent_token_file_that_does_not_exist_is_ignored_with_one_warning() {
    let scratch = Scratch::new("agents_ignored_token_file");
    let missing = scratch.path("no-such-agent-tokens");
    let mut args = agent_args(&scratch);
    args.extend(["--agent-token-file".into(), missing.into_os_string()]);
    let cli = config(args.clone()).unwrap();
    assert_eq!(cli.warnings(), [ignored("--agent-token-file")]);
    let (logged, status) = started_and_welcomed(scratch, args, "burst-1").await;
    assert_eq!(status, 200);
    assert_eq!(
        logged.matches(&ignored("--agent-token-file")).count(),
        1,
        "{logged}"
    );
}

/// Asserts that `--require-host-credential` still starts the server, that the server warns
/// once, naming the flag, and that a host with a certificate is welcomed. It catches a flag
/// that is refused and one that makes the server demand anything more of a host.
#[tokio::test]
async fn require_host_credential_is_ignored_with_one_warning() {
    let scratch = Scratch::new("agents_ignored_require_credential");
    let mut args = agent_args(&scratch);
    args.push("--require-host-credential".into());
    let parsed = config(args.clone()).unwrap();
    assert_eq!(parsed.warnings(), [ignored("--require-host-credential")]);
    let (logged, status) = started_and_welcomed(scratch, args, "burst-1").await;
    assert_eq!(status, 200);
    let line = ignored("--require-host-credential");
    assert_eq!(logged.matches(&line).count(), 1, "{logged}");
}

/// Asserts that a pool file naming `join_token_file` at a path that does not exist still
/// starts the server, that the server warns once, naming the key and the file that holds it,
/// and that a host with a certificate is welcomed. It catches a key that is read and one that
/// is dropped without a line.
#[tokio::test]
async fn a_pool_file_join_token_file_that_does_not_exist_is_ignored_with_one_warning() {
    let scratch = Scratch::new("agents_ignored_pool_token_file");
    let args = agent_args(&scratch);
    let missing = scratch.path("no-such-pool-tokens");
    let pool = format!(
        "join_token_file = {:?}\n{}",
        missing.display().to_string(),
        crate::agents_support::POOL
    );
    let file = scratch.write("agent-pool.toml", &pool, 0o644);
    let item = format!("{}: join_token_file", file.display());
    let parsed = config(args.clone()).unwrap();
    assert_eq!(parsed.warnings(), [ignored(&item)]);
    let (logged, status) = started_and_welcomed(scratch, args, "burst-1").await;
    assert_eq!(status, 200);
    assert_eq!(logged.matches(&ignored(&item)).count(), 1, "{logged}");
}
