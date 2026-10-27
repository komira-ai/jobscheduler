//! Everything that makes the server refuse to start, and the flags' parsing.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use clap::Parser;
use js_lease_server::{SharedClock, channel_ticker, parse_duration, run, start};

use crate::support::{Harness, Scratch, TOKEN, args_with, base_args, config, replace, request};

/// The flag a refusal names, and that its reason is not empty.
fn flag_of<T>(result: Result<T, js_lease_server::StartupError>) -> &'static str {
    let err = result.err().expect("the start should be refused");
    assert!(!err.reason().is_empty());
    assert!(err.to_string().starts_with(err.flag()));
    err.flag()
}

/// Asserts that a token file is read only when it is a regular file only its owner can
/// use, and that a refusal names `--token-file`.
#[test]
fn token_file_must_be_owner_only() {
    let scratch = Scratch::new("token_perms");
    let args = base_args(&scratch);
    for mode in [0o600, 0o400] {
        fs::set_permissions(scratch.path("token"), fs::Permissions::from_mode(mode)).unwrap();
        assert!(config(args.clone()).is_ok(), "mode {mode:o}");
    }
    for mode in [0o644, 0o640, 0o660, 0o604, 0o700, 0o777, 0o000] {
        fs::set_permissions(scratch.path("token"), fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            flag_of(config(args.clone())),
            "--token-file",
            "mode {mode:o}"
        );
    }
    let loose = scratch.write("loose", TOKEN, 0o644);
    let err = config(replace(args.clone(), "--token-file", loose))
        .err()
        .unwrap();
    assert!(err.reason().contains("0600"), "{err}");

    let missing = scratch.path("absent");
    assert_eq!(
        flag_of(config(replace(args.clone(), "--token-file", missing))),
        "--token-file"
    );
    let dir = scratch.path("dir");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        flag_of(config(replace(args.clone(), "--token-file", dir))),
        "--token-file"
    );
    let empty = scratch.write("empty", " \n", 0o600);
    assert_eq!(
        flag_of(config(replace(args, "--token-file", empty))),
        "--token-file"
    );
}

/// Asserts that a token file's trailing newline is not part of the token.
#[tokio::test]
async fn token_file_trailing_newline_is_trimmed() {
    let scratch = Scratch::new("token_newline");
    let args = base_args(&scratch);
    let with_newline = scratch.write("nl", &format!("{TOKEN}\n"), 0o600);
    let args = replace(args, "--token-file", with_newline);
    let h = Harness::with(scratch, args, None).await;
    let reply = request(h.addr(), "GET", "/v1/leases", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200);
}

/// Asserts that a listen address off the loopback needs both TLS flags, a loopback one
/// does not, and one TLS flag alone is refused naming the other.
#[test]
fn tls_is_required_off_the_loopback() {
    let scratch = Scratch::new("tls_required");
    let args = base_args(&scratch);
    for listen in ["0.0.0.0:7300", "192.0.2.1:7300", "[::]:7300"] {
        let args = replace(args.clone(), "--listen", listen);
        let err = config(args).err().unwrap();
        assert_eq!(err.flag(), "--listen", "{listen}");
        assert!(err.reason().contains("--tls-cert") && err.reason().contains("--tls-key"));
    }
    for listen in ["127.0.0.1:7300", "[::1]:7300", "127.9.9.9:1"] {
        assert!(
            config(replace(args.clone(), "--listen", listen)).is_ok(),
            "{listen}"
        );
    }
    let only_cert = args_with(&scratch, &["--tls-cert", "c.pem"]);
    assert_eq!(flag_of(config(only_cert)), "--tls-key");
    let only_key = args_with(&scratch, &["--tls-key", "k.pem"]);
    assert_eq!(flag_of(config(only_key)), "--tls-cert");
    let both = args_with(&scratch, &["--tls-cert", "c.pem", "--tls-key", "k.pem"]);
    assert!(config(replace(both, "--listen", "0.0.0.0:7300")).is_ok());
}

