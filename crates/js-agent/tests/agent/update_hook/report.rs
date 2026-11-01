//! The last line of standard output: its shape, byte for byte, and its tokens.

use js_agent::update_hook::{Class, MAX_REASON_LEN, Reason, Report, is_reason_token};

#[test]
fn the_report_of_a_step_that_says_nothing_is_reversible_and_a_release_matter() {
    let report = Report::default();
    assert!(report.reversible);
    assert_eq!(report.class, Class::Release);
    assert_eq!(
        report.to_line(),
        r#"{"class":"release","promotion_pending":false,"reversible":true}"#
    );
}

#[test]
fn a_failure_carries_its_reason_and_text() {
    let report = Report::failure(Reason::NotImplemented, "prepare is not implemented");
    assert_eq!(
        report.to_line(),
        concat!(
            r#"{"class":"release","detail":"prepare is not implemented","#,
            r#""promotion_pending":false,"reason":"not_implemented","reversible":true}"#
        )
    );
}

#[test]
fn every_field_is_written_when_set() {
    let report = Report {
        class: Class::Environment,
        reason: Some(Reason::BadArgs),
        retry_after_secs: Some(300),
        reversible: false,
        promotion_pending: true,
        detail: Some("a \"quoted\"\nline".to_owned()),
    };
    assert_eq!(
        report.to_line(),
        concat!(
            r#"{"class":"environment","detail":"a \"quoted\"\nline","#,
            r#""promotion_pending":true,"reason":"bad_args","#,
            r#""retry_after_secs":300,"reversible":false}"#
        )
    );
}

#[test]
fn each_optional_field_is_absent_until_set() {
    let only_retry = Report {
        retry_after_secs: Some(0),
        ..Report::default()
    };
    assert_eq!(
        only_retry.to_line(),
        r#"{"class":"release","promotion_pending":false,"retry_after_secs":0,"reversible":true}"#
    );
    let only_reason = Report {
        reason: Some(Reason::BadParams),
        ..Report::default()
    };
    assert_eq!(
        only_reason.to_line(),
        r#"{"class":"release","promotion_pending":false,"reason":"bad_params","reversible":true}"#
    );
    let irreversible = Report {
        reversible: false,
        ..Report::default()
    };
    assert_eq!(
        irreversible.to_line(),
        r#"{"class":"release","promotion_pending":false,"reversible":false}"#
    );
}

#[test]
fn the_line_is_one_object_on_one_line() {
    let mut out = Vec::new();
    Report::failure(Reason::BadArgs, "two\nlines")
        .write_to(&mut out)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.ends_with("}\n"));
    assert_eq!(text.trim_end().lines().count(), 1);
    let object: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    assert_eq!(object["detail"], "two\nlines");
}

#[test]
fn class_words() {
    assert_eq!(Class::Release.as_str(), "release");
    assert_eq!(Class::Environment.as_str(), "environment");
    assert_eq!(Class::default(), Class::Release);
}

#[test]
fn reason_tokens_are_lowercase_words_of_at_most_64_bytes() {
    let longest = "a".repeat(MAX_REASON_LEN);
    for good in ["a", "state_invalid", "x9_", "0", longest.as_str()] {
        assert!(is_reason_token(good), "{good:?}");
    }
    let too_long = "a".repeat(MAX_REASON_LEN + 1);
    for bad in [
        "",
        too_long.as_str(),
        "Upper",
        "with-dash",
        "with space",
        "dot.ted",
        "caf\u{e9}",
        "new\nline",
    ] {
        assert!(!is_reason_token(bad), "{bad:?}");
    }
}

#[test]
fn every_reason_the_hook_gives_is_a_token_and_they_differ() {
    let mut seen = std::collections::BTreeSet::new();
    for reason in Reason::ALL {
        assert!(is_reason_token(reason.as_str()), "{reason:?}");
        assert!(seen.insert(reason.as_str()), "{reason:?} repeated");
    }
}

#[test]
fn the_reasons_of_cutover_are_these_exact_tokens() {
    assert_eq!(Reason::StopFailed.as_str(), "stop_failed");
    assert_eq!(Reason::LinkFailed.as_str(), "link_failed");
    assert_eq!(Reason::StartFailed.as_str(), "start_failed");
}

#[test]
fn the_reason_of_an_unconfirmed_cordon_is_this_exact_token() {
    assert_eq!(Reason::CordonUnconfirmed.as_str(), "cordon_unconfirmed");
}

#[test]
fn the_reason_of_a_verify_that_heard_no_hello_is_this_exact_token() {
    assert_eq!(Reason::NoHello.as_str(), "no_hello");
}
