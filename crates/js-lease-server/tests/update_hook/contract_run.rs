//! The rest of the hook's contract with the updater: the record's rules, the reason tokens, the
//! verb's `run` and the exit codes with their last lines.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use js_lease_server::update_hook::{
    DEFAULT_UNIT, Done, Host, Invalid, Invocation, Outcome, Params, Reason, Record, Report,
    Rollback, SCHEMA, Schema, Step, StoreUnreachable, is_verb, run, validate,
};
use js_testkit::dir::TestDir;
use js_update_hook::{ReasonToken, is_reason_token};

use crate::support::{Rig, free, held};

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn code_of(code: ExitCode) -> String {
    format!("{code:?}")
}

fn exit(n: u8) -> String {
    code_of(ExitCode::from(n))
}

// ---------------------------------------------------------------------------------------
// record.rs

/// Asserts a record `prepare` wrote for the params validates, and the texts of both `Invalid`
/// rules.
#[test]
fn a_record_of_this_update_is_valid() {
    let rig = Rig::new("contract-record-valid");
    let record = rig.prepared(Some("old"));
    assert_eq!(validate(&record, &rig.declaration, &rig.params), Ok(()));
    assert_eq!(
        Invalid::Unit("x".to_owned()).to_string(),
        "the record's unit \"x\" is not the declared unit named by --unit"
    );
    assert_eq!(
        Invalid::OtherUpdate.to_string(),
        "the record is of another update than the params table's"
    );
}

/// Asserts a record whose unit is not the declaration's, or not the params' (`--unit`), is
/// `Unit` carrying the record's unit, even when the update also differs.
#[test]
fn a_record_of_another_unit_is_refused() {
    let mut rig = Rig::new("contract-record-unit");
    let record = rig.prepared(None);
    rig.declaration.unit = "other.service".to_owned();
    assert_eq!(
        validate(&record, &rig.declaration, &rig.params),
        Err(Invalid::Unit(record.unit.clone()))
    );
    rig.declaration.unit = record.unit.clone();
    rig.params.unit = "flag.service".to_owned();
    assert_eq!(
        validate(&record, &rig.declaration, &rig.params),
        Err(Invalid::Unit(record.unit.clone()))
    );
    rig.params.unit = record.unit.clone();
    let mut both = record.clone();
    both.unit = "third.service".to_owned();
    both.target = "9.9.9".to_owned();
    assert_eq!(
        validate(&both, &rig.declaration, &rig.params),
        Err(Invalid::Unit("third.service".to_owned()))
    );
}

/// Asserts a record whose staged release, target version or previous release differs from the
/// params' is `OtherUpdate`, including a record with a previous release for a first install and
/// the other way round.
///
/// Catches a step acting on a record left by another update.
#[test]
fn a_record_of_another_update_is_refused() {
    let rig = Rig::new("contract-record-update");
    let good = rig.prepared(None);

    let mut staged = good.clone();
    staged.staged = rig.path("releases/3.0.0");
    let mut target = good.clone();
    target.target = "3.0.0".to_owned();
    let mut previous = good.clone();
    previous.previous_target = Some(rig.path("releases/0.9.0"));
    let mut none_for_some = good.clone();
    none_for_some.previous_target = None;
    for record in [staged, target, previous, none_for_some] {
        assert_eq!(
            validate(&record, &rig.declaration, &rig.params),
            Err(Invalid::OtherUpdate)
        );
    }
    let first = Rig::first_install("contract-record-first");
    let mut with_previous = first.prepared(None);
    with_previous.previous_target = Some(first.previous());
    assert_eq!(
        validate(&with_previous, &first.declaration, &first.params),
        Err(Invalid::OtherUpdate)
    );
    assert_eq!(
        validate(&first.prepared(None), &first.declaration, &first.params),
        Ok(())
    );
}

