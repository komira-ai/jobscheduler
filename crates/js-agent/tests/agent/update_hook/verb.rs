//! The verb itself: argument handling, the exit status of each outcome, the machine's steps, the
//! dispatch in `main_with` and the process.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use js_agent::update_hook::{
    Class, DECLARATION_PATH, Done, Host, Invocation, Outcome, Params, Reason, Report, Step, VERB,
    run,
};
use js_testkit::dir::TestDir;

const PARAMS: &str = concat!(
    "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
    "drain_wait_secs = 3600\ndefer_retry_secs = 300\n",
    "\"updater.artifact_digest\" = \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
    "\"updater.staged_release\" = \"/staged\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
);

fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

fn words(list: &[&str]) -> Vec<OsString> {
    ["js-agent", VERB]
        .into_iter()
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

fn run_with(args: &[&str], host: &Host<'_>) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&words(args), &mut out, &mut err, host);
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

#[test]
fn each_outcome_exits_with_its_status_and_prints_the_report_the_step_gave() {
    let (_dir, params) = params_file("hook-verb-outcomes", PARAMS);
    let cases = [
        (Outcome::Success, 0),
        (Outcome::Failure, 1),
        (Outcome::Deferred, 75),
        (Outcome::Halt, 76),
    ];
    for (outcome, code) in cases {
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
        let ran = run_with(&["verify", "--params", &params], &Host { step: &step });
        assert_eq!(ran.status, status(code), "{outcome:?}");
        assert_eq!(outcome.code(), code);
        assert_eq!(ran.out, format!("{}\n", report.to_line()));
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
    let ran = run_with(
        &["prepare", "/staged", "--params", &params],
        &Host { step: &step },
    );
    assert_eq!(ran.status, status(0));
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0.step, Step::Prepare);
    assert_eq!(
        seen[0].0.artifact.as_deref(),
        Some(std::path::Path::new("/staged"))
    );
    assert_eq!(seen[0].1.cordon_lease_secs, 300);
    assert_eq!(seen[0].1.cordon_max_secs, 17355);
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
    let cases: [&[&str]; 4] = [
        &[],
        &["restart", "--params", "/p"],
        &["prepare", "--params", "/p"],
        &["cutover"],
    ];
    for args in cases {
        let ran = run_with(args, &Host { step: &step });
        assert_eq!(ran.status, status(1), "{args:?}");
        let line = last_line(&ran.out);
        assert_eq!(line["reason"], "bad_args", "{args:?}");
        assert_eq!(line["class"], "release");
        assert_eq!(line["reversible"], true);
        assert!(ran.err.starts_with("js-agent update-hook: "), "{}", ran.err);
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
            &Host { step: &step },
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
        &Host { step: &step },
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
        let ran = run_with(&["cleanup", "--params", file], &Host { step: &step });
        assert_eq!(ran.status, status(1), "{file}");
        let line = last_line(&ran.out);
        assert_eq!(line["reason"], "bad_params", "{file}");
        assert!(ran.err.starts_with("js-agent update-hook: "), "{}", ran.err);
    }
    assert!(!called.get());
}

#[test]
fn every_step_of_this_machine_reads_the_installed_declaration_before_anything_else() {
    assert!(
        !std::path::Path::new(DECLARATION_PATH).exists(),
        "this test needs a machine with no declaration installed"
    );
    let (_dir, params) = params_file("hook-verb-machine", PARAMS);
    for step in Step::ALL {
        let mut args = vec![step.name()];
        if step == Step::Prepare {
            args.push("/staged");
        }
        args.extend(["--params", &params]);
        let ran = run_with(&args, &Host::system());
        assert_eq!(ran.status, status(1), "{step}");
        let line = last_line(&ran.out);
        assert_eq!(line["reason"], "state_invalid", "{step}");
        assert!(
            line["detail"].as_str().unwrap().contains("declaration"),
            "{step}: {line}"
        );
        assert_eq!(line["class"], "release");
        assert_eq!(line["reversible"], true);
        assert_eq!(line["promotion_pending"], false);
    }
}

#[test]
fn main_with_answers_the_verb_before_it_installs_the_signal_handlers() {
    let (_dir, params) = params_file("hook-verb-dispatch", PARAMS);
    let installed = Cell::new(false);
    let install = || {
        installed.set(true);
        Err(std::io::Error::other("no signal handlers in a hook"))
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_agent::main_with(
        &install,
        words(&["verify", "--params", &params]),
        &mut out,
        &mut err,
    );
    assert!(!installed.get(), "the verb installed the signal handlers");
    assert_eq!(format!("{code:?}"), status(1));
    let line = last_line(&String::from_utf8(out).unwrap());
    assert_eq!(line["reason"], "state_invalid");
}

#[test]
fn the_binary_prints_one_valid_last_line_and_exits_1() {
    let (_dir, params) = params_file("hook-verb-process", PARAMS);
    let output = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args([VERB, "prepare"])
        .arg("/staged")
        .args(["--params", &params])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let line = last_line(&String::from_utf8(output.stdout).unwrap());
    assert_eq!(line["class"], "release");
    assert_eq!(line["reason"], "state_invalid");
    assert_eq!(line["reversible"], true);
    assert_eq!(line["promotion_pending"], false);
}

#[test]
fn the_binary_refuses_a_missing_params_flag_with_a_valid_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args([VERB, "cutover"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let line = last_line(&String::from_utf8(output.stdout).unwrap());
    assert_eq!(line["reason"], "bad_args");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--params"));
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
            let ran = run_with(&args, &Host { step: &step });
            assert_eq!(ran.status, status(1), "{step_name}");
            assert_eq!(last_line(&ran.out)["reason"], "bad_params", "{step_name}");
        }
    }
    assert!(!called.get());
}
