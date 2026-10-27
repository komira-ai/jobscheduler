//! Hostile input to a heartbeat, a reply and a welcome: every rule has a test, and each
//! says the defect it catches.

mod common;

use common::{broken, expect, heartbeat, long, result, welcome};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{
    Heartbeat, HeldRun, HeldState, HelloReply, Outcome, ProtocolRange, Refused, Rule, RunResult,
    Taint, ValidationError, Welcome,
};

fn check(edit: impl FnOnce(&mut Heartbeat)) -> ValidationError {
    broken(&heartbeat(), edit, Heartbeat::validate)
}

fn held(id: &str) -> HeldRun {
    HeldRun {
        run_id: id.into(),
        state: HeldState::Starting,
    }
}

/// A heartbeat session secret with a character outside `[A-Za-z0-9._-]`, a leading dot or
/// more than the cap is refused as an invalid heartbeat; one at the cap and an empty one
/// (an old agent's heartbeat, which the registry refuses by name) pass `validate`. It
/// catches a megabyte or a newline-bearing secret reaching a comparison or a log.
#[test]
fn heartbeat_session_is_bounded() {
    let code = refusal::INVALID_HEARTBEAT;
    for bad in [
        "a b",
        "a\n",
        ".lead",
        "caf\u{e9}",
        long(MAX_SESSION_LEN + 1).as_str(),
    ] {
        let err = check(|b| b.session = bad.into());
        assert_eq!((err.field(), err.code()), ("session", code), "{bad:?}");
    }
    let mut ok = heartbeat();
    ok.session = long(MAX_SESSION_LEN);
    assert_eq!(ok.validate(), Ok(()));
    ok.session = String::new();
    assert_eq!(ok.validate(), Ok(()));
}

/// A welcome with no session, one with a character outside `[A-Za-z0-9._-]` and one over
/// the cap are refused as an invalid welcome; one at the cap passes. It catches an agent
/// that accepts a welcome it can never keep a session with, or holds a secret it cannot
/// send back.
#[test]
fn welcome_session_is_required_and_bounded() {
    let code = refusal::INVALID_WELCOME;
    expect(
        welcome_check(|w| w.session = String::new()),
        "session",
        Rule::Empty,
        code,
    );
    expect(
        welcome_check(|w| w.session = "a b".into()),
        "session",
        Rule::BadChars,
        code,
    );
    expect(
        welcome_check(|w| w.session = long(MAX_SESSION_LEN + 1)),
        "session",
        Rule::TooLong,
        code,
    );
    let mut ok = welcome();
    ok.session = long(MAX_SESSION_LEN);
    assert_eq!(ok.validate(), Ok(()));
}

/// A heartbeat host id that is a path, upper case, empty or a megabyte is refused. It
/// catches `A\u0000` or `../x` reaching a table or a log as a host key.
#[test]
fn heartbeat_host_id_is_bounded() {
    for id in ["", "A\u{0}", "../x", "Upper", long(1 << 20).as_str()] {
        let err = check(|b| b.host_id = id.into());
        assert_eq!(
            (err.field(), err.code()),
            ("host_id", refusal::INVALID_HEARTBEAT)
        );
    }
}

/// Held runs on a heartbeat: over the cap, a bad id and a repeated id are refused; the
/// cap passes. It catches a 100k-entry held array and a run counted twice.
#[test]
fn heartbeat_held_is_bounded_and_distinct() {
    let err = check(|b| b.held = (0..=MAX_HELD).map(|i| held(&format!("r{i}"))).collect());
    expect(err, "held", Rule::TooMany, refusal::INVALID_HEARTBEAT);
    let err = check(|b| b.held = vec![held("../x")]);
    expect(
        err,
        "held.run_id",
        Rule::BadChars,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.held = vec![held("a"), held("a")]);
    expect(err, "held", Rule::Duplicate, refusal::INVALID_HEARTBEAT);
    let mut ok = heartbeat();
    ok.held = (0..MAX_HELD).map(|i| held(&format!("r{i}"))).collect();
    assert_eq!(ok.validate(), Ok(()));
    assert_eq!(held("r").validate(), Ok(()));
    assert!(held("").validate().is_err());
}

