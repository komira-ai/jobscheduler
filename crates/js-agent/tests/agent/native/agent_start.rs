//! The whole macOS agent's start, read by what reaches its native runtime and by the
//! refusals that end it before the hello: the resource limit flags, a flag only a Linux build
//! has, a worker that cannot start, the Full Disk Access it judges from its own file reader,
//! and the self-test it runs before the hello. The agent's other flags are in `agent.rs`.

use std::sync::Arc;

use js_agent::native::grant::PRIVACY_DIR;
use js_caps::FULL_DISK_ACCESS;
use serde_json::Value;

use super::agent::{hello_on, hello_with, native_flags};
use super::{Failing, Rig};
use crate::jobs::fake::{Build, FakeMachine};
use crate::support::{
    CountingSleeper, Jobs, Lowest, Scratch, Script, ScriptConnector, code, flags, mac_host, run_on,
    run_on_defaulted, welcome,
};

/// Asserts the resource limit flags through the whole macOS agent, as its command line and its
/// start read them: beside a native agent's flags, no limit flag gives the native runtime the
/// defaults (4096 MiB a file, 512 processes), and `--native-max-file-mib 128` (the least)
/// with `--native-max-processes 64` gives it 128 MiB in bytes and 64 processes, each agent
/// saying hello. A size below 128, a 0, a size that does not fit in bytes and a process count
/// of `u32::MAX` end the agent with status 2 before the hello, by exact text; with
/// `--no-jobs` each flag is refused by name. Catches a Mac agent that drops a limit given on
/// its command line, checks it only in a struct no command line reaches, or starts with a
/// limit it refused.
#[test]
fn the_limit_flags_go_through_the_mac_agents_start() {
    let dir = Scratch::new("native-limits-agent");
    // The native flags make their directories once; each run of the agent reuses them.
    let base = native_flags(&dir);
    let run = |extra: &[&str]| {
        let mut given = base.clone();
        given.extend(extra.iter().map(|flag| (*flag).to_owned()));
        let refs: Vec<&str> = given.iter().map(String::as_str).collect();
        let args = flags(&refs);
        let (script, sent) = Script::new(vec![welcome(1)]);
        let connector = ScriptConnector::of(script);
        let mut sleeper = CountingSleeper::stopping_at(1);
        let machine = Arc::new(FakeMachine::new());
        let jobs: Jobs<'_> = (machine.clone(), None);
        let ran = run_on(
            &args,
            &mac_host(),
            &connector,
            &mut sleeper,
            &mut Lowest,
            jobs,
        );
        let said_hello = !sent.borrow().is_empty();
        (ran, said_hello, machine.limits_seen())
    };
    let started: [(&[&str], (u64, u32)); 2] = [
        (&[], (4096 << 20, 512)),
        (
            &[
                "--native-max-file-mib",
                "128",
                "--native-max-processes",
                "64",
            ],
            (128 << 20, 64),
        ),
    ];
    for (extra, limits) in started {
        let (ran, said_hello, seen) = run(extra);
        assert_eq!(ran.status(), code(0), "{extra:?}: {}", ran.err);
        assert!(said_hello, "{extra:?}: no hello");
        assert_eq!(seen, vec![limits], "{extra:?}");
    }
    let too_big = (u64::MAX >> 20) + 1;
    let too_big_text = too_big.to_string();
    let refused: [(&[&str], String); 5] = [
        (
            &["--native-max-file-mib", "127"],
            "--native-max-file-mib: 127 is below 128, the largest output a build writes by \
             default"
                .to_owned(),
        ),
        (
            &["--native-max-file-mib", "0"],
            "--native-max-file-mib and --native-max-processes: above 0".to_owned(),
        ),
        (
            &["--native-max-processes", "0"],
            "--native-max-file-mib and --native-max-processes: above 0".to_owned(),
        ),
        (
            &["--native-max-file-mib", &too_big_text],
            format!("--native-max-file-mib: {too_big} is too large"),
        ),
        (
            &["--native-max-processes", "4294967295"],
            "--native-max-processes: 4294967295 is too large".to_owned(),
        ),
    ];
    for (extra, why) in refused {
        let (ran, said_hello, seen) = run(extra);
        assert_eq!(ran.status(), code(2), "{why}: {}", ran.err);
        assert_eq!(
            ran.err.lines().last(),
            Some(format!("js-agent: {why}").as_str()),
            "{}",
            ran.err
        );
        assert!(!said_hello, "{why}");
        assert_eq!(seen, Vec::new(), "{why}");
    }
    for (flag, value) in [
        ("--native-max-file-mib", "256"),
        ("--native-max-processes", "64"),
    ] {
        let args = flags(&["--no-jobs", flag, value]);
        let (script, sent) = Script::new(vec![welcome(1)]);
        let connector = ScriptConnector::of(script);
        let mut sleeper = CountingSleeper::stopping_at(1);
        let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
        let ran = run_on_defaulted(
            &args,
            &mac_host(),
            &connector,
            &mut sleeper,
            &mut Lowest,
            jobs,
        );
        assert_eq!(ran.status(), code(2), "{flag}: {}", ran.err);
        assert_eq!(
            ran.err,
            format!("js-agent: {flag}: not with --no-jobs (an agent that runs no job)\n"),
            "{flag}"
        );
        assert!(sent.borrow().is_empty(), "{flag}: something was sent");
    }
}

