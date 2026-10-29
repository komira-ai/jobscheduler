//! Hostile input to a heartbeat, a reply and a welcome: every rule has a test, and each
//! says the defect it catches.

mod common;

use common::{broken, expect, heartbeat, long, result, welcome};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{
    ExclusiveRefusal, Heartbeat, HeldRun, HeldState, HelloReply, Outcome, ProtocolRange,
    RefusalReason, Refused, Rule, RunResult, Taint, ValidationError, Welcome,
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
        field: None,
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
    let named = |field: &str| Refused {
        field: Some(field.into()),
        ..refused(refusal::HOST_CONTRACT, None)
    };
    assert_eq!(named("gpu-toolkit").validate(), Ok(()));
    for bad in ["", "a b", "A\0"] {
        assert_eq!(named(bad).validate().unwrap_err().code(), code);
    }
    expect(
        named(&long(limits::MAX_CAPABILITY_KEY_LEN + 1))
            .validate()
            .unwrap_err(),
        "field",
        Rule::TooLong,
        code,
    );
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

fn refusal_with(lease: &str, detail: &str) -> ExclusiveRefusal {
    ExclusiveRefusal {
        lease_id: lease.into(),
        code: RefusalReason::NotQuiet,
        detail: detail.into(),
    }
}

fn refused_check(refusal: ExclusiveRefusal) -> ValidationError {
    check(|b| b.exclusive_refused = Some(refusal))
}

/// `validate` (a sender's own check) holds the refusal detail to its cap in bytes: 256
/// pass, 257 fail, and a multi-byte character that straddles the cap makes the text over
/// it. It catches a character count in place of a byte count.
#[test]
fn refusal_detail_is_bounded_in_bytes() {
    assert_eq!(MAX_REFUSAL_DETAIL, 256);
    let ok = |detail: String| {
        let mut b = heartbeat();
        b.exclusive_refused = Some(refusal_with("1-1", &detail));
        b.validate()
    };
    assert_eq!(ok(String::new()), Ok(()));
    assert_eq!(ok(long(MAX_REFUSAL_DETAIL - 1)), Ok(()));
    assert_eq!(ok(long(MAX_REFUSAL_DETAIL)), Ok(()));
    let above = refused_check(refusal_with("1-1", &long(MAX_REFUSAL_DETAIL + 1)));
    expect(
        above,
        "exclusive_refused.detail",
        Rule::TooLong,
        refusal::INVALID_HEARTBEAT,
    );
    // 255 ASCII bytes and a two-byte character end at byte 257.
    let straddle = format!("{}\u{e9}", long(MAX_REFUSAL_DETAIL - 1));
    assert_eq!(straddle.len(), MAX_REFUSAL_DETAIL + 1);
    let err = refused_check(refusal_with("1-1", &straddle));
    expect(
        err,
        "exclusive_refused.detail",
        Rule::TooLong,
        refusal::INVALID_HEARTBEAT,
    );
    // The same character ending exactly at the cap passes.
    let fits = format!("{}\u{e9}", long(MAX_REFUSAL_DETAIL - 2));
    assert_eq!(ok(fits), Ok(()));
}

/// A control or format character in the refusal detail is refused. It catches a detail
/// that could forge a log line or reorder the text around it.
#[test]
fn refusal_detail_refuses_hidden_characters() {
    for bad in ["a\nb", "a\u{0}b", "a\u{1b}[31m", "a\u{202e}b", "a\u{200b}b"] {
        let err = refused_check(refusal_with("1-1", bad));
        expect(
            err,
            "exclusive_refused.detail",
            Rule::BadChars,
            refusal::INVALID_HEARTBEAT,
        );
    }
}

/// Lease ids of both reports use the id alphabet and length; an empty one is refused.
/// It catches a lease id outside the alphabet reaching the scheduler's records.
#[test]
fn exclusive_lease_ids_are_bounded() {
    let good = [long(1), long(MAX_ID_LEN), "7-1".to_string()];
    for id in &good {
        let mut b = heartbeat();
        b.exclusive_ready = Some(id.clone());
        assert_eq!(b.validate(), Ok(()));
        let mut b = heartbeat();
        b.exclusive_refused = Some(refusal_with(id, "x"));
        assert_eq!(b.validate(), Ok(()));
    }
    let bad = [
        ("", Rule::Empty),
        ("../x", Rule::BadChars),
        (".a", Rule::BadChars),
        (&long(MAX_ID_LEN + 1), Rule::TooLong),
    ];
    for (id, rule) in bad {
        let err = check(|b| b.exclusive_ready = Some(id.to_string()));
        expect(err, "exclusive_ready", rule, refusal::INVALID_HEARTBEAT);
        let err = refused_check(refusal_with(id, "x"));
        expect(
            err,
            "exclusive_refused.lease_id",
            rule,
            refusal::INVALID_HEARTBEAT,
        );
    }
}

/// A heartbeat that reports ready and refused at once is refused, whichever lease each
/// names. It catches the scheduler being given two contradictory reports.
#[test]
fn ready_and_refused_together_are_refused() {
    for ready in ["1-1", "2-2"] {
        let err = check(|b| {
            b.exclusive_ready = Some(ready.into());
            b.exclusive_refused = Some(refusal_with("1-1", "x"));
        });
        expect(
            err,
            "exclusive_refused",
            Rule::Inconsistent,
            refusal::INVALID_HEARTBEAT,
        );
    }
}

/// No error text of the exclusive fields repeats what the peer sent. It catches an
/// error built from the value.
#[test]
fn exclusive_errors_do_not_repeat_peer_input() {
    let marker = "PEER-MARKER-\u{1b}";
    let errors = [
        check(|b| b.exclusive_ready = Some(marker.into())),
        refused_check(refusal_with(marker, "x")),
        refused_check(refusal_with("1-1", marker)),
        refused_check(refusal_with("1-1", &format!("PEER-MARKER-{}", long(300)))),
        check(|b| {
            b.exclusive_ready = Some("PEER-MARKER-1".into());
            b.exclusive_refused = Some(refusal_with("PEER-MARKER-1", "x"));
        }),
    ];
    for err in errors {
        let text = format!("{err} {err:?} {}", err.code());
        assert!(!text.contains("PEER-MARKER"), "{text}");
    }
}

/// The exclusive reports show in the heartbeat's Debug text while the session stays
/// hidden. It catches the new fields dropped from Debug, or the secret leaking with them.
#[test]
fn heartbeat_debug_shows_exclusive_and_hides_session() {
    let mut value = heartbeat();
    value.session = SECRET.into();
    value.exclusive_ready = Some("lease-ready-7".into());
    value.exclusive_refused = Some(refusal_with("lease-refused-8", "why"));
    let text = format!("{value:?}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(
        text.contains("lease-ready-7") && text.contains("lease-refused-8"),
        "{text}"
    );
}

/// A refusal checked on its own carries the heartbeat code and names the field it
/// broke. It catches the public check being dropped from a refusal read outside a
/// heartbeat.
#[test]
fn a_single_refusal_is_checked() {
    assert_eq!(refusal_with("1-1", "x").validate(), Ok(()));
    let err = refusal_with("", "x").validate().unwrap_err();
    expect(
        err,
        "exclusive_refused.lease_id",
        Rule::Empty,
        refusal::INVALID_HEARTBEAT,
    );
    let err = refusal_with("1-1", "a\nb").validate().unwrap_err();
    expect(
        err,
        "exclusive_refused.detail",
        Rule::BadChars,
        refusal::INVALID_HEARTBEAT,
    );
}

fn decode_with_detail(detail: &str) -> Heartbeat {
    let mut b = heartbeat();
    b.exclusive_refused = Some(refusal_with("1-1", "x"));
    let mut value = serde_json::to_value(&b).unwrap();
    value["exclusive_refused"]["detail"] = detail.into();
    serde_json::from_value(value).unwrap()
}

/// A refusal detail of exactly the cap decodes verbatim. It catches a cut that is off by
/// one.
#[test]
fn decoded_refusal_detail_at_the_cap_is_kept() {
    let detail = long(MAX_REFUSAL_DETAIL);
    let got = decode_with_detail(&detail);
    assert_eq!(got.exclusive_refused.unwrap().detail, detail);
}

/// A refusal detail over the cap is cut on decode to the longest prefix within the cap
/// that ends on a character boundary, and the rest of the heartbeat survives: the code,
/// the results and the held list. A two-byte character straddling the cap is dropped
/// whole. It catches a refusal of the whole heartbeat, a cut inside a character, and a
/// cut that loses the rest of the beat.
#[test]
fn decoded_refusal_detail_over_the_cap_is_cut_and_the_beat_survives() {
    let over = long(MAX_REFUSAL_DETAIL + 50);
    let got = decode_with_detail(&over);
    let refusal = got.exclusive_refused.clone().unwrap();
    assert_eq!(refusal.detail, over[..MAX_REFUSAL_DETAIL]);
    assert_eq!(refusal.code, RefusalReason::NotQuiet);
    assert_eq!(got.results, heartbeat().results);
    assert_eq!(got.held, heartbeat().held);
    assert_eq!(got.validate(), Ok(()));

    let straddle = format!("{}\u{e9}tail", long(MAX_REFUSAL_DETAIL - 1));
    let cut = decode_with_detail(&straddle)
        .exclusive_refused
        .unwrap()
        .detail;
    assert_eq!(cut, long(MAX_REFUSAL_DETAIL - 1));

    let fits = format!("{}\u{e9}tail", long(MAX_REFUSAL_DETAIL - 2));
    let cut = decode_with_detail(&fits).exclusive_refused.unwrap().detail;
    assert_eq!(cut.len(), MAX_REFUSAL_DETAIL);
    assert!(cut.ends_with('\u{e9}'));
}