/// Results on a heartbeat: over the cap, a bad run or lease id, a detail that is long or
/// holds a control character, and a run reported twice are refused; the caps pass. It
/// catches a 100 MB detail and a 10^7-element results array.
#[test]
fn heartbeat_results_are_bounded() {
    let mut many = Vec::new();
    for i in 0..=MAX_RESULTS {
        many.push(RunResult {
            run_id: format!("r{i}"),
            ..result()
        });
    }
    expect(
        check(|b| b.results = many),
        "results",
        Rule::TooMany,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.results[0].run_id = "../x".into());
    expect(
        err,
        "results.run_id",
        Rule::BadChars,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.results[0].lease_id.clear());
    expect(
        err,
        "results.lease_id",
        Rule::Empty,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.results[0].detail = Some(long(MAX_DETAIL_LEN + 1)));
    expect(
        err,
        "results.detail",
        Rule::TooLong,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.results[0].detail = Some("a\u{0}b".into()));
    expect(
        err,
        "results.detail",
        Rule::BadChars,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.results.push(result()));
    expect(err, "results", Rule::Duplicate, refusal::INVALID_HEARTBEAT);
    let mut ok = heartbeat();
    ok.results[0].detail = Some(long(MAX_DETAIL_LEN));
    assert_eq!(ok.validate(), Ok(()));
    assert_eq!(result().validate(), Ok(()));
    let mut bad = result();
    bad.run_id.clear();
    assert!(bad.validate().is_err());
}

/// A success with a non-zero exit code, and a failure or a failed start with exit code zero, are refused;
/// the consistent pairs and a missing code pass. It catches a result that contradicts
/// itself and would be recorded as a clean run.
#[test]
fn outcome_must_agree_with_exit_code() {
    let with = |outcome, exit_code| RunResult {
        outcome,
        exit_code,
        ..result()
    };
    for bad in [
        with(Outcome::Succeeded, Some(1)),
        with(Outcome::Succeeded, Some(-1)),
        with(Outcome::Failed, Some(0)),
        with(Outcome::FailedToStart, Some(0)),
    ] {
        expect(
            bad.validate().unwrap_err(),
            "results.exit_code",
            Rule::Inconsistent,
            refusal::INVALID_HEARTBEAT,
        );
    }
    for good in [
        with(Outcome::Succeeded, Some(0)),
        with(Outcome::Succeeded, None),
        with(Outcome::Failed, Some(2)),
        with(Outcome::Failed, None),
        with(Outcome::FailedToStart, Some(125)),
        with(Outcome::FailedToStart, None),
        with(Outcome::Killed, Some(137)),
        with(Outcome::Unknown, Some(0)),
    ] {
        assert_eq!(good.validate(), Ok(()));
    }
}

/// The taint: an empty, long or control-character reason, a bad run id and a long check
/// are refused. It catches unbounded text a server would persist.
#[test]
fn taint_is_bounded() {
    let err = check(|b| b.taint.as_mut().unwrap().reason.clear());
    expect(err, "taint.reason", Rule::Empty, refusal::INVALID_HEARTBEAT);
    let err = check(|b| b.taint.as_mut().unwrap().reason = long(MAX_TAINT_TEXT_LEN + 1));
    expect(
        err,
        "taint.reason",
        Rule::TooLong,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.taint.as_mut().unwrap().run_id = Some("../x".into()));
    expect(
        err,
        "taint.run_id",
        Rule::BadChars,
        refusal::INVALID_HEARTBEAT,
    );
    let err = check(|b| b.taint.as_mut().unwrap().check = Some("a\nb".into()));
    expect(
        err,
        "taint.check",
        Rule::BadChars,
        refusal::INVALID_HEARTBEAT,
    );
    let mut ok = heartbeat();
    ok.taint = None;
    assert_eq!(ok.validate(), Ok(()));
    let bare = Taint {
        reason: "x".into(),
        run_id: None,
        check: None,
    };
    assert_eq!(bare.validate(), Ok(()));
    assert!(
        Taint {
            reason: String::new(),
            ..bare
        }
        .validate()
        .is_err()
    );
}

fn welcome_check(edit: impl FnOnce(&mut Welcome)) -> ValidationError {
    broken(&welcome(), edit, Welcome::validate)
}

