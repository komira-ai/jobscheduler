//! The `--params` table: a flat table of scalars with two required whole-second keys.

use js_agent::update_hook::{Params, ParamsError};
use js_testkit::dir::TestDir;

const GOOD: &str = "cordon_lease_secs = 300\ncordon_max_secs = 17355\n";

fn refused(text: &str) -> ParamsError {
    Params::parse(text).expect_err(text)
}

#[test]
fn the_two_required_keys_are_read() {
    assert_eq!(
        Params::parse(GOOD).unwrap(),
        Params {
            cordon_lease_secs: 300,
            cordon_max_secs: 17355,
            promotion_ceiling_secs: None,
        }
    );
}

#[test]
fn the_reserved_key_is_read_when_quoted() {
    let text = format!("{GOOD}\"updater.promotion_ceiling_secs\" = 3600\n");
    assert_eq!(
        Params::parse(&text).unwrap().promotion_ceiling_secs,
        Some(3600)
    );
}

#[test]
fn a_key_the_hook_does_not_know_is_ignored() {
    let text = format!("{GOOD}note = \"x\"\nflag = true\nratio = 0.5\nmore = 7\n");
    let params = Params::parse(&text).unwrap();
    assert_eq!(params.cordon_lease_secs, 300);
    assert_eq!(params.promotion_ceiling_secs, None);
}

#[test]
fn a_missing_key_is_named() {
    let error = refused("cordon_max_secs = 1\n");
    assert!(matches!(error, ParamsError::Missing("cordon_lease_secs")));
    let error = refused("cordon_lease_secs = 1\n");
    assert!(matches!(error, ParamsError::Missing("cordon_max_secs")));
    assert!(matches!(refused(""), ParamsError::Missing(_)));
}

#[test]
fn a_value_that_is_not_whole_seconds_is_refused() {
    for text in [
        "cordon_lease_secs = \"300\"\ncordon_max_secs = 1\n",
        "cordon_lease_secs = -1\ncordon_max_secs = 1\n",
        "cordon_lease_secs = 1\ncordon_max_secs = 2.5\n",
        "cordon_lease_secs = 1\ncordon_max_secs = true\n",
    ] {
        assert!(
            matches!(refused(text), ParamsError::NotSeconds(_)),
            "{text}"
        );
    }
    let error = refused("cordon_lease_secs = \"300\"\ncordon_max_secs = 1\n");
    assert_eq!(
        error.to_string(),
        "params key \"cordon_lease_secs\" must be a whole number of seconds, not negative"
    );
}

#[test]
fn the_reserved_key_must_be_whole_seconds_too() {
    for value in ["\"1h\"", "-5", "1.5"] {
        let text = format!("{GOOD}\"updater.promotion_ceiling_secs\" = {value}\n");
        match refused(&text) {
            ParamsError::NotSeconds(key) => assert_eq!(key, "updater.promotion_ceiling_secs"),
            other => panic!("{value}: {other}"),
        }
    }
}

#[test]
fn the_reserved_key_written_bare_is_a_table_and_refused() {
    let text = format!("{GOOD}updater.promotion_ceiling_secs = 3600\n");
    match refused(&text) {
        ParamsError::NotScalar(key) => assert_eq!(key, "updater"),
        other => panic!("{other}"),
    }
}

#[test]
fn a_value_that_is_not_a_scalar_is_refused() {
    for extra in [
        "list = [1, 2]",
        "table = { a = 1 }",
        "when = 2026-10-01T07:32:00Z",
    ] {
        let text = format!("{GOOD}{extra}\n");
        assert!(
            matches!(refused(&text), ParamsError::NotScalar(_)),
            "{extra}"
        );
    }
}

#[test]
fn text_that_is_not_toml_is_refused() {
    assert!(matches!(
        refused("cordon_lease_secs ="),
        ParamsError::Syntax(_)
    ));
}

#[test]
fn a_file_is_read_and_a_missing_one_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-params");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, GOOD).unwrap();
    assert_eq!(Params::read(&file).unwrap().cordon_max_secs, 17355);
    let error = Params::read(&dir.path().join("absent.toml")).unwrap_err();
    assert!(matches!(error, ParamsError::Read(_)));
    assert!(error.to_string().starts_with("cannot read the params file"));
}
