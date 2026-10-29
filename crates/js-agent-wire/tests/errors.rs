//! Errors never echo what a peer sent.

mod common;

use common::{broken, hello, job, long};
use js_agent_wire::{Hello, JobSpec, Mount, Rule, ValidationError};

fn short(err: &ValidationError) {
    let text = err.to_string();
    assert!(text.len() < 100, "{} bytes", text.len());
    assert!(!text.contains("aaaa"), "{text}");
}

/// A megabyte in an id, an image, a label key, a mount path, a root name and a label
/// value gives a short fixed `Display`, not an echo, and the stable `code()` is
/// unchanged. It catches an error built with `{value:?}` that would put a megabyte in a
/// log line or a reply.
#[test]
fn display_never_echoes_a_megabyte() {
    let big = long(1 << 20);
    let errors = [
        broken(&hello(), |h| h.host_id = big.clone(), Hello::validate),
        broken(&hello(), |h| h.pool = big.clone(), Hello::validate),
        broken(&hello(), |h| h.agent_version = big.clone(), Hello::validate),
        broken(
            &hello(),
            |h| drop(h.labels.insert(big.clone(), "v".into())),
            Hello::validate,
        ),
        broken(&job(), |j| j.run_id = big.clone(), JobSpec::validate),
        broken(&job(), |j| j.image = big.clone(), JobSpec::validate),
        broken(&job(), |j| j.args = vec![big.clone()], JobSpec::validate),
        broken(
            &job(),
            |j| drop(j.labels.insert("k".into(), big.clone())),
            JobSpec::validate,
        ),
        broken(&job(), |j| j.mounts[0].rel = big.clone(), JobSpec::validate),
        broken(
            &job(),
            |j| j.mounts[0].root_name = big.clone(),
            JobSpec::validate,
        ),
    ];
    for err in &errors {
        short(err);
        assert!(err.code().len() < 40);
    }
    let rel = Mount {
        root_name: "cache".into(),
        rel: format!("../{big}"),
        ro: false,
    };
    short(&rel.validate().unwrap_err());
}

/// `Display` is the field name and the rule's fixed text, and each rule has distinct
/// text. It catches two rules sharing a message and a code leaking into the text.
#[test]
fn display_is_field_and_rule_text() {
    let err = broken(
        &job(),
        |j| j.image = "--privileged".into(),
        JobSpec::validate,
    );
    assert_eq!(
        err.to_string(),
        "image holds a character that is not allowed"
    );
    assert_eq!((err.field(), err.rule()), ("image", Rule::BadChars));
    let rules = [
        Rule::Empty,
        Rule::TooLong,
        Rule::TooMany,
        Rule::BadChars,
        Rule::OutOfRange,
        Rule::Duplicate,
        Rule::Inconsistent,
        Rule::Reserved,
        Rule::Escapes,
        Rule::UnsupportedProtocol,
        Rule::NotPinned,
    ];
    let mut texts: Vec<String> = rules.iter().map(ToString::to_string).collect();
    texts.sort();
    texts.dedup();
    assert_eq!(texts.len(), rules.len());
}
