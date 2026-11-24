//! The verb itself: the exit status of each outcome, the last line and the words on standard
//! error, what is refused before a step runs, `uninstall`, and the dispatch from the program.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use js_agent::update_hook::config::{Done, Host, Params, Reason, Report, VERB, run};
use js_agent::update_hook::{
    Class, Host as UpdateHost, Invocation, Outcome, Step, is_reason_token,
};
use js_testkit::dir::TestDir;

use super::rig::text;

fn words(list: &[&str]) -> Vec<OsString> {
    ["js-agent", VERB]
        .into_iter()
        .chain(list.iter().copied())
        .map(OsString::from)
        .collect()
}

fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// A params file in a directory of its own.
fn params_file(name: &str, text: &str) -> (TestDir, String) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let file = dir.path().join("params.toml");
    std::fs::write(&file, text).unwrap();
    let path = file.to_str().unwrap().to_owned();
    (dir, path)
}

struct Ran {
    status: String,
    out: String,
    err: String,
}

fn run_with(list: &[&str], host: &Host<'_>) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(list), &mut out, &mut err, host);
    Ran {
        status: format!("{code:?}"),
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn last_line(out: &str) -> serde_json::Value {
    assert!(out.ends_with('\n'), "{out:?}");
    assert_eq!(out.trim_end().lines().count(), 1, "{out:?}");
    serde_json::from_str(out.trim_end()).unwrap()
}

fn params_text() -> String {
    text("/var/lib/js-agent-config-test", 600, 300, 600)
}

/// Asserts that every reason is a token the updater accepts, that the tokens are the twelve the
/// hook documents, and that no two are alike.
///
/// Catches a misspelt or over-long token, which the updater reads as no reason.
#[test]
fn every_reason_is_a_token_the_updater_accepts() {
    let tokens: Vec<_> = Reason::ALL.iter().map(|reason| reason.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "bad_args",
            "bad_params",
            "locked",
            "state_invalid",
            "bad_config",
            "stop_failed",
            "start_failed",
            "no_hello",
            "lease_unreachable",
            "not_verified",
            "keep_failed",
            "restore_failed"
        ]
    );
    for token in &tokens {
        assert!(is_reason_token(token), "{token}");
    }
}

/// Asserts, for a step that succeeds and one that fails, the exit status, that the last line of
/// standard output is one JSON object that says what the step said, and what goes to standard
/// error; and that the step is given the invocation and the params the file holds.
///
/// Catches an exit status that does not match the line, a last line that is not alone, and a
/// step run on the wrong params.
#[test]
fn each_ending_is_an_exit_status_a_last_line_and_a_line_of_words() {
    let (_dir, file) = params_file("config-verb-endings", &params_text());
    let seen = RefCell::new(Vec::new());
    let next = Cell::new(0);
    let step = |invocation: &Invocation, params: &Params| {
        seen.borrow_mut()
            .push((invocation.clone(), params.unit.clone()));
        next.set(next.get() + 1);
        match next.get() {
            1 => Done {
                outcome: Outcome::Success,
                report: Report::default(),
            },
            2 => Done {
                outcome: Outcome::Success,
                report: Report {
                    detail: Some("restarted".to_owned()),
                    ..Report::default()
                },
            },
            3 => Done {
                outcome: Outcome::Failure,
                report: Report {
                    class: Class::Environment,
                    ..Report::failure(Reason::LeaseUnreachable, "no server")
                },
            },
            _ => Done {
                outcome: Outcome::Deferred,
                report: Report {
                    class: Class::Environment,
                    retry_after_secs: Some(60),
                    ..Report::failure(Reason::LeaseUnreachable, "no server")
                },
            },
        }
    };
    let host = Host { step: &step };
    let ran = run_with(&["cutover", "--params", &file], &host);
    assert_eq!(ran.status, status(0));
    let line = last_line(&ran.out);
    assert_eq!(line["class"], "release");
    assert_eq!(line["reversible"], true);
    assert_eq!(line["promotion_pending"], false);
    assert!(line.get("reason").is_none() && line.get("detail").is_none());
    assert_eq!(ran.err, "");

    let ran = run_with(&["verify", "--params", &file], &host);
    assert_eq!(ran.status, status(0));
    assert_eq!(last_line(&ran.out)["detail"], "restarted");
    assert_eq!(
        ran.err,
        "js-agent config-hook: verify succeeded: restarted\n"
    );

    let ran = run_with(&["rollback", "--params", &file], &host);
    assert_eq!(ran.status, status(1));
    let line = last_line(&ran.out);
    assert_eq!(line["reason"], "lease_unreachable");
    assert_eq!(line["class"], "environment");
    assert_eq!(
        ran.err,
        "js-agent config-hook: rollback failed: lease_unreachable: no server\n"
    );

    let ran = run_with(&["cutover", "--params", &file], &host);
    assert_eq!(ran.status, status(75));
    let line = last_line(&ran.out);
    assert_eq!(line["reason"], "lease_unreachable");
    assert_eq!(line["retry_after_secs"], 60);
    assert_eq!(
        ran.err,
        "js-agent config-hook: cutover deferred: lease_unreachable: no server\n"
    );
    let seen = seen.borrow();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].0.step, Step::Cutover);
    assert_eq!(seen[0].1, "js-agent.service");
}

