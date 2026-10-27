//! Hostile input to a heartbeat, a reply and a welcome: every rule has a test, and each
//! says the defect it catches.

mod common;

use common::{broken, expect, heartbeat, long, result, welcome};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldRun, HeldState, HelloReply, Outcome,
    ProtocolRange, Refused, ResultAck, Rule, RunResult, Taint, ValidationError, Welcome,
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

/// A success with a non-zero exit code, and a failure with exit code zero, are refused;
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

fn reply_check(edit: impl FnOnce(&mut HeartbeatReply)) -> ValidationError {
    broken(&reply(), edit, HeartbeatReply::validate)
}

fn reply() -> HeartbeatReply {
    HeartbeatReply {
        commands: vec![
            Command::Stop {
                run_id: "r1".into(),
            },
            Command::Unknown,
        ],
        result_acks: vec![ResultAck {
            run_id: "r1".into(),
            ack: Ack::Accepted,
        }],
        taint_ack: false,
    }
}

/// A reply with more commands or acknowledgements than the cap, a command that is invalid
/// and an acknowledgement with a bad run id are refused; the caps pass. It catches a
/// server (or an impostor) sending an agent an unbounded command list.
#[test]
fn reply_is_bounded_and_every_entry_validated() {
    let err = reply_check(|r| r.commands = vec![Command::Unknown; MAX_COMMANDS + 1]);
    expect(err, "commands", Rule::TooMany, refusal::INVALID_REPLY);
    let err = reply_check(|r| {
        r.commands.push(Command::Cancel {
            run_id: "../x".into(),
        })
    });
    expect(err, "run_id", Rule::BadChars, refusal::INVALID_REPLY);
    let ack = ResultAck {
        run_id: "r".into(),
        ack: Ack::Stale,
    };
    let err = reply_check(|r| r.result_acks = vec![ack.clone(); MAX_RESULTS + 1]);
    expect(err, "result_acks", Rule::TooMany, refusal::INVALID_REPLY);
    let err = reply_check(|r| r.result_acks[0].run_id = "a b".into());
    expect(
        err,
        "result_acks.run_id",
        Rule::BadChars,
        refusal::INVALID_REPLY,
    );
    let mut ok = reply();
    ok.commands = vec![Command::Unknown; MAX_COMMANDS];
    ok.result_acks = vec![ack; MAX_RESULTS];
    assert_eq!(ok.validate(), Ok(()));
    assert_eq!(HeartbeatReply::default().validate(), Ok(()));
    assert_eq!(
        ResultAck {
            run_id: "r".into(),
            ack: Ack::Mismatch
        }
        .validate(),
        Ok(())
    );
    assert!(
        ResultAck {
            run_id: String::new(),
            ack: Ack::Mismatch
        }
        .validate()
        .is_err()
    );
}

fn welcome_check(edit: impl FnOnce(&mut Welcome)) -> ValidationError {
    broken(&welcome(), edit, Welcome::validate)
}

/// A welcome with a zero interval, an interval or grace above its cap, a grace that does
/// not exceed two intervals, a protocol the agent does not speak or a bad pool is
/// refused; the caps and the smallest valid grace pass. It catches the server reply
/// `{"heartbeat_interval_ms":0}` that would make an agent spin.
#[test]
fn welcome_is_checked_before_the_agent_acts() {
    let code = refusal::INVALID_WELCOME;
    expect(
        welcome_check(|w| w.heartbeat_interval_ms = 0),
        "heartbeat_interval_ms",
        Rule::OutOfRange,
        code,
    );
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