/// Asserts that the ttl flags take seconds with an optional unit, refuse what is not a
/// positive duration, and that a default ttl above the maximum names `--default-ttl`.
#[test]
fn durations_parse_and_settings_are_checked() {
    let cases = [("90", 90), ("90s", 90), ("15m", 900), ("2h", 7200)];
    for (text, seconds) in cases {
        assert_eq!(parse_duration(text).unwrap().as_secs(), seconds, "{text}");
    }
    let refused = [
        "",
        "0",
        "0s",
        "s",
        "-5",
        "1.5s",
        "5d",
        "5 s",
        "99999999999999999999",
        "18446744073709551615h",
    ];
    for text in refused {
        assert!(parse_duration(text).is_err(), "{text:?}");
    }

    let scratch = Scratch::new("settings");
    let over = args_with(&scratch, &["--default-ttl", "2h", "--max-ttl", "1h"]);
    assert_eq!(flag_of(config(over)), "--default-ttl");
    let ok = args_with(
        &scratch,
        &["--default-ttl", "1m", "--max-ttl", "1h", "--tick", "250s"],
    );
    assert_eq!(config(ok).unwrap().tick_period().as_secs(), 250);
}

/// Asserts that `--boot` has no default and only the two modes, through the program's
/// own entry point: a usage error exits 2 and names the flag, help exits 0.
#[test]
fn run_reports_usage_and_help() {
    let scratch = Scratch::new("usage");
    let mut args = base_args(&scratch);
    let at = args.iter().position(|a| a == "--boot").unwrap();
    args.drain(at..at + 2);
    let (code, out, err) = run_captured(args);
    assert!(code == ExitCode::from(2));
    assert!(err.contains("--boot"), "{err}");
    assert!(out.is_empty());

    let bad = replace(base_args(&scratch), "--boot", "maybe");
    let (code, _, err) = run_captured(bad);
    assert!(code == ExitCode::from(2));
    assert!(
        err.contains("initialise") && err.contains("resume"),
        "{err}"
    );

    let (code, out, err) = run_captured(vec!["js-lease-server".into(), "--help".into()]);
    assert!(code == ExitCode::SUCCESS);
    assert!(out.contains("--token-file") && err.is_empty());
}

/// Asserts that a refusal found after the flags parse is printed with its flag and ends
/// the run with status 2: a loose token file, a non-loopback listen without TLS.
#[test]
fn run_reports_refusals_with_status_2() {
    let scratch = Scratch::new("run_refusals");
    let loose = scratch.write("loose", TOKEN, 0o644);
    let args = replace(base_args(&scratch), "--token-file", loose);
    let (code, out, err) = run_captured(args);
    assert!(code == ExitCode::from(2));
    assert!(out.is_empty());
    assert!(err.starts_with("js-lease-server: --token-file:"), "{err}");

    let args = replace(base_args(&scratch), "--listen", "0.0.0.0:7300");
    let (code, _, err) = run_captured(args);
    assert!(code == ExitCode::from(2));
    assert!(err.contains("--listen"), "{err}");
}

fn run_captured(args: Vec<OsString>) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts that a server whose first boot is not asked for refuses a missing lease file
/// naming `--boot`, and that a first boot creates the file.
#[tokio::test]
async fn resume_needs_the_state_file() {
    let scratch = Scratch::new("resume_missing");
    let args = replace(base_args(&scratch), "--boot", "resume");
    let (_, ticker) = channel_ticker();
    let started = start(config(args).unwrap(), SharedClock::new(clock()), ticker).await;
    assert_eq!(flag_of(started), "--boot");
    assert!(!scratch.state().exists());

    let args = base_args(&scratch);
    let (_, ticker) = channel_ticker();
    let started = start(config(args).unwrap(), SharedClock::new(clock()), ticker).await;
    assert!(started.is_ok());
    assert!(scratch.state().exists());
}

fn clock() -> js_lease::ManualClock {
    js_lease::ManualClock::default()
}

