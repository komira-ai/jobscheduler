//! The flags that turn registration on, the token file and the pool file behind them,
//! and how a token is replaced while hosts are registered.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use clap::Parser;
use js_lease_server::{Cli, run};
use serde_json::json;

use crate::agents_support::{JOIN, JOIN_B, agent_args, beat, hello_with, join, post, session};
use crate::support::{Harness, LogCapture, Scratch, TOKEN, base_args, config, replace};

fn secs(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n)
}

/// The flag a refusal names, with a reason that is not empty.
fn flag_of(args: Vec<OsString>) -> (&'static str, String) {
    let err = config(args).err().expect("the start should be refused");
    assert!(!err.reason().is_empty());
    (err.flag(), err.reason().to_string())
}

/// `base_args` without its `--pool-config`.
fn without_pool_config(scratch: &Scratch) -> Vec<OsString> {
    let mut args = base_args(scratch);
    let at = args.iter().position(|arg| arg == "--pool-config").unwrap();
    args.drain(at..at + 2);
    args
}

/// `args` with `flag` and its `value` added.
fn with_flag(mut args: Vec<OsString>, flag: &str, value: impl Into<OsString>) -> Vec<OsString> {
    args.push(flag.into());
    args.push(value.into());
    args
}

/// Asserts that a server with neither a static pool nor agent registration is refused,
/// naming `--pool-config`, and that the program ends with status 2 and says so; and that
/// the static pool alone and agent registration alone both start. It catches a server
/// that starts with nothing to lease, and a refusal that names no flag.
#[test]
fn neither_source_configured_refused() {
    let scratch = Scratch::new("agents_neither");
    let (flag, reason) = flag_of(without_pool_config(&scratch));
    assert_eq!(flag, "--pool-config");
    assert!(reason.contains("--agent-token-file"), "{reason}");

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(without_pool_config(&scratch), &mut out, &mut err);
    assert!(code == ExitCode::from(2));
    let err = String::from_utf8(err).unwrap();
    assert!(err.starts_with("js-lease-server: --pool-config:"), "{err}");

    assert!(config(base_args(&scratch)).is_ok());
    assert!(config(agent_args(&scratch, false)).is_ok());
    assert!(config(agent_args(&scratch, true)).is_ok());
}

/// Asserts that the two agent flags come together, each refusal naming the one that is
/// missing, and that the hello limit flags are accepted only beside the token file and
/// only above zero. It catches registration half turned on: tokens with no pool to join,
/// or a pool nobody can reach.
#[test]
fn agent_flags_come_together() {
    let scratch = Scratch::new("agents_together");
    let full = agent_args(&scratch, true);
    let value_of = |flag: &str| {
        let at = full.iter().position(|arg| arg == flag).unwrap();
        full[at + 1].clone()
    };
    let only_tokens = with_flag(
        base_args(&scratch),
        "--agent-token-file",
        value_of("--agent-token-file"),
    );
    assert_eq!(flag_of(only_tokens).0, "--agent-pool-config");
    let only_pool = with_flag(
        base_args(&scratch),
        "--agent-pool-config",
        value_of("--agent-pool-config"),
    );
    assert_eq!(flag_of(only_pool).0, "--agent-token-file");

    let burst = with_flag(base_args(&scratch), "--agent-hello-burst", "5");
    assert!(Cli::try_parse_from(burst).is_err());
    let interval = with_flag(base_args(&scratch), "--agent-hello-interval", "5s");
    assert!(Cli::try_parse_from(interval).is_err());
    let zero = with_flag(full.clone(), "--agent-hello-burst", "0");
    assert!(Cli::try_parse_from(zero).is_err());
    let limited = with_flag(full, "--agent-hello-burst", "5");
    assert!(Cli::try_parse_from(limited).is_ok());
}

