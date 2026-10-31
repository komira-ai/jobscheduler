//! The verb's flags, the credential it picks, the host it names, and the exit status and
//! words of each way the window can end, over a fake lease server and a clock moved by hand.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use js_agent::cordon::{Connect, Connection, Host as VerbHost, parse_window, run};
use js_agent::own_host::{Lease, LeaseError};
use js_client::ClientError;

use super::fakes::{Call, Effect, FakeLease, FakePace, Kind};
use crate::jobs::private;
use crate::support::{Host, Scratch, code, linux_host, mac_host};

/// What the verb asked the connector for.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Seen {
    server: String,
    credential: PathBuf,
    ca_cert: Option<PathBuf>,
    host: String,
}

/// What a run of the verb printed and how it ended.
pub(super) struct Ran {
    pub(super) status: String,
    pub(super) out: String,
    pub(super) err: String,
    pub(super) seen: Vec<Seen>,
}

/// Runs the verb on `flags` with `connect` as the connector.
pub(super) fn drive_with(
    flags: &[&str],
    host: &Host,
    connect: &Connect<'_>,
    pace: &mut FakePace<'_>,
) -> Ran {
    let seen = RefCell::new(Vec::new());
    let recording = |at: &Connection<'_>| {
        seen.borrow_mut().push(Seen {
            server: at.server.to_owned(),
            credential: at.credential.to_path_buf(),
            ca_cert: at.ca_cert.map(ToOwned::to_owned),
            host: at.host.to_owned(),
        });
        connect(at)
    };
    let mut verb = VerbHost {
        platform: host.platform.clone(),
        commands: &host.commands,
        files: &host.files,
        connect: &recording,
        pace,
    };
    let mut args: Vec<OsString> = vec!["js-agent".into(), "cordon".into()];
    args.extend(flags.iter().map(OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run(&args, &mut out, &mut err, &mut verb);
    drop(verb);
    Ran {
        status: format!("{status:?}"),
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        seen: seen.into_inner(),
    }
}

/// Runs the verb against `lease`.
fn drive(flags: &[&str], host: &Host, lease: &FakeLease, pace: &mut FakePace) -> Ran {
    let lease = lease.clone();
    drive_with(flags, host, &move |_| Ok(Box::new(lease.clone())), pace)
}

/// The flags every run needs besides the ones under test.
const BASE: [&str; 6] = [
    "--server",
    "http://127.0.0.1:7300",
    "--token-file",
    "/token",
    "--host-id",
    "h-1",
];

/// `BASE` and `more`.
fn with(more: &[&'static str]) -> Vec<&'static str> {
    BASE.iter().chain(more).copied().collect()
}

/// Asserts `--help`: it exits 0 with the text on standard output and nothing on standard
/// error, it names every line the verb prints and every exit status, and it says that a
/// process killed with SIGKILL leaves its cordon to lapse within `--lease-secs`. It catches a
/// help that is an error, one that is silent about the lapse, and one that stops listing a
/// line a script reads.
#[test]
fn help_lists_the_lines_the_statuses_and_the_lapse_of_a_killed_verb() {
    let ran = drive(
        &["--help"],
        &linux_host(),
        &FakeLease::new(),
        &mut FakePace::new(),
    );
    assert_eq!(ran.status, code(0));
    assert!(ran.err.is_empty(), "{}", ran.err);
    assert!(ran.seen.is_empty());
    for word in [
        "cordoned host=<id> expires_in_ms=<n> leases=<n>",
        "idle host=<id>",
        "lifted host=<id>",
        "lapsed host=<id>",
        "kept host=<id> reason=already_cordoned",
        "SIGKILL",
        "lapses by itself within --lease-secs",
        "3 another holder's cordon",
        "--for <DURATION>",
        "--lease-secs <N>",
    ] {
        assert!(ran.out.contains(word), "{word}: {}", ran.out);
    }
}

/// Asserts the usage errors: no server, no `--for`, a `--for` that is not a duration, a
/// `--lease-secs` of 0 and of one more than the server's cut, a flag the verb does not have,
/// no credential, a host id that cannot be one and a machine id that cannot be read each end
/// with status 2 and a message naming what is wrong, and ask the connector for nothing. It
/// catches a wrong flag that starts a window and one that reports a usage error as a server
/// error.
#[test]
fn a_wrong_flag_or_file_ends_with_status_2_and_names_the_flag() {
    let bare = linux_host();
    let unreadable = Host {
        files: Default::default(),
        ..linux_host()
    };
    let cases: Vec<(Vec<&str>, &Host, &str)> = vec![
        (vec![], &bare, "--server"),
        (with(&[]), &bare, "--for"),
        (with(&["--for", "2x"]), &bare, "\"2x\" is not a duration"),
        (
            with(&["--for", "1h", "--lease-secs", "0"]),
            &bare,
            "--lease-secs",
        ),
        (
            with(&["--for", "1h", "--lease-secs", "3601"]),
            &bare,
            "--lease-secs",
        ),
        (with(&["--for", "1h", "--nope"]), &bare, "--nope"),
        (
            vec![
                "--server",
                "http://127.0.0.1:1",
                "--for",
                "1h",
                "--host-id",
                "h-1",
            ],
            &bare,
            "--token-file: required unless --state-root holds a host credential",
        ),
        (
            vec![
                "--server",
                "http://127.0.0.1:1",
                "--token-file",
                "/t",
                "--for",
                "1h",
                "--host-id",
                "UPPER",
            ],
            &bare,
            "--host-id",
        ),
        (
            vec![
                "--server",
                "http://127.0.0.1:1",
                "--token-file",
                "/t",
                "--for",
                "1h",
            ],
            &unreadable,
            "pass --host-id",
        ),
    ];
    for (flags, host, word) in cases {
        let ran = drive(&flags, host, &FakeLease::new(), &mut FakePace::new());
        assert_eq!(ran.status, code(2), "{flags:?}: {}", ran.err);
        assert!(ran.err.starts_with("js-agent cordon: "), "{}", ran.err);
        assert!(ran.err.contains(word), "{flags:?}: {}", ran.err);
        assert!(ran.out.is_empty() && ran.seen.is_empty(), "{flags:?}");
    }
}

/// Asserts that a connector that refuses a flag (an unreadable `--ca-cert`, a token file of
/// the wrong mode) ends the verb with status 2 and the connector's own words. It catches a
/// connection error reported as a server error, which would send an operator to the server.
#[test]
fn a_flag_the_connector_refuses_ends_with_status_2() {
    let refuse = |_: &Connection<'_>| -> Result<Box<dyn Lease>, ClientError> {
        Err(ClientError::Flag {
            flag: "--ca-cert",
            why: "/ca.pem: no certificate found".to_owned(),
        })
    };
    let more = with(&["--for", "1h", "--ca-cert", "/ca.pem"]);
    let ran = drive_with(&more, &linux_host(), &refuse, &mut FakePace::new());
    assert_eq!(ran.status, code(2));
    assert_eq!(
        ran.err,
        "js-agent cordon: --ca-cert: /ca.pem: no certificate found\n"
    );
    assert_eq!(ran.seen[0].ca_cert, Some(PathBuf::from("/ca.pem")));
}

/// Writes a host credential of `text` in a state root and returns its directory.
fn state_with(files: &Scratch, name: &str, text: Option<&str>) -> PathBuf {
    let dir = private(files, name);
    if let Some(text) = text {
        files.write(&format!("{name}/host-credential"), text, 0o600);
    }
    dir
}

/// Asserts which file the bearer is read from: the host credential of `--state-root` when it
/// holds one, even beside a `--token-file` (the agent too sends the credential once it has
/// one, and the pool token is refused on these routes); the token file when the state root
/// holds none; and, when the credential file is there and is not a credential, a usage error
/// that names the file and does not print its content. It catches a verb that sends the token
/// of a host that has already swapped it, and one that echoes a file it could not read.
#[test]
fn the_host_credential_of_the_state_root_wins_over_the_token_file() {
    let files = Scratch::new("cordon_verb_credentials");
    let kept = state_with(&files, "kept", Some(&"c".repeat(64)));
    let empty = state_with(&files, "empty", None);
    let broken = state_with(&files, "broken", Some("not a credential!"));
    let (kept_flag, empty_flag, broken_flag) = (
        kept.display().to_string(),
        empty.display().to_string(),
        broken.display().to_string(),
    );
    let host = linux_host();
    let more = |root: &str| {
        [
            "--server",
            "https://leases.example:7300",
            "--token-file",
            "/token",
            "--state-root",
            root,
            "--host-id",
            "h-1",
            "--for",
            "1m",
        ]
        .map(str::to_owned)
        .to_vec()
    };
    let run_on = |root: &str| {
        let flags = more(root);
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        drive(&flags, &host, &FakeLease::new(), &mut FakePace::new())
    };

    let ran = run_on(&kept_flag);
    assert_eq!(ran.status, code(0), "{}", ran.err);
    assert_eq!(
        ran.seen,
        [Seen {
            server: "https://leases.example:7300".to_owned(),
            credential: kept.join("host-credential"),
            ca_cert: None,
            host: "h-1".to_owned(),
        }]
    );

    let ran = run_on(&empty_flag);
    assert_eq!(ran.status, code(0), "{}", ran.err);
    assert_eq!(ran.seen[0].credential, PathBuf::from("/token"));

    let ran = run_on(&broken_flag);
    assert_eq!(ran.status, code(2));
    assert!(ran.err.contains("host-credential"), "{}", ran.err);
    assert!(ran.err.contains("does not hold a host credential"));
    assert!(!ran.err.contains("not a credential!"), "{}", ran.err);
    assert!(ran.seen.is_empty());
}

/// Asserts the host the verb cordons: the one `--host-id` names, and without it the id the
/// agent registers under (`h-` and a salted hash of the machine id), on Linux from
/// `/etc/machine-id` and on a Mac from the platform UUID. It catches a default that differs
/// from the agent's, which would cordon a host the agent is not.
#[test]
fn the_default_host_is_the_one_the_agent_registers_under() {
    for host in [linux_host(), mac_host()] {
        let machine = js_agent::machine_id(&host.platform, &host.commands, &host.files).unwrap();
        let more = [
            "--server",
            "http://127.0.0.1:1",
            "--token-file",
            "/t",
            "--for",
            "1m",
        ];
        // The server answers for the host it is asked about, as the real one does.
        let lease = FakeLease::new();
        lease.set(|state| state.id = js_agent::host_id(&machine));
        let ran = drive(&more, &host, &lease, &mut FakePace::new());
        assert_eq!(ran.status, code(0), "{}", ran.err);
        assert_eq!(ran.seen[0].host, js_agent::host_id(&machine));
        assert_eq!(ran.seen[0].host.len(), 34);
    }
    let named = with(&["--for", "1m"]);
    let ran = drive(
        &named,
        &linux_host(),
        &FakeLease::new(),
        &mut FakePace::new(),
    );
    assert_eq!(ran.seen[0].host, "h-1");
}

/// Asserts the three ways the window ends, each as status, standard output and standard error:
/// held and lifted (0, the three lines, nothing on stderr), another holder's cordon (3, the
/// `kept` line, a message on stderr, the cordon left in place) and a server error (1, nothing
/// on stdout, the message on stderr). It catches an exit status that does not tell a script
/// which of them happened.
#[test]
fn the_statuses_tell_held_from_another_holders_cordon_from_an_error() {
    let more = with(&["--for", "10m"]);

    let lease = FakeLease::new();
    let ran = drive(&more, &linux_host(), &lease, &mut FakePace::new());
    assert_eq!(ran.status, code(0));
    let lines = [
        "cordoned host=h-1 expires_in_ms=300000 leases=0",
        "idle host=h-1",
        "lifted host=h-1",
    ];
    assert_eq!(ran.out, format!("{}\n", lines.join("\n")));
    assert!(ran.err.is_empty(), "{}", ran.err);

    let lease = FakeLease::new();
    lease.set(|state| state.cordoned = true);
    let ran = drive(&more, &linux_host(), &lease, &mut FakePace::new());
    assert_eq!(ran.status, code(3));
    assert_eq!(ran.out, "kept host=h-1 reason=already_cordoned\n");
    assert_eq!(
        ran.err,
        "js-agent cordon: host h-1 is cordoned by another holder; it is left in place\n"
    );
    assert!(lease.cordoned());

    let lease = FakeLease::new();
    lease.on(
        Kind::Cordon,
        0,
        Effect::Fail(LeaseError::Unreachable(
            "cannot talk to the server".to_owned(),
        )),
    );
    let ran = drive(&more, &linux_host(), &lease, &mut FakePace::new());
    assert_eq!(ran.status, code(1));
    assert!(ran.out.is_empty());
    assert_eq!(
        ran.err,
        "js-agent cordon: server error: cannot cordon the host: cannot talk to the server\n"
    );
}

/// Asserts `--for` and `--lease-secs`: each duration form is the number of seconds it says
/// (the window passes on the clock to the second), `--lease-secs` is what each cordon request
/// asks for, and the default is 300. It catches a unit read wrong and a flag that is parsed
/// and not used.
#[test]
fn for_and_lease_secs_set_the_window_and_the_request() {
    for (text, seconds) in [
        ("45s", 45),
        ("90m", 5400),
        ("2h", 7200),
        ("1h30m5s", 5405),
        ("1d", 86400),
    ] {
        let (lease, mut pace) = (FakeLease::new(), FakePace::new());
        let more = with(&["--for", text]);
        let ran = drive(&more, &linux_host(), &lease, &mut pace);
        assert_eq!(ran.status, code(0), "{text}: {}", ran.err);
        assert_eq!(pace.elapsed(), Duration::from_secs(seconds), "{text}");
        assert_eq!(lease.calls()[0], Call::Cordon(300));
    }
    let (lease, mut pace) = (FakeLease::new(), FakePace::new());
    let more = with(&["--for", "2h", "--lease-secs", "3600"]);
    drive(&more, &linux_host(), &lease, &mut pace);
    assert_eq!(lease.calls()[0], Call::Cordon(3600));
    assert_eq!(
        pace.waits[0],
        Duration::from_secs(1200),
        "a third of 3600 s"
    );
}

/// Asserts the duration syntax: sums of `<digits><unit>` terms with the units `s`, `m`, `h`
/// and `d`; and that the empty text, zero, a bare number, an unknown unit, a unit with no
/// number, a trailing number, a sign, a space, a fraction and numbers too large to count all
/// fail. It catches `--for 0`, which would end at once, and a bare `--for 3600` whose unit a
/// reader would have to guess.
#[test]
fn a_window_is_terms_of_a_number_and_a_unit() {
    for (text, seconds) in [("1s", 1), ("90m", 5400), ("1h30m", 5400), ("2d1s", 172_801)] {
        assert_eq!(
            parse_window(text),
            Ok(Duration::from_secs(seconds)),
            "{text}"
        );
    }
    for text in [
        "",
        "0s",
        "0h0m",
        "5",
        "2x",
        "h",
        "1h2",
        "-1h",
        " 1h",
        "1.5h",
        "99999999999999999999s",
        "18446744073709551615h",
        "18446744073709551615s1s",
    ] {
        let error = parse_window(text).unwrap_err();
        assert!(
            error.contains("is not a duration such as 90m"),
            "{text}: {error}"
        );
    }
}

/// Asserts that the program's entry point answers the verb before it installs the daemon's
/// signal handling, and that a verb which cannot install its own says so and ends with status
/// 1 instead of cordoning a host it cannot lift on a stop. It catches a dispatch that falls
/// through to the agent's flags, which would answer `--pool` is missing.
#[test]
fn the_entry_point_answers_the_verb_and_reports_signal_handlers_that_cannot_be_set() {
    let args: Vec<OsString> = ["js-agent", "cordon", "--server", "http://127.0.0.1:1"]
        .map(OsString::from)
        .to_vec();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let install = || -> std::io::Result<js_agent::SystemSleeper> {
        Err(std::io::Error::other("no signal handlers here"))
    };
    let status = js_agent::main_with(&install, args, &mut out, &mut err);
    assert_eq!(format!("{status:?}"), code(1));
    let err = String::from_utf8(err).unwrap();
    assert_eq!(
        err,
        "js-agent cordon: cannot install signal handlers: no signal handlers here\n"
    );
    assert!(out.is_empty());
}