/// Asserts `Record::new` copies the update from the params with every mark clear, a record
/// round-trips through JSON, and an unknown field or a missing one is refused.
#[test]
fn a_record_is_the_update_with_no_marks_and_round_trips() {
    let rig = Rig::new("contract-record-new");
    let record = Record::new(&rig.params, Some(rig.previous()));
    assert_eq!(record.schema, SCHEMA);
    assert_eq!(SCHEMA, 1);
    assert_eq!(record.target, "2.0.0");
    assert_eq!(record.previous.as_deref(), Some("1.0.0"));
    assert_eq!(record.staged, rig.staged());
    assert_eq!(record.previous_target, Some(rig.previous()));
    assert_eq!(record.unit, DEFAULT_UNIT);
    assert_eq!(record.holder, None);
    assert_eq!(record.store_schema, None);
    assert!(!record.stop_begun && !record.link_moved && !record.start_issued && !record.verify_ok);
    assert_eq!(record.rb, Rollback::default());
    assert_eq!(record.rb_holder, None);

    let json = serde_json::to_string(&record).unwrap();
    assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), record);
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    value["surprise"] = true.into();
    assert!(serde_json::from_value::<Record>(value.clone()).is_err());
    value.as_object_mut().unwrap().remove("surprise");
    value.as_object_mut().unwrap().remove("target");
    assert!(serde_json::from_value::<Record>(value).is_err());
    let rb = serde_json::json!({"stop_begun": true, "link_restored": false, "start_issued": false, "serving_ok": false, "extra": 1});
    assert!(serde_json::from_value::<Rollback>(rb).is_err());
}

// ---------------------------------------------------------------------------------------
// report.rs

fn reason_index(reason: Reason) -> usize {
    match reason {
        Reason::BadArgs => 0,
        Reason::BadParams => 1,
        Reason::Locked => 2,
        Reason::StateInvalid => 3,
        Reason::StoreUnreachable => 4,
        Reason::SchemaUnsupported => 5,
        Reason::StopFailed => 6,
        Reason::LockNotReleased => 7,
        Reason::LinkFailed => 8,
        Reason::StartFailed => 9,
        Reason::NoLock => 10,
        Reason::NotServing => 11,
        Reason::NoPreviousRelease => 12,
        Reason::RollbackNotServing => 13,
        Reason::NotVerified => 14,
    }
}

/// Asserts every reason in `Reason::ALL` is a token the updater accepts, the tokens are
/// distinct, `ALL` lists every variant once and in order (the match above has no wildcard, so a
/// new variant stops this compiling), and the trait's token is the inherent one.
#[test]
fn every_reason_is_a_valid_distinct_token() {
    let mut seen = std::collections::BTreeSet::new();
    for (i, reason) in Reason::ALL.into_iter().enumerate() {
        assert_eq!(reason_index(reason), i, "{reason:?} is out of order");
        let token = reason.as_str();
        assert!(is_reason_token(token), "{token}");
        assert!(seen.insert(token), "{token} is given twice");
        assert_eq!(ReasonToken::as_str(reason), token);
    }
    assert_eq!(seen.len(), 15);
    assert_eq!(
        Reason::ALL.map(Reason::as_str),
        [
            "bad_args",
            "bad_params",
            "locked",
            "state_invalid",
            "store_unreachable",
            "schema_unsupported",
            "stop_failed",
            "lock_not_released",
            "link_failed",
            "start_failed",
            "no_lock",
            "not_serving",
            "no_previous_release",
            "rollback_not_serving",
            "not_verified",
        ]
    );
}

// ---------------------------------------------------------------------------------------
// mod.rs

/// A writer that fails every write: a closed standard output.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

struct Ran {
    code: String,
    out: String,
    err: String,
}

fn invoke(words: &[&str], step: &dyn Fn(&Invocation, &Params) -> Done) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&os(words), &mut out, &mut err, &Host { step });
    Ran {
        code: code_of(code),
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn never(_: &Invocation, _: &Params) -> Done {
    panic!("no step should have run");
}

fn succeeded() -> Done {
    Done {
        outcome: Outcome::Success,
        report: Report::default(),
    }
}

fn params_file(dir: &Path, staged: &str) -> PathBuf {
    let file = dir.join("params.toml");
    fs::write(
        &file,
        format!(
            "\"updater.staged_release\" = \"{staged}\"\n\"updater.target_version\" = \"2.0.0\"\ndefer_retry_secs = 60\n"
        ),
    )
    .unwrap();
    file
}

/// Asserts a refused command line exits 1 without running a step, with the refusal on stderr
/// prefixed `js-lease-server update-hook: ` and the last line on stdout a `bad_args` failure
/// carrying the same words.
#[test]
fn a_refused_command_line_exits_1_with_bad_args() {
    let got = invoke(&["js-lease-server", "update-hook", "frob"], &never);
    assert_eq!(got.code, exit(1));
    assert_eq!(
        got.err,
        "js-lease-server update-hook: unknown step \"frob\"\n"
    );
    let line = Report::failure(Reason::BadArgs, "unknown step \"frob\"").to_line();
    assert_eq!(got.out, format!("{line}\n"));
    let parsed: serde_json::Value = serde_json::from_str(line.as_str()).unwrap();
    assert_eq!(parsed["reason"], "bad_args");
    assert_eq!(parsed["class"], "release");
}

/// Asserts a params file that cannot be read or is wrong exits 1 as `bad_params`, with the
/// exact words on both streams, and no step runs.
#[test]
fn an_unreadable_params_file_exits_1_with_bad_params() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-run-badparams");
    let missing = dir.path().join("absent.toml");
    let os_words = fs::read_to_string(&missing).unwrap_err().to_string();
    let missing_text = missing.to_str().unwrap();
    let got = invoke(
        &[
            "js-lease-server",
            "update-hook",
            "cleanup",
            "--params",
            missing_text,
        ],
        &never,
    );
    let detail = format!("cannot read the params file: {os_words}");
    assert_eq!(got.code, exit(1));
    assert_eq!(got.err, format!("js-lease-server update-hook: {detail}\n"));
    assert_eq!(
        got.out,
        format!("{}\n", Report::failure(Reason::BadParams, detail).to_line())
    );

    let wrong = dir.path().join("wrong.toml");
    fs::write(&wrong, "defer_retry_secs = 1\n").unwrap();
    let got = invoke(
        &[
            "js-lease-server",
            "update-hook",
            "cleanup",
            "--params",
            wrong.to_str().unwrap(),
        ],
        &never,
    );
    let detail = "params key \"updater.staged_release\" is required";
    assert_eq!(got.err, format!("js-lease-server update-hook: {detail}\n"));
    assert_eq!(
        got.out,
        format!("{}\n", Report::failure(Reason::BadParams, detail).to_line())
    );
}