/// Asserts that a second server over one lease file, a lease file that is not a lease
/// file, a pool file that is missing or wrong, and a listen address already in use are
/// each refused naming their flag.
#[tokio::test]
async fn other_start_refusals_name_their_flag() {
    let first = Harness::new("refusals").await;
    let args = base_args(&first.scratch);
    let (_, ticker) = channel_ticker();
    let second = start(
        config(args.clone()).unwrap(),
        SharedClock::new(clock()),
        ticker,
    )
    .await;
    assert_eq!(flag_of(second), "--state-file");

    let scratch = Scratch::new("refusals_files");
    let garbage = scratch.write("garbage.toml", "this is not a lease file", 0o600);
    let args = replace(base_args(&scratch), "--state-file", garbage);
    let (_, ticker) = channel_ticker();
    let started = start(config(args).unwrap(), SharedClock::new(clock()), ticker).await;
    assert_eq!(flag_of(started), "--state-file");

    for (name, text) in [("typo.toml", "[[host]]\nidd = 1\n"), ("missing.toml", "")] {
        let pool = scratch.write(name, text, 0o644);
        if name == "missing.toml" {
            fs::remove_file(&pool).unwrap();
        }
        let args = replace(base_args(&scratch), "--pool-config", pool);
        let (_, ticker) = channel_ticker();
        let started = start(config(args).unwrap(), SharedClock::new(clock()), ticker).await;
        assert_eq!(flag_of(started), "--pool-config");
    }

    let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = busy.local_addr().unwrap().to_string();
    let args = replace(base_args(&scratch), "--listen", taken);
    let (_, ticker) = channel_ticker();
    let started = start(config(args).unwrap(), SharedClock::new(clock()), ticker).await;
    assert_eq!(flag_of(started), "--listen");
}

/// Asserts that a named pipe at the token path is refused at once, naming
/// `--token-file`, instead of the start waiting for a writer that never comes.
#[test]
fn a_named_pipe_as_token_file_is_refused() {
    use rustix::fs::{CWD, FileType, Mode, mknodat};
    let scratch = Scratch::new("token_fifo");
    let pipe = scratch.path("pipe");
    mknodat(CWD, &pipe, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
    let args = replace(base_args(&scratch), "--token-file", pipe);
    assert_eq!(flag_of(config(args)), "--token-file");
}

/// Asserts that a symbolic link to an owner-only token file is followed (secret mounts
/// are links), and that the checks are on the file it leads to: a link to a loose file
/// is refused.
#[test]
fn a_symlinked_token_file_is_checked_at_its_target() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new("token_symlink");
    let args = base_args(&scratch);
    let good = scratch.path("good-link");
    symlink(scratch.path("token"), &good).unwrap();
    assert!(config(replace(args.clone(), "--token-file", good)).is_ok());
    let loose = scratch.write("loose", TOKEN, 0o644);
    let bad = scratch.path("bad-link");
    symlink(loose, &bad).unwrap();
    assert_eq!(
        flag_of(config(replace(args, "--token-file", bad))),
        "--token-file"
    );
}

/// Asserts that a token that no header value could equal is refused at start with the
/// reason, rather than accepted and then answering 401 to everyone: a line break, a
/// space or a non-ASCII byte inside it.
#[test]
fn a_token_no_header_can_carry_is_refused() {
    let scratch = Scratch::new("token_unusable");
    let args = base_args(&scratch);
    for (name, text) in [
        ("lines", "first\nsecond\n"),
        ("inner-space", "two words"),
        ("leading-space", " token"),
        ("non-ascii", "caf\u{e9}"),
    ] {
        let path = scratch.write(name, text, 0o600);
        let err = config(replace(args.clone(), "--token-file", path))
            .err()
            .unwrap();
        assert_eq!(err.flag(), "--token-file", "{name}");
        assert!(err.reason().contains("printable ASCII"), "{name}: {err}");
    }
}

/// Asserts that the connection limits are flags, that a zero connection cap or a zero
/// duration is refused by the parser, and that the values given reach the server.
#[test]
fn connection_limits_are_flags() {
    let scratch = Scratch::new("limit_flags");
    for bad in [
        "--max-connections=0",
        "--max-connections=70000",
        "--handshake-timeout=0",
        "--header-timeout=0s",
        "--drain-timeout=soon",
    ] {
        let args = args_with(&scratch, &[bad]);
        assert!(js_lease_server::Cli::try_parse_from(args).is_err(), "{bad}");
    }
    let args = args_with(
        &scratch,
        &[
            "--max-connections=7",
            "--handshake-timeout=3",
            "--header-timeout=2m",
            "--drain-timeout=1h",
        ],
    );
    let cli = js_lease_server::Cli::try_parse_from(args).unwrap();
    assert_eq!(cli.max_connections, 7);
    assert_eq!(cli.handshake_timeout.as_secs(), 3);
    assert_eq!(cli.header_timeout.as_secs(), 120);
    assert_eq!(cli.drain_timeout.as_secs(), 3600);
}
