//! The verb itself: argument handling in both forms hostkeeper uses (behind `update-hook` and the
//! bare step), the exit status of each outcome, the machine's steps, the dispatch in `main_with`
//! and the process.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use js_agent::update_hook::{
    Class, Done, DrainParams, Host, Invocation, Machine, MaintainInvocation, MaintenanceStep,
    Outcome, Params, Reason, Report, Step, VERB, fd_stat, run, run_step,
};
use js_testkit::dir::TestDir;

/// The params of a host the hook cannot keep its state on, whoever runs the test: the state
/// directory's parent is `/dev/null`, a device file, which every Unix has and the hook refuses
/// as not a directory without making or changing anything. (A parent that is merely not there
/// would be made by the hook, as on a first install, and only a root test could make it.)
const PARAMS: &str = concat!(
    "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
    "drain_wait_secs = 3600\ndefer_retry_secs = 300\n",
    "unit = \"js-agent-crun.service\"\nstate_dir = \"/dev/null/js-agent-update\"\n",
    "release_link = \"/nowhere/current\"\nflags_file = \"/nowhere/flags\"\n",
    "token_file = \"/nowhere/token\"\nstate_root = \"/nowhere/state\"\nruntime = \"crun\"\n",
    "prepare_timeout_secs = 3900\ncutover_timeout_secs = 4320\nverify_timeout_secs = 615\n",
    "rollback_timeout_secs = 8220\ncleanup_timeout_secs = 120\nverify_deadline_secs = 9000\n",
    "\"updater.artifact_digest\" = \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
    "\"updater.staged_release\" = \"/staged\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
);

/// The two ways the updater starts the hook: behind the verb, and with the bare step.
const FORMS: [bool; 2] = [true, false];

fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// The command line: the program, the verb when `verb`, and `list`.
fn words(verb: bool, list: &[&str]) -> Vec<OsString> {
    ["js-agent"]
        .into_iter()
        .chain(verb.then_some(VERB))
        .chain(list.iter().copied())
        .map(OsString::from)
        .collect()
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

/// The maintenance steps of a host whose test runs an update step: none runs.
fn no_maintain(_: &MaintainInvocation, _: &DrainParams) -> Done {
    unreachable!("a step of an update ran the maintenance steps")
}

/// Runs the hook behind the verb.
fn run_with(args: &[&str], host: &Host<'_>) -> Ran {
    run_in(true, args, host)
}

/// Runs the hook in the form `verb` says.
fn run_in(verb: bool, args: &[&str], host: &Host<'_>) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(verb, args), &mut out, &mut err, host);
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

/// Each outcome exits with its status and prints the step's report as the last line; a step that
/// does not succeed also says on standard error how it ended, with the reason and the detail of
/// that line, which is what hostkeeper's journal keeps of a hook, and a success says its detail
/// there. Catches a refusal that the journal shows as a bare reason token.
#[test]
fn each_outcome_exits_with_its_status_and_prints_the_report_the_step_gave() {
    let (_dir, params) = params_file("hook-verb-outcomes", PARAMS);
    let said = |ending: &str| {
        format!("js-agent update-hook: verify {ending}: not_implemented: the step's own words\n")
    };
    let cases = [
        (
            Outcome::Success,
            0,
            "js-agent update-hook: verify succeeded: the step's own words\n".to_owned(),
        ),
        (Outcome::Failure, 1, said("failed")),
        (Outcome::Deferred, 75, said("deferred")),
        (Outcome::Halt, 76, said("halted")),
    ];
    for (outcome, code, err) in cases {
        let report = Report {
            class: Class::Environment,
            retry_after_secs: Some(300),
            reversible: false,
            ..Report::failure(Reason::NotImplemented, "the step's own words")
        };
        let given = report.clone();
        let step = move |_: &Invocation, _: &Params| Done {
            outcome,
            report: given.clone(),
        };
        for verb in FORMS {
            let ran = run_in(
                verb,
                &["verify", "--params", &params],
                &Host {
                    step: &step,
                    maintain: &no_maintain,
                },
            );
            assert_eq!(ran.status, status(code), "{outcome:?}");
            assert_eq!(outcome.code(), code);
            assert_eq!(ran.out, format!("{}\n", report.to_line()));
            assert_eq!(ran.err, err, "{outcome:?}");
        }
    }
}

/// A success with nothing to say (no detail) writes nothing on standard error, in both forms.
#[test]
fn a_success_without_a_detail_says_nothing_on_standard_error() {
    let (_dir, params) = params_file("hook-verb-quiet-success", PARAMS);
    let step = |_: &Invocation, _: &Params| Done {
        outcome: Outcome::Success,
        report: Report::default(),
    };
    for verb in FORMS {
        let ran = run_in(
            verb,
            &["cleanup", "--params", &params],
            &Host {
                step: &step,
                maintain: &no_maintain,
            },
        );
        assert_eq!(ran.status, status(0));
        assert_eq!(ran.out, format!("{}\n", Report::default().to_line()));
        assert_eq!(ran.err, "");
    }
}

#[test]
fn the_step_is_given_the_invocation_and_the_params_it_was_asked_with() {
    let (_dir, params) = params_file("hook-verb-given", PARAMS);
    let seen = RefCell::new(Vec::new());
    let step = |invocation: &Invocation, params: &Params| {
        seen.borrow_mut().push((invocation.clone(), params.clone()));
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    for verb in FORMS {
        let ran = run_in(
            verb,
            &["prepare", "/staged", "--params", &params],
            &Host {
                step: &step,
                maintain: &no_maintain,
            },
        );
        assert_eq!(ran.status, status(0));
    }
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 2);
    // Behind the verb or bare, the step is given the same invocation and the same table.
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0].0.step, Step::Prepare);
    assert_eq!(
        seen[0].0.artifact.as_deref(),
        Some(std::path::Path::new("/staged"))
    );
    assert_eq!(seen[0].1.cordon_lease_secs, 300);
    assert_eq!(seen[0].1.cordon_max_secs, 17355);
    assert_eq!(seen[0].1.settings.unit, "js-agent-crun.service");
}