/// Asserts `prepare` given a directory other than the table's staged release exits 1 as
/// `bad_args` with the exact words, and no step runs; given the same directory it reaches the
/// step.
///
/// Catches a hook that acts on a tree the updater did not verify.
#[test]
fn a_prepare_artifact_that_is_not_the_staged_release_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-run-artifact");
    let params = params_file(dir.path(), "/staged/2.0.0");
    let params = params.to_str().unwrap();
    let got = invoke(
        &[
            "js-lease-server",
            "update-hook",
            "prepare",
            "/other/2.0.0",
            "--params",
            params,
        ],
        &never,
    );
    let detail = "the artifact \"/other/2.0.0\" is not the staged release updater.staged_release names (\"/staged/2.0.0\")";
    assert_eq!(got.code, exit(1));
    assert_eq!(got.err, format!("js-lease-server update-hook: {detail}\n"));
    assert_eq!(
        got.out,
        format!("{}\n", Report::failure(Reason::BadArgs, detail).to_line())
    );

    let reached = Cell::new(false);
    let ok = invoke(
        &[
            "js-lease-server",
            "update-hook",
            "prepare",
            "/staged/2.0.0",
            "--params",
            params,
        ],
        &|_, _| {
            reached.set(true);
            succeeded()
        },
    );
    assert!(reached.get());
    assert_eq!(ok.code, exit(0));
    assert_eq!(ok.err, "");
}

/// Asserts a good invocation reaches the host's step with the parsed invocation and the params
/// the file holds with the flags put in (release link and unit), and with no flags the default
/// unit and no link; the step's report is the stdout line.
#[test]
fn a_good_invocation_reaches_the_step_with_the_flags_in_the_params() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-run-good");
    let params = params_file(dir.path(), "/staged/2.0.0");
    let params = params.to_str().unwrap();
    let seen: RefCell<Vec<(Invocation, Params)>> = RefCell::default();
    let step = |inv: &Invocation, p: &Params| {
        seen.borrow_mut().push((inv.clone(), p.clone()));
        let mut done = succeeded();
        done.report.detail = Some("hello".to_owned());
        done
    };
    let got = invoke(
        &[
            "js-lease-server",
            "update-hook",
            "--release-link=/opt/link",
            "--unit=u.service",
            "--store=/opt/store",
            "--healthz-url=http://127.0.0.1:99",
            "verify",
            "--params",
            params,
        ],
        &step,
    );
    let plain = invoke(
        &[
            "js-lease-server",
            "update-hook",
            "verify",
            "--params",
            params,
        ],
        &step,
    );
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 2);
    let (inv, p) = &seen[0];
    assert_eq!(inv.step, Step::Verify);
    assert_eq!(inv.flags.store, Some(PathBuf::from("/opt/store")));
    assert_eq!(inv.flags.healthz, Some("127.0.0.1:99".parse().unwrap()));
    assert_eq!(p.release_link, Some(PathBuf::from("/opt/link")));
    assert_eq!(p.unit, "u.service");
    assert_eq!(p.target_version, "2.0.0");
    assert_eq!(p.defer_retry_secs, 60);
    assert_eq!(seen[1].1.release_link, None);
    assert_eq!(seen[1].1.unit, DEFAULT_UNIT);
    let report = Report {
        detail: Some("hello".to_owned()),
        ..Report::default()
    };
    assert_eq!(got.out, format!("{}\n", report.to_line()));
    assert_eq!(plain.out, got.out);
    assert_eq!(got.err, "");
}