/// Asserts that the join token file is read only when it is a regular file only its owner
/// can use, naming `--agent-token-file` otherwise, with the same modes the client token
/// file accepts. It catches a join token readable by other users on the box.
#[test]
fn token_file_mode_0644_refused_naming_the_flag() {
    let scratch = Scratch::new("agents_token_modes");
    let args = agent_args(&scratch, false);
    for mode in [0o600, 0o400] {
        fs::set_permissions(
            scratch.path("agent-tokens"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        assert!(config(args.clone()).is_ok(), "mode {mode:o}");
    }
    for mode in [0o644, 0o640, 0o660, 0o604, 0o700, 0o777, 0o000] {
        fs::set_permissions(
            scratch.path("agent-tokens"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        let (flag, reason) = flag_of(args.clone());
        assert_eq!(flag, "--agent-token-file", "mode {mode:o}");
        // A file nobody can read is refused when it is opened, before its mode is judged.
        let hinted = reason.contains("0600") | (mode == 0);
        assert!(hinted, "{reason}");
    }
}

/// Asserts what the join token file may hold: one or several tokens of at least 32
/// bytes, blank lines and repeats allowed; it is refused, naming the flag, when it is
/// missing, a directory, empty, holds a short token (the reason names the line and does
/// not print the token), holds a token with a space in it, holds more than 16 tokens, or
/// holds the client token (the reason points at `--token-file` and does not print it).
#[test]
fn token_file_contents_are_checked() {
    let scratch = Scratch::new("agents_token_contents");
    let args = agent_args(&scratch, false);
    let put = |text: &str| {
        let path = scratch.write("tokens", text, 0o600);
        replace(args.clone(), "--agent-token-file", path)
    };
    assert!(config(put(&format!("{JOIN}\n\n{JOIN_B}\n{JOIN}\n"))).is_ok());
    assert!(config(put(&format!("{JOIN}\r\n"))).is_ok());

    for (text, wants) in [
        ("\n \n".to_string(), "holds no token"),
        (format!("{JOIN}\nshort\n"), "line 2"),
        (format!("{JOIN}\nhas a space in it\n"), "line 2"),
        (format!("{JOIN}\n{TOKEN}\n"), "--token-file"),
    ] {
        let (flag, reason) = flag_of(put(&text));
        assert_eq!(flag, "--agent-token-file");
        assert!(reason.contains(wants), "{wants}: {reason}");
        assert!(!reason.contains(TOKEN), "{reason}");
        assert!(!reason.contains("short"), "{reason}");
    }
    let many = (0..17).map(|n| format!("{JOIN}-{n}\n")).collect::<String>();
    let (flag, reason) = flag_of(put(&many));
    assert_eq!(flag, "--agent-token-file");
    assert!(reason.contains("more than 16"), "{reason}");

    let missing = scratch.path("absent");
    let (flag, _) = flag_of(replace(args.clone(), "--agent-token-file", missing));
    assert_eq!(flag, "--agent-token-file");
    let dir = scratch.path("dir");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o600)).unwrap();
    let (flag, _) = flag_of(replace(args, "--agent-token-file", dir));
    assert_eq!(flag, "--agent-token-file");
}

/// Asserts that a pool file that is missing, is not TOML, names a setting that does not
/// exist, or breaks a rule of the wire (here: a grace not above two intervals) is refused
/// naming `--agent-pool-config`, with the path and the cause. It catches a typo that
/// starts a pool with defaults.
#[test]
fn agent_pool_file_errors_name_the_flag() {
    let scratch = Scratch::new("agents_pool_file");
    let args = agent_args(&scratch, false);
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

/// Asserts the rotation as documented: a token added to the file is not accepted until a
/// tick reads it, then admits hosts beside the old one; a host stays bound to the token
/// that admitted it (the other token cannot heartbeat for it); a token removed from the
/// file is refused (401) at the next tick, which cuts off the hosts bound to it, and the
/// new token cannot take their ids over while they are bound. It catches a token file
/// read once, a second token that is ignored, and a removed token that stays valid.
#[tokio::test]
async fn token_rotation_second_token_admits() {
    let scratch = Scratch::new("agents_rotation");
    let args = agent_args(&scratch, false);
    scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let h = Harness::with(scratch, args, None).await;
    let old = join(&h, "burst-1").await;
    assert_eq!(hello_with(&h, JOIN_B, "burst-2").await.status, 401);

    h.scratch
        .write("agent-tokens", &format!("{JOIN}\n{JOIN_B}\n"), 0o600);
    assert_eq!(hello_with(&h, JOIN_B, "burst-2").await.status, 401);
    h.ticks.step().await;
    let new = session(&hello_with(&h, JOIN_B, "burst-2").await, "burst-2");
    assert_eq!(beat(&h, &old).await.status, 200);
    let both = post(&h, "/v1/agent/heartbeat", Some(JOIN_B), &new.beat()).await;
    assert_eq!(both.status, 200, "{}", both.body);
    let borrowed = post(&h, "/v1/agent/heartbeat", Some(JOIN_B), &old.beat()).await;
    assert_eq!(borrowed.status, 403);
    assert_eq!(borrowed.json(), json!({ "error": "host-bound" }));

    h.scratch
        .write("agent-tokens", &format!("{JOIN_B}\n"), 0o600);
    h.ticks.step().await;
    assert_eq!(beat(&h, &old).await.status, 401);
    assert_eq!(hello_with(&h, JOIN, "burst-1").await.status, 401);
    let kept = post(&h, "/v1/agent/heartbeat", Some(JOIN_B), &new.beat()).await;
    assert_eq!(kept.status, 200, "{}", kept.body);
    let live = hello_with(&h, JOIN_B, "burst-1").await;
    assert_eq!(
        (live.status, live.json()),
        (403, json!({ "error": "host-bound" }))
    );
    h.clock.advance(secs(16));
    let lost = hello_with(&h, JOIN_B, "burst-1").await;
    assert_eq!(
        (lost.status, lost.json()),
        (403, json!({ "error": "host-bound" }))
    );
    assert_eq!(h.running.agent_errors(), 0);
}

/// Asserts that a token file that cannot be read at a tick (emptied, made readable by
/// others, removed) leaves the tokens in force, is counted and logged without any token
/// in the log, and that a good file is taken again afterwards. It catches a half-saved
/// edit that locks every agent out, and a failure nobody can see.
#[tokio::test]
async fn a_token_file_that_cannot_be_read_keeps_the_tokens_in_force() {
    let logs = LogCapture::start();
    let scratch = Scratch::new("agents_reload_failure");
    let args = agent_args(&scratch, false);
    scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let h = Harness::with(scratch, args, None).await;
    let path = h.scratch.path("agent-tokens");
    let me = join(&h, "burst-1").await;

    fs::write(&path, "\n").unwrap();
    h.ticks.step().await;
    assert_eq!(h.running.agent_errors(), 1);
    assert_eq!(beat(&h, &me).await.status, 200);

    fs::write(&path, format!("{JOIN_B}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    h.ticks.step().await;
    assert_eq!(h.running.agent_errors(), 2);
    assert_eq!(beat(&h, &me).await.status, 200);
    assert_eq!(hello_with(&h, JOIN_B, "burst-2").await.status, 401);

    fs::remove_file(&path).unwrap();
    h.ticks.step().await;
    assert_eq!(h.running.agent_errors(), 3);
    assert_eq!(beat(&h, &me).await.status, 200);

    h.scratch
        .write("agent-tokens", &format!("{JOIN_B}\n"), 0o600);
    h.ticks.step().await;
    assert_eq!(h.running.agent_errors(), 3);
    assert_eq!(beat(&h, &me).await.status, 401);
    assert_eq!(hello_with(&h, JOIN_B, "burst-2").await.status, 200);

    let text = logs.text();
    assert!(
        text.contains("cannot read the agent token file again"),
        "{text}"
    );
    assert!(!text.contains(JOIN), "{text}");
    assert!(!text.contains(JOIN_B), "{text}");
}