/// Asserts that a command line or params file the hook refuses ends the step before any step
/// runs: exit 1, `bad_args` or `bad_params`, reversible, and the reason on standard error.
///
/// Catches a refused invocation that runs a step on defaults.
#[test]
fn a_refused_command_line_or_params_file_runs_no_step() {
    let step = |_: &Invocation, _: &Params| -> Done { panic!("no step runs") };
    let host = Host { step: &step };
    let ran = run_with(&[], &host);
    assert_eq!(ran.status, status(1));
    let line = last_line(&ran.out);
    assert_eq!(
        (line["reason"].as_str(), line["detail"].as_str()),
        (Some("bad_args"), Some("no step given"))
    );
    assert_eq!(line["reversible"], true);
    assert_eq!(ran.err, "js-agent config-hook: no step given\n");

    let (_dir, bad) = params_file("config-verb-bad-params", "unit = \"x\"\n");
    let ran = run_with(&["cutover", "--params", &bad], &host);
    assert_eq!(ran.status, status(1));
    let line = last_line(&ran.out);
    assert_eq!(line["reason"], "bad_params");
    assert_eq!(line["detail"], "params key \"flags_file\" is required");
    let ran = run_with(&["cutover", "--params", "/nowhere/params.toml"], &host);
    assert_eq!(last_line(&ran.out)["reason"], "bad_params");
}

/// Asserts that `uninstall` ends in success with the bare last line, without a step run and
/// whether or not it is given a params file (one that does not exist is not read).
///
/// Catches an uninstall that is held up by a table it does not need, or that touches the unit.
#[test]
fn uninstall_does_nothing_and_succeeds() {
    let step = |_: &Invocation, _: &Params| -> Done { panic!("uninstall runs no step") };
    let host = Host { step: &step };
    for list in [
        &["uninstall"][..],
        &["uninstall", "--params", "/nowhere/params.toml"],
    ] {
        let ran = run_with(list, &host);
        assert_eq!(ran.status, status(0), "{list:?}");
        let line = last_line(&ran.out);
        assert_eq!(
            (line["class"].as_str(), line["reversible"].as_bool()),
            (Some("release"), Some(true))
        );
        assert!(line.get("reason").is_none());
        assert_eq!(ran.err, "");
    }
}