/// Asserts each outcome a step returns is the process's exit status: 0, 1, 75 and 76.
#[test]
fn the_exit_status_is_the_steps_outcome() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-run-status");
    let params = params_file(dir.path(), "/staged/2.0.0");
    let params = params.to_str().unwrap();
    for (outcome, n) in [
        (Outcome::Success, 0),
        (Outcome::Failure, 1),
        (Outcome::Deferred, 75),
        (Outcome::Halt, 76),
    ] {
        let got = invoke(
            &[
                "js-lease-server",
                "update-hook",
                "cleanup",
                "--params",
                params,
            ],
            &|_, _| Done {
                outcome,
                report: Report::default(),
            },
        );
        assert_eq!(got.code, exit(n), "{outcome:?}");
    }
}

/// Asserts `is_verb` is true only when the second word is `update-hook`: false for another
/// verb, for no arguments, for the program name alone, and for the verb in first place.
#[test]
fn only_the_second_word_selects_the_verb() {
    assert!(is_verb(&os(&["js-lease-server", "update-hook"])));
    assert!(is_verb(&os(&["x", "update-hook", "cleanup"])));
    assert!(!is_verb(&os(&["js-lease-server", "--listen"])));
    assert!(!is_verb(&os(&["js-lease-server", "update-hooks"])));
    assert!(!is_verb(&os(&["js-lease-server", ""])));
    assert!(!is_verb(&[]));
    assert!(!is_verb(&os(&["js-lease-server"])));
    assert!(!is_verb(&os(&["update-hook"])));
    assert!(!is_verb(&os(&["update-hook", "js-lease-server"])));
}

/// Asserts a closed standard output and a closed standard error change nothing about the exit
/// status: a refusal still exits 1, and a halt from the step still exits 76.
#[test]
fn a_closed_stdout_still_gives_the_exit_code() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-run-closed");
    let params = params_file(dir.path(), "/staged/2.0.0");
    let params = params.to_str().unwrap();
    let refused = run(
        &os(&["js-lease-server", "update-hook"]),
        &mut Closed,
        &mut Closed,
        &Host { step: &never },
    );
    assert_eq!(code_of(refused), exit(1));
    let halted = run(
        &os(&[
            "js-lease-server",
            "update-hook",
            "cleanup",
            "--params",
            params,
        ]),
        &mut Closed,
        &mut Vec::new(),
        &Host {
            step: &|_, _| Done {
                outcome: Outcome::Halt,
                report: Report::default(),
            },
        },
    );
    assert_eq!(code_of(halted), exit(76));
}

/// Asserts the library's own `run` answers the verb before the server's flags are parsed:
/// `update-hook` alone exits 1 with `bad_args` "no step given", the refusal on stderr, and no
/// usage text from the server's flag parser on either stream.
#[test]
fn the_server_answers_the_verb_before_its_flags_are_parsed() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_lease_server::run(os(&["js-lease-server", "update-hook"]), &mut out, &mut err);
    let (out, err) = (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    );
    assert_eq!(code_of(code), exit(1));
    assert_eq!(err, "js-lease-server update-hook: no step given\n");
    assert_eq!(
        out,
        format!(
            "{}\n",
            Report::failure(Reason::BadArgs, "no step given").to_line()
        )
    );
    assert!(!out.contains("Usage") && !err.contains("Usage"));
}

// ---------------------------------------------------------------------------------------
// the exit codes

struct Row {
    name: &'static str,
    step: Step,
    setup: fn(&Rig),
}