/// A welcome with a zero interval, one below the floor, an interval or grace above its
/// cap, a grace that does not exceed two intervals, a protocol the agent does not speak or a bad pool is
/// refused; the caps, the floor and the smallest valid grace pass. It catches the server reply
/// `{"heartbeat_interval_ms":1}` that would make an agent heartbeat in a busy loop.
#[test]
fn welcome_is_checked_before_the_agent_acts() {
    let code = refusal::INVALID_WELCOME;
    expect(
        welcome_check(|w| w.heartbeat_interval_ms = 0),
        "heartbeat_interval_ms",
        Rule::OutOfRange,
        code,
    );
    let err = welcome_check(|w| w.heartbeat_interval_ms = MIN_HEARTBEAT_INTERVAL_MS - 1);
    expect(err, "heartbeat_interval_ms", Rule::OutOfRange, code);
    let mut at_floor = welcome();
    at_floor.heartbeat_interval_ms = MIN_HEARTBEAT_INTERVAL_MS;
    assert_eq!(at_floor.validate(), Ok(()));
    let err = welcome_check(|w| w.heartbeat_interval_ms = MAX_HEARTBEAT_INTERVAL_MS + 1);
    expect(err, "heartbeat_interval_ms", Rule::OutOfRange, code);
    expect(
        welcome_check(|w| w.grace_ms = 0),
        "grace_ms",
        Rule::OutOfRange,
        code,
    );
    expect(
        welcome_check(|w| w.grace_ms = MAX_GRACE_MS + 1),
        "grace_ms",
        Rule::OutOfRange,
        code,
    );
    expect(
        welcome_check(|w| w.grace_ms = 10_000),
        "grace_ms",
        Rule::Inconsistent,
        code,
    );
    expect(
        welcome_check(|w| w.protocol = 0),
        "protocol",
        Rule::OutOfRange,
        code,
    );
    expect(
        welcome_check(|w| w.protocol = 2),
        "protocol",
        Rule::OutOfRange,
        code,
    );
    expect(
        welcome_check(|w| w.pool = "../x".into()),
        "pool",
        Rule::BadChars,
        code,
    );
    let mut ok = welcome();
    ok.grace_ms = 10_001;
    assert_eq!(ok.validate(), Ok(()));
    ok.heartbeat_interval_ms = MAX_HEARTBEAT_INTERVAL_MS;
    ok.grace_ms = MAX_GRACE_MS;
    assert_eq!(ok.validate(), Ok(()));
}

/// A refusal with a bad or long code, or an empty supported range, is refused; a code
/// with or without the range passes; a hello reply is validated by whichever it holds. It
/// catches a refusal that carries unbounded text.
#[test]
fn refusal_and_hello_reply_are_checked() {
    let refused = |code: &str, supported| Refused {
        code: code.into(),
        supported,
    };
    let code = refusal::INVALID_WELCOME;
    for bad in ["", "a b", "A\0"] {
        assert_eq!(refused(bad, None).validate().unwrap_err().code(), code);
    }
    expect(
        refused(&long(MAX_CODE_LEN + 1), None)
            .validate()
            .unwrap_err(),
        "code",
        Rule::TooLong,
        code,
    );
    let range = ProtocolRange { min: 0, max: 1 };
    expect(
        refused("x", Some(range)).validate().unwrap_err(),
        "supported",
        Rule::OutOfRange,
        code,
    );
    assert_eq!(refused("host-live", None).validate(), Ok(()));
    let ok_range = Some(ProtocolRange { min: 1, max: 2 });
    assert_eq!(refused("unsupported-protocol", ok_range).validate(), Ok(()));
    assert_eq!(HelloReply::Welcome(welcome()).validate(), Ok(()));
    assert_eq!(HelloReply::Refused(refused("x", None)).validate(), Ok(()));
    let mut bad = welcome();
    bad.epoch = 1;
    bad.grace_ms = 0;
    assert!(HelloReply::Welcome(bad).validate().is_err());
    assert!(HelloReply::Refused(refused("", None)).validate().is_err());
}

const SECRET: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// `{:?}` of a welcome printed the session secret, so a log line of a `Result<Welcome, _>`
/// or a panic message handed the secret to anyone who reads the log. The other fields
/// still print, and the secret does not.
#[test]
fn welcome_debug_does_not_print_the_session_secret() {
    let mut value = welcome();
    value.session = SECRET.into();
    let text = format!("{value:?}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(
        text.contains("builders") && text.contains("session: \"..\""),
        "{text}"
    );
}

/// `{:?}` of a hello reply carrying a welcome printed the session secret through the
/// derived `Debug` of the inner welcome; the reply must print the redacted form.
#[test]
fn hello_reply_debug_does_not_print_the_session_secret() {
    let mut value = welcome();
    value.session = SECRET.into();
    let text = format!("{:?}", HelloReply::Welcome(value));
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("session: \"..\""), "{text}");
}

/// `{:?}` of a heartbeat printed the session secret, so a logged heartbeat or an
/// `unwrap_err()` panic let a reader of the log keep the host live. The other fields
/// still print, and the secret does not.
#[test]
fn heartbeat_debug_does_not_print_the_session_secret() {
    let mut value = heartbeat();
    value.session = SECRET.into();
    let text = format!("{value:?}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(
        text.contains("mac-01") && text.contains("session: \"..\""),
        "{text}"
    );
}