/// Asserts that `update_hook::run` hands the config verb to the config hook, and that
/// `main_with` answers it before it installs the signal handlers; a `uninstall` there ends 0.
///
/// Catches a verb that falls through to the agent's flag parser, or to the update hook.
#[test]
fn the_program_answers_the_config_verb_before_it_installs_the_signal_handlers() {
    let step = |_: &Invocation, _: &js_agent::update_hook::Params| -> js_agent::update_hook::Done {
        panic!("the update hook's step must not run")
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_agent::update_hook::run(
        &words(&[]),
        &mut out,
        &mut err,
        &UpdateHost {
            step: &step,
            maintain: &no_maintain,
        },
    );
    assert_eq!(format!("{code:?}"), status(1));
    assert_eq!(
        last_line(&String::from_utf8(out).unwrap())["detail"],
        "no step given"
    );

    let installed = Cell::new(false);
    let install = || {
        installed.set(true);
        Err(std::io::Error::other("no signal handlers in a hook"))
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_agent::main_with(&install, words(&["uninstall"]), &mut out, &mut err);
    assert!(!installed.get(), "the hook installed the signal handlers");
    assert_eq!(format!("{code:?}"), status(0));
    assert_eq!(
        last_line(&String::from_utf8(out).unwrap())["class"],
        "release"
    );
}

/// Asserts that the program's own dispatch runs a step on this machine's ports: `prepare` of
/// a params file whose state directory is in the test's scratch directory ends with one valid
/// last line, in success when the test runs as root (the directory is made, root's) and as
/// `state_invalid` otherwise (the hook keeps its state only in a directory root owns), and the
/// update hook's own step never runs.
///
/// Catches a verb whose machine host is never reached from the program, or one that runs the
/// update hook's steps on a config package's params.
#[test]
fn the_program_runs_a_config_step_on_the_machine() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "config-verb-machine");
    let root = dir.path().join("tree");
    std::fs::create_dir_all(&root).unwrap();
    let (_params_dir, file) = params_file(
        "config-verb-machine-params",
        &text(root.to_str().unwrap(), 600, 300, 600),
    );
    let step = |_: &Invocation, _: &js_agent::update_hook::Params| -> js_agent::update_hook::Done {
        panic!("the update hook's step must not run")
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_agent::update_hook::run(
        &words(&["prepare", "/staged", "--params", &file]),
        &mut out,
        &mut err,
        &UpdateHost {
            step: &step,
            maintain: &no_maintain,
        },
    );
    let line = last_line(&String::from_utf8(out).unwrap());
    if rustix::process::geteuid().is_root() {
        assert_eq!(format!("{code:?}"), status(0));
        assert!(line.get("reason").is_none(), "{line}");
        assert!(root.join("state").is_dir());
    } else {
        assert_eq!(format!("{code:?}"), status(1));
        assert_eq!(line["reason"], "state_invalid", "{line}");
        assert!(
            String::from_utf8(err)
                .unwrap()
                .starts_with("js-agent config-hook: prepare failed: state_invalid: ")
        );
    }
}

/// Asserts that the binary answers `config-hook` with one valid last line and the status of
/// the step: 0 for uninstall, 1 for a params file it cannot read.
///
/// Catches a binary that does not route the verb.
#[test]
fn the_binary_prints_one_valid_last_line_and_the_status_of_the_step() {
    let uninstall = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args([VERB, "uninstall"])
        .output()
        .unwrap();
    assert_eq!(uninstall.status.code(), Some(0));
    assert_eq!(
        last_line(&String::from_utf8(uninstall.stdout).unwrap())["class"],
        "release"
    );
    let refused = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args([VERB, "cutover", "--params", "/nowhere/params.toml"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .starts_with("js-agent config-hook: cannot read the params file: ")
    );
    assert_eq!(
        last_line(&String::from_utf8(refused.stdout).unwrap())["reason"],
        "bad_params"
    );
}

/// The maintenance body of an update host whose test runs no maintenance step.
fn no_maintain(
    _: &js_agent::update_hook::MaintainInvocation,
    _: &js_agent::update_hook::DrainParams,
) -> js_agent::update_hook::Done {
    unreachable!("a config step ran the update hook's maintenance steps")
}