/// A record as cutover leaves it after its start, with the link on the staged release.
fn started(rig: &Rig) -> Record {
    let mut record = rig.prepared(Some("old"));
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    rig.put(&record);
    *rig.link.target.borrow_mut() = Some(rig.staged());
    record
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            name: "prepare_ok",
            step: Step::Prepare,
            setup: |_| {},
        },
        Row {
            name: "prepare_store_unreachable",
            step: Step::Prepare,
            setup: |rig| *rig.store.schema.borrow_mut() = Err(StoreUnreachable),
        },
        Row {
            name: "prepare_schema_missing",
            step: Step::Prepare,
            setup: |rig| *rig.store.schema.borrow_mut() = Ok(Schema::Missing),
        },
        Row {
            name: "prepare_link_outside",
            step: Step::Prepare,
            setup: |rig| *rig.link.target.borrow_mut() = Some(rig.path("elsewhere/1.0.0")),
        },
        Row {
            name: "prepare_after_cutover_began",
            step: Step::Prepare,
            setup: |rig| {
                let mut record = rig.prepared(Some("old"));
                record.stop_begun = true;
                rig.put(&record);
            },
        },
        Row {
            name: "cutover_without_record",
            step: Step::Cutover,
            setup: |_| {},
        },
        Row {
            name: "cutover_ok",
            step: Step::Cutover,
            setup: |rig| {
                assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
                rig.store.answer([free()]);
            },
        },
        Row {
            name: "cutover_lock_held",
            step: Step::Cutover,
            setup: |rig| assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success),
        },
        Row {
            name: "verify_ok",
            step: Step::Verify,
            setup: |rig| {
                started(rig);
                rig.store.answer([held("new")]);
            },
        },
        Row {
            name: "verify_no_new_lock",
            step: Step::Verify,
            setup: |rig| {
                started(rig);
            },
        },
        Row {
            name: "verify_not_serving",
            step: Step::Verify,
            setup: |rig| {
                started(rig);
                rig.store.answer([held("new")]);
                *rig.health.answers.borrow_mut() = [Ok(503)].into();
            },
        },
        Row {
            name: "verify_unit_died",
            step: Step::Verify,
            setup: |rig| {
                started(rig);
                rig.units.set("inactive", "dead");
            },
        },
        Row {
            name: "rollback_never_stopped",
            step: Step::Rollback,
            setup: |rig| rig.put(&rig.prepared(Some("old"))),
        },
        Row {
            name: "rollback_ok",
            step: Step::Rollback,
            setup: |rig| {
                started(rig);
                rig.store.answer([held("old"), free(), held("new")]);
            },
        },
        Row {
            name: "rollback_unknown_release",
            step: Step::Rollback,
            setup: |rig| {
                started(rig);
                *rig.link.target.borrow_mut() = Some(rig.path("releases/9.9.9"));
            },
        },
        Row {
            name: "cleanup_not_verified",
            step: Step::Cleanup,
            setup: |rig| rig.put(&rig.prepared(Some("old"))),
        },
        Row {
            name: "cleanup_ok",
            step: Step::Cleanup,
            setup: |rig| {
                let mut record = started(rig);
                record.verify_ok = true;
                rig.put(&record);
            },
        },
        Row {
            name: "cleanup_after_cleanup",
            step: Step::Cleanup,
            setup: |_| {},
        },
    ]
}

/// A first install whose link was moved, rolled back: there is no release to restore.
fn first_install_row() -> String {
    let rig = Rig::first_install("contract-exit-first-install");
    let mut record = rig.prepared(None);
    record.stop_begun = true;
    record.link_moved = true;
    rig.put(&record);
    line_of(&rig, "rollback_first_install_moved", Step::Rollback)
}

fn line_of(rig: &Rig, name: &str, step: Step) -> String {
    let done = rig.run(step);
    let root = rig.dir.path().to_str().unwrap();
    let line = done.report.to_line().replace(root, "<root>");
    format!("{name} {step} {} {line}\n", done.outcome.code())
}

/// Asserts the five steps, run on the rig, end with the exit codes 0, 1, 75 and 76 and the
/// last lines the golden file `golden/exit_codes.txt` pins by exact equality (the rig's root
/// directory shown as `<root>`). The test never writes the golden: a difference fails showing
/// both texts.
///
/// Catches an exit code, a reason token, a class, a `reversible` flag or a detail that changes
/// without the contract changing with it.
#[test]
fn exit_codes_are_the_contract() {
    let mut actual = String::new();
    for row in rows() {
        let rig = Rig::new(&format!("contract-exit-{}", row.name));
        (row.setup)(&rig);
        actual.push_str(&line_of(&rig, row.name, row.step));
    }
    actual.push_str(&first_install_row());
    let codes: std::collections::BTreeSet<&str> = actual
        .lines()
        .filter_map(|line| line.split(' ').nth(2))
        .collect();
    assert_eq!(
        codes.into_iter().collect::<Vec<_>>(),
        ["0", "1", "75", "76"]
    );
    let golden = include_str!("golden/exit_codes.txt");
    assert_eq!(
        actual, golden,
        "the exit codes and last lines differ from golden/exit_codes.txt\n--- actual ---\n{actual}--- golden ---\n{golden}"
    );
}
