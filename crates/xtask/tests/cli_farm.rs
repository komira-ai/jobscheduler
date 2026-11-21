//! `cargo xtask farm-build` end to end: the verb is run as a process with `--buck2` naming the
//! stand-in (src/bin/xtask-fake.rs, linked as `buck2`), which answers each buck2 call from files of
//! the working directory and records the calls it got (`buck2.calls`). Each test reads the verb's
//! exit status and words, and those calls. Nothing here reaches a network or a build farm.

use std::path::Path;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;

const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// What buck2 prints for a key that is set.
const SET: &str = r#"{"buildrules_re.linux_x86_64_properties":"OSFamily=Linux,ISA=x86-64"}"#;
/// A farm build's summary line.
const FARM: &str = "Commands: 474 (cached: 0, remote: 474, local: 0)";
/// The audit call.
const AUDIT: &str = "audit config buildrules_re.linux_x86_64_properties --style json";
/// The calls of a run that got as far as the build.
const BUILT: [&str; 3] = [AUDIT, "build //:ci", "kill"];

/// A fresh working directory with `answers` (file name, text) in it, and the stand-in as `buck2`
/// beside them.
fn rig(name: &str, answers: &[(&str, &str)]) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("cli-farm-{name}"));
    std::fs::hard_link(FAKE, dir.join("buck2")).expect("link the stand-in as buck2");
    for (file, text) in answers {
        std::fs::write(dir.join(file), text).expect("an answer");
    }
    dir
}

/// `xtask <args>` in `cwd`.
fn xtask(cwd: &Path, args: &[&str]) -> Output {
    Command::new(XTASK)
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

/// `xtask farm-build --buck2 <buck2> --target //:ci` in `cwd`.
fn farm_build(cwd: &Path, buck2: &Path) -> Output {
    xtask(
        cwd,
        &[
            "farm-build",
            "--buck2",
            buck2.to_str().expect("utf-8"),
            "--target",
            "//:ci",
        ],
    )
}

/// The calls the stand-in recorded, one per line.
fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("buck2.calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_farm_build_prints_its_summary_line_and_stops_buck2_with_status_0() {
    // Catches: the build not run, run before the worker check, its log not passed on, its
    // summary not printed, or buck2's daemon left running.
    let log = format!("[t] Waiting on 3 actions\n[t] {FARM}\nBUILD SUCCEEDED\n");
    let d = rig(
        "ok",
        &[("buck2-audit.stdout", SET), ("buck2-build.stderr", &log)],
    );
    let o = farm_build(&d, &d.join("buck2"));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FARM}\n"));
    assert_eq!(stderr(&o), log);
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn a_build_with_actions_on_this_machine_fails_with_status_1() {
    // Catches: a build that ran an action on the runner taken for a farm build.
    let line = "Commands: 5 (cached: 2, remote: 1, local: 2)";
    let d = rig(
        "local",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", &format!("{line}\nBUILD SUCCEEDED\n")),
        ],
    );
    let o = farm_build(&d, &d.join("buck2"));
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert_eq!(
        stderr(&o),
        format!(
            "{line}\nBUILD SUCCEEDED\nxtask: `{line}`: actions ran on this machine, not on the farm (local must be 0)\n"
        )
    );
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn a_build_with_no_summary_line_fails_with_status_1() {
    // Catches: a build whose summary is missing taken for one that ran nothing locally.
    let d = rig(
        "no-summary",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", "BUILD SUCCEEDED\n"),
        ],
    );
    let o = farm_build(&d, &d.join("buck2"));
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        "BUILD SUCCEEDED\nxtask: the build printed no summary line (`Commands: N (cached: N, remote: N, local: N)`)\n"
    );
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn a_failed_build_fails_with_status_1_shows_its_log_and_still_stops_buck2() {
    // Catches: a failed build read as a pass because its summary says `local: 0`, its log (the
    // failing action's words) hidden, or the daemon left running after a failure.
    let log = format!("Action failed: test of js-types\n{FARM}\nBUILD FAILED\n");
    let d = rig(
        "failed",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", &log),
            ("buck2-build.status", "3"),
        ],
    );
    let o = farm_build(&d, &d.join("buck2"));
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert_eq!(
        stderr(&o),
        format!("{log}xtask: `buck2 build //:ci` failed (exit status: 3)\n")
    );
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn no_worker_property_set_fails_before_the_build_with_status_1() {
    // Catches: a build started with no farm to run on (buck2 would then fail at analysis, or a
    // later change could fall back to this machine): the key unset (buck2's `{}`, the stand-in's
    // answer when it has no file), set to nothing, and set to blanks.
    for (name, audit) in [
        ("unset", None),
        (
            "empty",
            Some(r#"{"buildrules_re.linux_x86_64_properties":""}"#),
        ),
        (
            "blank",
            Some(r#"{"buildrules_re.linux_x86_64_properties":"  "}"#),
        ),
    ] {
        let answers: Vec<(&str, &str)> = audit
            .map(|text| ("buck2-audit.stdout", text))
            .into_iter()
            .collect();
        let d = rig(&format!("workers-{name}"), &answers);
        let o = farm_build(&d, &d.join("buck2"));
        assert_eq!(o.status.code(), Some(1), "{name}");
        assert_eq!(stdout(&o), "", "{name}");
        assert_eq!(
            stderr(&o),
            "xtask: buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no \
             [komira_re] linux_x86_64_properties, so buck2 has no farm to build on\n",
            "{name}"
        );
        assert_eq!(calls(&d), [AUDIT, "kill"], "{name}");
    }
}

#[test]
fn a_failed_audit_fails_before_the_build_with_status_1() {
    // Catches: an audit that failed (no .buckconfig, a broken cell) read as a set key, though it
    // printed one.
    let d = rig(
        "audit-fails",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-audit.stderr", "no .buckconfig\n"),
            ("buck2-audit.status", "3"),
        ],
    );
    let o = farm_build(&d, &d.join("buck2"));
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        "xtask: `buck2 audit config` failed (exit status: 3): no .buckconfig\n"
    );
    assert_eq!(calls(&d), [AUDIT, "kill"]);
}

#[test]
fn a_buck2_that_cannot_start_fails_with_status_1_and_the_flags_are_required() {
    // Catches: a missing buck2 taken for a passing build, and a verb that runs without its
    // target or its buck2.
    let d = rig("no-buck2", &[]);
    let o = farm_build(&d, Path::new("/nonexistent/buck2"));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buck2\": "),
        "{}",
        stderr(&o)
    );
    for args in [
        &["farm-build", "--buck2", FAKE][..],
        &["farm-build", "--target", "//:ci"][..],
    ] {
        let o = xtask(&d, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains("required"), "{}", stderr(&o));
    }
    assert_eq!(calls(&d), Vec::<String>::new());
}