#[test]
fn a_refused_command_line_runs_no_step_and_still_ends_with_a_valid_line() {
    let called = Cell::new(false);
    let step = |_: &Invocation, _: &Params| {
        called.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    // `uninstall` is hostkeeper's step for a service that declares an uninstall bound; the
    // package declares none, and the word is refused like any other the hook does not know.
    let cases: [&[&str]; 6] = [
        &[],
        &["restart", "--params", "/p"],
        &["prepare", "--params", "/p"],
        &["cutover"],
        &["uninstall", "--params", "/p"],
        &["--release-link=/opt/x", "cutover", "--params", "/p"],
    ];
    for args in cases {
        for verb in FORMS {
            // Bare, nothing at all is not a command line of the hook's: it needs the verb.
            let ran = run_in(
                verb,
                args,
                &Host {
                    step: &step,
                    maintain: &no_maintain,
                },
            );
            assert_eq!(ran.status, status(1), "{args:?}");
            let line = last_line(&ran.out);
            assert_eq!(line["reason"], "bad_args", "{args:?}");
            assert_eq!(line["class"], "release");
            assert_eq!(line["reversible"], true);
            assert!(ran.err.starts_with("js-agent update-hook: "), "{}", ran.err);
        }
    }
    assert!(!called.get());
}

#[test]
fn a_prepare_artifact_that_is_not_the_staged_release_of_the_table_runs_no_step() {
    let (_dir, params) = params_file("hook-verb-artifact", PARAMS);
    let called = Cell::new(false);
    let step = |_: &Invocation, _: &Params| {
        called.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    for artifact in ["/other", "/staged/", "/staged/sub", "staged"] {
        let ran = run_with(
            &["prepare", artifact, "--params", &params],
            &Host {
                step: &step,
                maintain: &no_maintain,
            },
        );
        assert_eq!(ran.status, status(1), "{artifact}");
        let line = last_line(&ran.out);
        assert_eq!(line["reason"], "bad_args", "{artifact}");
        assert!(ran.err.contains("is not the staged release"), "{}", ran.err);
    }
    assert!(!called.get());
    // The same path runs the step.
    let ran = run_with(
        &["prepare", "/staged", "--params", &params],
        &Host {
            step: &step,
            maintain: &no_maintain,
        },
    );
    assert_eq!(ran.status, status(0));
    assert!(called.get());
}

#[test]
fn an_unreadable_or_incomplete_params_file_runs_no_step() {
    let called = Cell::new(false);
    let step = |_: &Invocation, _: &Params| {
        called.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let (dir, incomplete) = params_file("hook-verb-badparams", "cordon_lease_secs = 1\n");
    let absent = dir.path().join("absent.toml");
    for file in [incomplete.as_str(), absent.to_str().unwrap()] {
        let ran = run_with(
            &["cleanup", "--params", file],
            &Host {
                step: &step,
                maintain: &no_maintain,
            },
        );
        assert_eq!(ran.status, status(1), "{file}");
        let line = last_line(&ran.out);
        assert_eq!(line["reason"], "bad_params", "{file}");
        assert!(ran.err.starts_with("js-agent update-hook: "), "{}", ran.err);
    }
    assert!(!called.get());
}

#[test]
fn every_step_of_a_machine_ends_state_invalid_when_the_state_directory_cannot_be_made() {
    let (_dir, params) = params_file("hook-verb-machine", PARAMS);
    let stat = |_: &std::path::Path, fd: std::os::fd::BorrowedFd<'_>| fd_stat(fd);
    let own_id = || Err("unused".to_owned());
    let machine = Machine {
        stat: &stat,
        own_host_id: &own_id,
        ..Machine::system()
    };
    let run_machine =
        |invocation: &Invocation, params: &Params| run_step(invocation, params, &machine);
    for step in Step::ALL {
        for verb in FORMS {
            let mut args = vec![step.name()];
            if step == Step::Prepare {
                args.push("/staged");
            }
            args.extend(["--params", &params]);
            let ran = run_in(
                verb,
                &args,
                &Host {
                    step: &run_machine,
                    maintain: &no_maintain,
                },
            );
            assert_eq!(ran.status, status(1), "{step}");
            let line = last_line(&ran.out);
            assert_eq!(line["reason"], "state_invalid", "{step}");
            assert!(
                line["detail"].as_str().unwrap().contains("/dev/null"),
                "{step}: {line}"
            );
            assert_eq!(line["class"], "release");
            assert_eq!(line["reversible"], true);
            assert_eq!(line["promotion_pending"], false);
        }
    }
}

#[test]
fn main_with_answers_the_verb_and_the_bare_steps_before_it_installs_the_signal_handlers() {
    let (_dir, params) = params_file("hook-verb-dispatch", PARAMS);
    for verb in FORMS {
        let installed = Cell::new(false);
        let install = || {
            installed.set(true);
            Err(std::io::Error::other("no signal handlers in a hook"))
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = js_agent::main_with(
            &install,
            words(verb, &["verify", "--params", &params]),
            &mut out,
            &mut err,
        );
        assert!(!installed.get(), "the hook installed the signal handlers");
        assert_eq!(format!("{code:?}"), status(1));
        let line = last_line(&String::from_utf8(out).unwrap());
        assert_eq!(line["reason"], "state_invalid");
    }
}

#[test]
fn the_binary_prints_one_valid_last_line_and_exits_1() {
    let (_dir, params) = params_file("hook-verb-process", PARAMS);
    for verb in FORMS {
        let output = Command::new(env!("CARGO_BIN_EXE_js-agent"))
            .args(verb.then_some(VERB))
            .arg("prepare")
            .arg("/staged")
            .args(["--params", &params])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let detail = "above the state directory, /dev/null: is not a directory";
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("js-agent update-hook: prepare failed: state_invalid: {detail}\n")
        );
        let line = last_line(&String::from_utf8(output.stdout).unwrap());
        assert_eq!(line["detail"], detail);
        assert_eq!(line["class"], "release");
        assert_eq!(line["reason"], "state_invalid");
        assert_eq!(line["reversible"], true);
        assert_eq!(line["promotion_pending"], false);
    }
}

#[test]
fn the_binary_refuses_a_missing_params_flag_with_a_valid_line() {
    for words in [vec![VERB, "cutover"], vec!["cutover"], vec!["uninstall"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_js-agent"))
            .args(&words)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{words:?}");
        let line = last_line(&String::from_utf8(output.stdout).unwrap());
        assert_eq!(line["reason"], "bad_args", "{words:?}");
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("js-agent update-hook: "));
    }
}

#[test]
fn a_params_file_without_a_good_artifact_digest_runs_no_step_on_any_step() {
    let called = Cell::new(false);
    let step = |_: &Invocation, _: &Params| {
        called.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let digest_line = format!(
        "\"updater.artifact_digest\" = \"sha256:{}\"\n",
        "a".repeat(64)
    );
    let text = PARAMS;
    assert!(text.contains(&digest_line), "{text}");
    let missing = text.replace(&digest_line, "");
    let malformed = text.replace(&"a".repeat(64), "xyz");
    let (_dir, missing_file) = params_file("hook-verb-nodigest", &missing);
    let (_dir2, malformed_file) = params_file("hook-verb-baddigest", &malformed);
    for step_name in ["prepare", "cutover", "verify", "rollback", "cleanup"] {
        for file in [&missing_file, &malformed_file] {
            let mut args = vec![step_name];
            if step_name == "prepare" {
                args.push("/staged");
            }
            args.extend(["--params", file.as_str()]);
            for verb in FORMS {
                let ran = run_in(
                    verb,
                    &args,
                    &Host {
                        step: &step,
                        maintain: &no_maintain,
                    },
                );
                assert_eq!(ran.status, status(1), "{step_name}");
                assert_eq!(last_line(&ran.out)["reason"], "bad_params", "{step_name}");
            }
        }
    }
    assert!(!called.get());
}

/// A params file of a maintenance run whose state root is `state_root`: the keys `drain` and
/// `resume` read, and keys they ignore.
fn maintain_text(state_root: &std::path::Path) -> String {
    format!(
        concat!(
            "unit = \"js-agent-crun.service\"\nflags_file = \"/nowhere/flags\"\n",
            "token_file = \"/nowhere/token\"\nstate_root = \"{}\"\n",
            "cordon_lease_secs = 300\ndefer_retry_secs = 120\n",
            "\"updater.service\" = \"js-agent\"\n\"updater.run_id\" = \"r1\"\n",
            "\"updater.drain_deadline_unix\" = 1893456000\n",
        ),
        state_root.display()
    )
}

/// Asserts that `drain` and `resume`, behind the verb and bare, call the maintenance step and
/// never the update step, with the invocation and the params read from the file; that each
/// outcome exits with its status and prints the step's last line; and that standard error names
/// the step word (`drain deferred: drain_deadline: ...`, `resume succeeded: ...`). It catches a
/// dispatch that runs a maintenance word as an update step or reports it under another word.
#[test]
fn a_maintenance_step_runs_the_maintain_function_and_reports_under_its_own_word() {
    let (dir, params) = params_file(
        "hook-verb-maintain",
        &maintain_text(std::path::Path::new("/the/state")),
    );
    let update_ran = Cell::new(false);
    let step = |_: &Invocation, _: &Params| {
        update_ran.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let cases = [
        (
            MaintenanceStep::Drain,
            Outcome::Deferred,
            75,
            Report {
                retry_after_secs: Some(120),
                ..Report::failure(Reason::DrainDeadline, "work is left")
            },
            "js-agent update-hook: drain deferred: drain_deadline: work is left\n",
        ),
        (
            MaintenanceStep::Drain,
            Outcome::Success,
            0,
            Report::default(),
            "",
        ),
        (
            MaintenanceStep::Resume,
            Outcome::Success,
            0,
            Report {
                detail: Some("the agent was restarted".to_owned()),
                ..Report::default()
            },
            "js-agent update-hook: resume succeeded: the agent was restarted\n",
        ),
        (
            MaintenanceStep::Resume,
            Outcome::Failure,
            1,
            Report::failure(Reason::HoldFailed, "the hold file: gone"),
            "js-agent update-hook: resume failed: hold_failed: the hold file: gone\n",
        ),
    ];
    for (which, outcome, code, report, err) in cases {
        let seen = RefCell::new(Vec::new());
        let given = report.clone();
        let maintain = |invocation: &MaintainInvocation, table: &DrainParams| {
            seen.borrow_mut().push((invocation.clone(), table.clone()));
            Done {
                outcome,
                report: given.clone(),
            }
        };
        for verb in FORMS {
            let host = Host {
                step: &step,
                maintain: &maintain,
            };
            let ran = run_in(verb, &[which.name(), "--params", &params], &host);
            assert_eq!(ran.status, status(code), "{which} {outcome:?}");
            assert_eq!(ran.out, format!("{}\n", report.to_line()));
            assert_eq!(ran.err, err);
        }
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 2, "{which}");
        assert_eq!(seen[0], seen[1]);
        assert_eq!(seen[0].0.step, which);
        assert_eq!(seen[0].0.params, std::path::Path::new(&params));
        assert_eq!(seen[0].1.unit, "js-agent-crun.service");
        assert_eq!(seen[0].1.state_root, std::path::Path::new("/the/state"));
        assert_eq!(seen[0].1.cordon_lease_secs, 300);
        assert_eq!(seen[0].1.defer_retry_secs, 120);
        assert_eq!(seen[0].1.drain_deadline_unix, Some(1_893_456_000));
    }
    assert!(!update_ran.get());
    drop(dir);
}

/// Asserts that a params file `DrainParams` refuses (a lease of one second, a file that is not
/// there) ends `bad_params` with exit 1 and the refusal's text on standard error, and that
/// `maintain` does not run. It catches a drain that starts before its table is known good.
#[test]
fn a_maintenance_step_with_a_bad_params_file_is_refused_before_it_runs() {
    let called = Cell::new(false);
    let maintain = |_: &MaintainInvocation, _: &DrainParams| {
        called.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let host = Host {
        step: &no_maintain_step,
        maintain: &maintain,
    };
    let (dir, short) = params_file(
        "hook-verb-maintain-badparams",
        &maintain_text(std::path::Path::new("/s"))
            .replace("cordon_lease_secs = 300", "cordon_lease_secs = 1"),
    );
    let absent = dir.path().join("absent.toml");
    let absent = absent.to_str().unwrap();
    for which in MaintenanceStep::ALL {
        for verb in FORMS {
            let ran = run_in(verb, &[which.name(), "--params", &short], &host);
            assert_eq!(ran.status, status(1), "{which}");
            let line = last_line(&ran.out);
            assert_eq!(line["reason"], "bad_params");
            assert_eq!(line["class"], "release");
            assert_eq!(
                ran.err,
                "js-agent update-hook: cordon_lease_secs 1 is shorter than 30 s, three polls\n"
            );
            let ran = run_in(verb, &[which.name(), "--params", absent], &host);
            assert_eq!(ran.status, status(1), "{which}");
            assert_eq!(last_line(&ran.out)["reason"], "bad_params");
            assert!(
                ran.err
                    .starts_with("js-agent update-hook: cannot read the params file: "),
                "{}",
                ran.err
            );
        }
    }
    assert!(!called.get());
}

fn no_maintain_step(_: &Invocation, _: &Params) -> Done {
    unreachable!("a maintenance step ran the update step")
}

/// Asserts that an artifact given to `drain` is `bad_args` naming it, with exit 1 and no step
/// run, and that `drain` without `--params` is `bad_args` too. It catches a drain that takes the
/// staged-release argument of `prepare`.
#[test]
fn a_maintenance_step_given_an_artifact_or_no_params_runs_no_step() {
    let (_dir, params) = params_file(
        "hook-verb-maintain-badargs",
        &maintain_text(std::path::Path::new("/s")),
    );
    let called = Cell::new(false);
    let maintain = |_: &MaintainInvocation, _: &DrainParams| {
        called.set(true);
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    };
    let host = Host {
        step: &no_maintain_step,
        maintain: &maintain,
    };
    for verb in FORMS {
        let ran = run_in(verb, &["drain", "/staged", "--params", &params], &host);
        assert_eq!(ran.status, status(1));
        assert_eq!(last_line(&ran.out)["reason"], "bad_args");
        assert_eq!(
            ran.err,
            "js-agent update-hook: unexpected argument \"/staged\"\n"
        );
        let ran = run_in(verb, &["resume"], &host);
        assert_eq!(ran.status, status(1));
        assert_eq!(last_line(&ran.out)["reason"], "bad_args");
        assert_eq!(
            ran.err,
            "js-agent update-hook: --params <file> is required\n"
        );
    }
    assert!(!called.get());
}

/// Asserts that the machine's maintenance steps, reached through `Host::system()`, end `resume`
/// with success and the empty report, and `drain` with `state_invalid` and its exact text, when
/// the state root the params name is not there, in both forms. It catches a host whose
/// `maintain` is not the machine's, and a drain that succeeds with no agent to drain.
#[test]
fn the_machine_resumes_nothing_and_refuses_to_drain_when_the_state_root_is_not_there() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-verb-maintain-machine");
    let absent = dir.path().join("state");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, maintain_text(&absent)).unwrap();
    let params = file.to_str().unwrap();
    for verb in FORMS {
        let ran = run_in(verb, &["resume", "--params", params], &Host::system());
        assert_eq!(ran.status, status(0));
        assert_eq!(ran.out, format!("{}\n", Report::default().to_line()));
        assert_eq!(ran.err, "");
        let ran = run_in(verb, &["drain", "--params", params], &Host::system());
        let detail = format!(
            "state_root: {} is not there, and a drain needs the state root of the agent that runs",
            absent.display()
        );
        assert_eq!(ran.status, status(1));
        assert_eq!(
            ran.err,
            format!("js-agent update-hook: drain failed: state_invalid: {detail}\n")
        );
        let line = last_line(&ran.out);
        assert_eq!(line["reason"], "state_invalid");
        assert_eq!(line["detail"], detail.as_str());
        assert_eq!(line["class"], "release");
    }
    assert!(!absent.exists());
}

/// Asserts that `main_with` answers the bare `drain` and `resume` through the hook before it
/// installs the signal handlers. It catches a program that sends the new words to the agent.
#[test]
fn main_with_answers_the_maintenance_words_before_it_installs_the_signal_handlers() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-verb-maintain-main");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, maintain_text(&dir.path().join("state"))).unwrap();
    let params = file.to_str().unwrap().to_owned();
    for (word, code, reason) in [("drain", 1, Some("state_invalid")), ("resume", 0, None)] {
        for verb in FORMS {
            let installed = Cell::new(false);
            let install = || {
                installed.set(true);
                Err(std::io::Error::other("no signal handlers in a hook"))
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let exit = js_agent::main_with(
                &install,
                words(verb, &[word, "--params", &params]),
                &mut out,
                &mut err,
            );
            assert!(!installed.get(), "{word}: the hook installed the handlers");
            assert_eq!(format!("{exit:?}"), status(code), "{word}");
            let line = last_line(&String::from_utf8(out).unwrap());
            match reason {
                Some(reason) => assert_eq!(line["reason"], reason),
                None => assert!(line.get("reason").is_none(), "{line}"),
            }
        }
    }
}
