//! The command line: the flags that go to the setup script, the defaults, and what clap
//! refuses.

use std::ffi::OsString;

use super::rig::{INSTANCE, PROVIDER, Rig, TOKEN, status};

/// Asserts that every setup flag is passed to the script, in the script's own spelling, after
/// `--apply`, with `--pool` defaulting to the provider when `--server` is given, and the
/// scratch-quota switch as a bare flag. It catches a flag that is dropped (so the unit would
/// be written with a placeholder) and a pool that is not the provider.
#[test]
fn the_setup_flags_reach_the_script() {
    let rig = Rig::new("enroll_flags_all");
    let ran = rig.go(
        &[
            "--apply",
            "--runtime",
            "crun",
            "--server",
            "https://leases.example:7300",
            "--job-user",
            "jsjob2",
            "--subid-start",
            "300000",
            "--unit",
            "/srv/js-agent-crun.service",
            "--unit-sha256",
            "aa",
            "--tmpfiles",
            "/srv/tmpfiles-crun.conf",
            "--tmpfiles-sha256",
            "bb",
            "--scratch-mount-unit",
            "scratch.mount",
            "--dependents-limit",
            "50",
            "--skip-scratch-quota",
        ],
        TOKEN,
    );
    assert_eq!(ran.status, status(0), "{ran:?}");
    let want = [
        "--apply",
        "--job-user",
        "jsjob2",
        "--subid-start",
        "300000",
        "--unit",
        "/srv/js-agent-crun.service",
        "--unit-sha256",
        "aa",
        "--tmpfiles",
        "/srv/tmpfiles-crun.conf",
        "--tmpfiles-sha256",
        "bb",
        "--server",
        "https://leases.example:7300",
        "--pool",
        PROVIDER,
        "--scratch-mount-unit",
        "scratch.mount",
        "--dependents-limit",
        "50",
        "--skip-scratch-quota",
    ];
    assert_eq!(rig.setup_args().unwrap(), want);
}

/// Asserts that `--pool` overrides the provider as the pool written into the unit, that a
/// `--pool` without `--server` is not passed (the script wants both or neither), and that a
/// dry run passes the flags without `--apply`. It catches a pool taken from the wrong place.
#[test]
fn the_pool_defaults_to_the_provider_and_can_be_named() {
    let rig = Rig::new("enroll_flags_pool");
    let ran = rig.go(
        &["--server", "http://127.0.0.1:7300", "--pool", "farm"],
        TOKEN,
    );
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        rig.setup_args().unwrap(),
        ["--server", "http://127.0.0.1:7300", "--pool", "farm"]
    );
    let ran = rig.go(&["--pool", "farm"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(rig.setup_args().unwrap(), Vec::<String>::new());
}

/// Asserts that the help is printed on standard output with status 0, lists the flags and
/// the exit statuses, and runs nothing; and that a runtime other than `crun`, a missing
/// `--provider` or `--instance`, an unknown flag and a token on the command line are all
/// usage errors (status 2) with nothing run. It catches a flag that takes a token.
#[test]
fn the_command_line_is_checked_by_clap() {
    let rig = Rig::new("enroll_clap");
    let ran = rig.run(&["--help"], "");
    assert_eq!(ran.status, status(0), "{ran:?}");
    for word in [
        "--provider",
        "--instance",
        "--apply",
        "--runtime",
        "--server",
        "standard input",
    ] {
        assert!(ran.out.contains(word), "{word}: {}", ran.out);
    }
    let refused: [&[&str]; 5] = [
        &[
            "--provider",
            PROVIDER,
            "--instance",
            INSTANCE,
            "--runtime",
            "docker",
        ],
        &["--instance", INSTANCE],
        &["--provider", PROVIDER],
        &[
            "--provider",
            PROVIDER,
            "--instance",
            INSTANCE,
            "--frobnicate",
        ],
        &[
            "--provider",
            PROVIDER,
            "--instance",
            INSTANCE,
            "--token",
            TOKEN,
        ],
    ];
    for words in refused {
        let ran = rig.run(words, TOKEN);
        assert_eq!(ran.status, status(2), "{words:?}: {ran:?}");
        assert_eq!(ran.out, "");
    }
    assert_eq!(rig.setup_args(), None);
}

/// Asserts that the program answers the verb before it parses the agent's flags, and that
/// on this machine (not root) it refuses with status 2 and changes nothing. It catches a
/// verb that is not wired into `main_with`, which would be read as the agent's own flags.
#[test]
fn main_with_answers_the_verb() {
    if rustix::process::geteuid().as_raw() == 0 {
        return;
    }
    let install = || -> std::io::Result<js_agent::SystemSleeper> {
        panic!("a verb installs no signal handler")
    };
    let args: Vec<OsString> = [
        "js-agent",
        "enroll",
        "--provider",
        PROVIDER,
        "--instance",
        INSTANCE,
    ]
    .map(OsString::from)
    .to_vec();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_agent::main_with(&install, args, &mut out, &mut err);
    assert_eq!(format!("{code:?}"), status(2));
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("js-agent enroll: must run as root"), "{err}");
}