/// Asserts that a Mac agent given a flag only a Linux build has, in the spaced and the attached
/// spelling, ends with status 2 before anything is sent, with exactly the refusal that names
/// the flag, says it is Linux only, and names `--native-accounts` as what this build runs.
/// Catches a refusal that leaves out what the Mac build runs jobs with, or one that reaches
/// clap's unknown-flag text instead.
#[test]
fn a_linux_only_flag_is_refused_on_a_mac_naming_native_accounts() {
    let cases: [(&[&str], &str); 2] = [
        (&["--container-bin", "/bin/sh"], "container-bin"),
        (&["--job-user=x"], "job-user"),
    ];
    for (extra, name) in cases {
        let args = flags(extra);
        let (script, sent) = Script::new(vec![welcome(1)]);
        let connector = ScriptConnector::of(script);
        let mut sleeper = CountingSleeper::stopping_at(1);
        let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
        let ran = run_on_defaulted(
            &args,
            &mac_host(),
            &connector,
            &mut sleeper,
            &mut Lowest,
            jobs,
        );
        assert_eq!(ran.status(), code(2), "{extra:?}: {}", ran.err);
        assert!(sent.borrow().is_empty(), "{extra:?}: something was sent");
        assert_eq!(
            ran.err,
            format!(
                "js-agent: --{name}: Linux only (a flag of crun or of the container engine): \
                 this macOS build runs native jobs (--native-accounts)\n"
            ),
            "{extra:?}"
        );
    }
}

/// Asserts that a native agent whose runtime cannot be made ends with status 2 before the
/// hello, and that the text of its last line is exactly the reason under `--native-accounts`,
/// the flag of the native runtime. Catches a start failure told under another runtime's flag,
/// one that loses the reason, and an agent that says hello with no worker.
#[test]
fn a_native_worker_that_cannot_start_ends_the_agent_naming_native_accounts() {
    let dir = Scratch::new("native-agent-worker-fails");
    let extra = native_flags(&dir);
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let machine = FakeMachine::failing(Build::RuntimeFails);
    let (ran, hello) = hello_with(&mac_host(), &refs, welcome(1), machine);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(
        ran.err.lines().last(),
        Some("js-agent: --native-accounts: the engine does not answer"),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);
}

/// Asserts that the agent judges Full Disk Access from the file reader it is given, not from
/// the real file system: with the privacy directory not in the fake's files the hello says
/// `undetermined` (a listing that failed for another reason than permission), and with it
/// there but unlistable it says `not-granted`. A listing of the real directory of a test run
/// gives neither of the first. Catches an agent that lists the real directory, one that asks
/// another directory than [`PRIVACY_DIR`], and one that does not report what it found.
#[test]
fn the_agent_judges_full_disk_access_from_its_own_file_reader() {
    let cases = [
        (mac_host().files, "undetermined"),
        (
            mac_host().files.with_unlistable_dir(PRIVACY_DIR),
            "not-granted",
        ),
    ];
    for (n, (files, expected)) in cases.into_iter().enumerate() {
        let dir = Scratch::new(&format!("native-agent-fda{n}"));
        let mut host = mac_host();
        host.files = files;
        let (ran, hello) = hello_on(&host, &native_flags(&dir), welcome(1));
        assert_eq!(ran.status(), code(0), "{expected}: {}", ran.err);
        assert_eq!(hello["capabilities"][FULL_DISK_ACCESS], expected);
    }
}

/// Asserts that the self-test the agent runs before its hello ends the start when it fails,
/// with a text that begins with `--native-accounts: the self-test failed: ` and the
/// supervisor's failure, and that a passing one runs `/usr/bin/true` once through an account
/// it then retires, and gives back the runtime with no warning. Catches a start that says hello over a pool that cannot run a
/// job, one that tells the failure under another flag, and one that skips the self-test.
#[test]
fn the_self_test_ends_the_start_when_it_fails() {
    let rig = Rig::new("agent-self-test-fails");
    rig.launcher.fail(Failing::Setup);
    let why = match js_agent::platform::self_tested(rig.rt) {
        Ok(_) => panic!("the self-test must fail"),
        Err(why) => why,
    };
    let prefix = "--native-accounts: the self-test failed: \
                  the supervisor: ended before the job started: SetupFailed";
    assert!(why.starts_with(prefix), "{why}");

    let rig = Rig::new("agent-self-test-passes");
    let lent = rig.accounts.records();
    assert_eq!(lent.len(), 1, "{lent:?}");
    let (_, warnings) = js_agent::platform::self_tested(rig.rt).unwrap();
    assert_eq!(warnings, Vec::<String>::new());
    let launched = rig.launcher.launched();
    assert_eq!(launched.len(), 1, "the self-test starts one supervisor");
    assert_eq!(launched[0].program, "/usr/bin/true");
    assert!(
        rig.accounts.records().iter().all(|a| !lent.contains(a)),
        "the self-test's account is retired"
    );
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
}
