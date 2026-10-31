//! The `--params` table: a flat table of scalars with two required whole-second keys and the
//! reserved `updater.*` keys the updater adds.

use std::path::PathBuf;

use js_agent::update_hook::{
    MIN_CORDON_LEASE_SECS, POLL_SECS, Params, ParamsError, RELEASE_LINK_KEY,
};
use js_testkit::dir::TestDir;

const CORDON: &str = concat!(
    "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
    "drain_wait_secs = 3600\ndefer_retry_secs = 300\n",
);
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RELEASES: &str = concat!(
    "\"updater.artifact_digest\" = \"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    "\"updater.staged_release\" = \"/opt/js-agent/releases/new\"\n",
    "\"updater.previous_release\" = \"/opt/js-agent/releases/old\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
    "\"updater.previous_version\" = \"1.3.2\"\n",
);
/// A first install: the updater sets no previous release or version.
const FIRST_INSTALL: &str = concat!(
    "\"updater.artifact_digest\" = \"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    "\"updater.staged_release\" = \"/opt/js-agent/releases/new\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
);

fn good() -> String {
    format!("{CORDON}{RELEASES}")
}

/// The cordon keys and the two required release keys, but for `key`, which is set to `value`
/// (a TOML value) as the one under test.
fn with(key: &str, value: &str) -> String {
    let digest = format!("\"{DIGEST}\"");
    let required = [
        ("updater.artifact_digest", digest.as_str()),
        ("updater.staged_release", "\"/opt/js-agent/releases/new\""),
        ("updater.target_version", "\"1.4.0\""),
    ];
    let mut text = CORDON.to_owned();
    for (name, ok) in required.iter().filter(|(name, _)| *name != key) {
        text.push_str(&format!("\"{name}\" = {ok}\n"));
    }
    text.push_str(&format!("\"{key}\" = {value}\n"));
    text
}

fn refused(text: &str) -> ParamsError {
    Params::parse(text).expect_err(text)
}

#[test]
fn the_cordon_keys_and_the_reserved_release_keys_are_read() {
    assert_eq!(
        Params::parse(&good()).unwrap(),
        Params {
            cordon_lease_secs: 300,
            cordon_max_secs: 17355,
            drain_wait_secs: 3600,
            defer_retry_secs: 300,
            artifact_digest: DIGEST.to_owned(),
            promotion_ceiling_secs: None,
            staged_release: PathBuf::from("/opt/js-agent/releases/new"),
            previous_release: Some(PathBuf::from("/opt/js-agent/releases/old")),
            release_link: None,
            target_version: "1.4.0".to_owned(),
            previous_version: Some("1.3.2".to_owned()),
        }
    );
}

#[test]
fn the_release_link_is_read_when_the_declaration_names_it_and_absent_otherwise() {
    assert_eq!(RELEASE_LINK_KEY, "release_link");
    assert_eq!(Params::parse(&good()).unwrap().release_link, None);
    let named = format!(
        "{}\"release_link\" = \"/opt/js-agent-link/current\"\n",
        good()
    );
    assert_eq!(
        Params::parse(&named).unwrap().release_link,
        Some(PathBuf::from("/opt/js-agent-link/current"))
    );
}

#[test]
fn a_relative_or_non_text_release_link_is_refused() {
    let relative = format!("{}\"release_link\" = \"current\"\n", good());
    assert!(matches!(refused(&relative), ParamsError::Relative(key) if key == RELEASE_LINK_KEY));
    let number = format!("{}\"release_link\" = 7\n", good());
    assert!(matches!(refused(&number), ParamsError::NotText(key) if key == RELEASE_LINK_KEY));
}

#[test]
fn a_first_install_has_no_previous_release_or_version() {
    let params = Params::parse(&format!("{CORDON}{FIRST_INSTALL}")).unwrap();
    assert_eq!(params.previous_release, None);
    assert_eq!(params.previous_version, None);
    assert_eq!(params.target_version, "1.4.0");
}

#[test]
fn the_promotion_ceiling_is_read_when_quoted() {
    let text = format!("{}\"updater.promotion_ceiling_secs\" = 3600\n", good());
    assert_eq!(
        Params::parse(&text).unwrap().promotion_ceiling_secs,
        Some(3600)
    );
}

#[test]
fn a_key_the_hook_does_not_know_is_ignored() {
    let text = format!(
        "{}note = \"x\"\nflag = true\nratio = 0.5\nmore = 7\n\"updater.future\" = 1\n",
        good()
    );
    let params = Params::parse(&text).unwrap();
    assert_eq!(params.cordon_lease_secs, 300);
    assert_eq!(params.promotion_ceiling_secs, None);
}

#[test]
fn a_missing_required_key_is_named() {
    let without = |key: &str| -> String {
        good()
            .lines()
            .filter(|line| !line.contains(key))
            .map(|line| format!("{line}\n"))
            .collect()
    };
    for key in [
        "cordon_lease_secs",
        "cordon_max_secs",
        "drain_wait_secs",
        "defer_retry_secs",
        "updater.artifact_digest",
        "updater.staged_release",
        "updater.target_version",
    ] {
        match refused(&without(key)) {
            ParamsError::Missing(missing) => assert_eq!(missing, key),
            other => panic!("{key}: {other}"),
        }
    }
    assert!(matches!(refused(""), ParamsError::Missing(_)));
}

#[test]
fn a_value_that_is_not_whole_seconds_is_refused() {
    let releases = RELEASES;
    for text in [
        format!("cordon_lease_secs = \"300\"\ncordon_max_secs = 1\n{releases}"),
        format!("cordon_lease_secs = -1\ncordon_max_secs = 1\n{releases}"),
        format!("cordon_lease_secs = 1\ncordon_max_secs = 2.5\n{releases}"),
        format!("cordon_lease_secs = 1\ncordon_max_secs = true\n{releases}"),
    ] {
        assert!(
            matches!(refused(&text), ParamsError::NotSeconds(_)),
            "{text}"
        );
    }
    let error = refused(&format!(
        "cordon_lease_secs = \"300\"\ncordon_max_secs = 1\n{releases}"
    ));
    assert_eq!(
        error.to_string(),
        "params key \"cordon_lease_secs\" must be a whole number of seconds, not negative"
    );
}

#[test]
fn the_promotion_ceiling_must_be_whole_seconds_too() {
    for value in ["\"1h\"", "-5", "1.5"] {
        let text = format!("{}\"updater.promotion_ceiling_secs\" = {value}\n", good());
        match refused(&text) {
            ParamsError::NotSeconds(key) => assert_eq!(key, "updater.promotion_ceiling_secs"),
            other => panic!("{value}: {other}"),
        }
    }
}

#[test]
fn the_reserved_keys_written_bare_are_a_table_and_refused() {
    let text = format!("{}updater.promotion_ceiling_secs = 3600\n", good());
    match refused(&text) {
        ParamsError::NotScalar(key) => assert_eq!(key, "updater"),
        other => panic!("{other}"),
    }
}

#[test]
fn a_release_or_version_that_is_not_a_string_is_refused() {
    for key in [
        "updater.staged_release",
        "updater.previous_release",
        "updater.target_version",
        "updater.previous_version",
    ] {
        match refused(&with(key, "7")) {
            ParamsError::NotText(got) => assert_eq!(got, key),
            other => panic!("{key}: {other}"),
        }
    }
}

#[test]
fn a_release_path_that_is_not_absolute_is_refused() {
    for key in ["updater.staged_release", "updater.previous_release"] {
        match refused(&with(key, "\"releases/new\"")) {
            ParamsError::Relative(got) => assert_eq!(got, key),
            other => panic!("{key}: {other}"),
        }
    }
    let error = ParamsError::Relative("k".to_owned());
    assert_eq!(
        error.to_string(),
        "params key \"k\" must be an absolute path"
    );
    assert_eq!(
        ParamsError::NotText("k".to_owned()).to_string(),
        "params key \"k\" must be a string"
    );
}

#[test]
fn a_value_that_is_not_a_scalar_is_refused() {
    for extra in [
        "list = [1, 2]",
        "table = { a = 1 }",
        "when = 2026-10-01T07:32:00Z",
    ] {
        let text = format!("{}{extra}\n", good());
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
    std::fs::write(&file, good()).unwrap();
    assert_eq!(Params::read(&file).unwrap().cordon_max_secs, 17355);
    let error = Params::read(&dir.path().join("absent.toml")).unwrap_err();
    assert!(matches!(error, ParamsError::Read(_)));
    assert!(error.to_string().starts_with("cannot read the params file"));
}

#[test]
fn the_artifact_digest_must_be_sha256_and_64_lowercase_hex_digits_in_a_string() {
    for bad in [
        "\"\"",
        "\"sha256:\"",
        "\"sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789\"",
        "\"sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"",
        "\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"",
        "\"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde\"",
    ] {
        match refused(&with("updater.artifact_digest", bad)) {
            ParamsError::NotDigest(key) => assert_eq!(key, "updater.artifact_digest"),
            other => panic!("{bad}: {other}"),
        }
    }
    match refused(&with("updater.artifact_digest", "7")) {
        ParamsError::NotText(key) => assert_eq!(key, "updater.artifact_digest"),
        other => panic!("{other}"),
    }
}

#[test]
fn the_wait_and_the_retry_are_whole_seconds_and_the_wait_and_cordon_keys_are_positive() {
    let releases = RELEASES;
    let text = |lease: &str, max: &str, wait: &str, retry: &str| {
        format!(
            "cordon_lease_secs = {lease}\ncordon_max_secs = {max}\ndrain_wait_secs = {wait}\ndefer_retry_secs = {retry}\n{releases}"
        )
    };
    assert!(Params::parse(&text("300", "17355", "3600", "300")).is_ok());
    assert!(
        Params::parse(&text("300", "17355", "3600", "0")).is_ok(),
        "a retry of 0 asks for none"
    );
    for (lease, max, wait, retry, zero) in [
        ("0", "17355", "3600", "300", "cordon_lease_secs"),
        ("300", "0", "3600", "300", "cordon_max_secs"),
        ("300", "17355", "0", "300", "drain_wait_secs"),
    ] {
        match refused(&text(lease, max, wait, retry)) {
            ParamsError::Zero(key) => assert_eq!(key, zero),
            other => panic!("{zero}: {other}"),
        }
    }
    for (wait, retry, key) in [
        ("\"3600\"", "300", "drain_wait_secs"),
        ("-1", "300", "drain_wait_secs"),
        ("3600", "1.5", "defer_retry_secs"),
        ("3600", "\"300\"", "defer_retry_secs"),
    ] {
        match refused(&text("300", "17355", wait, retry)) {
            ParamsError::NotSeconds(got) => assert_eq!(got, key),
            other => panic!("{key}: {other}"),
        }
    }
}

#[test]
fn the_cap_must_cover_the_drain_wait() {
    let text = |max: u64, wait: u64| {
        format!(
            "cordon_lease_secs = 300\ncordon_max_secs = {max}\ndrain_wait_secs = {wait}\ndefer_retry_secs = 300\n{RELEASES}"
        )
    };
    assert!(Params::parse(&text(3600, 3600)).is_ok());
    let error = refused(&text(3599, 3600));
    assert!(matches!(
        error,
        ParamsError::CapBelowWait {
            cap: 3599,
            wait: 3600
        }
    ));
    assert_eq!(
        error.to_string(),
        "cordon_max_secs 3599 is shorter than drain_wait_secs 3600"
    );
    assert_eq!(
        ParamsError::Zero("k".to_owned()).to_string(),
        "params key \"k\" must not be zero"
    );
    assert_eq!(
        ParamsError::NotDigest("k".to_owned()).to_string(),
        "params key \"k\" must be sha256: and 64 lowercase hex digits"
    );
}

#[test]
fn the_cordon_lease_must_be_at_least_three_polls() {
    let text = |lease: u64| {
        format!(
            "cordon_lease_secs = {lease}\ncordon_max_secs = 17355\ndrain_wait_secs = 3600\ndefer_retry_secs = 0\n{RELEASES}"
        )
    };
    assert_eq!(MIN_CORDON_LEASE_SECS, 3 * POLL_SECS);
    assert!(Params::parse(&text(MIN_CORDON_LEASE_SECS)).is_ok());
    let error = refused(&text(MIN_CORDON_LEASE_SECS - 1));
    assert!(matches!(
        error,
        ParamsError::LeaseTooShort { lease: 29, min: 30 }
    ));
    assert_eq!(
        error.to_string(),
        "cordon_lease_secs 29 is shorter than 30 s, three polls"
    );
    // A zero lease is still named as zero.
    assert!(matches!(refused(&text(0)), ParamsError::Zero(_)));
}

#[test]
fn a_retry_of_zero_seconds_means_try_again_at_once() {
    let text = format!(
        "cordon_lease_secs = 300\ncordon_max_secs = 17355\ndrain_wait_secs = 3600\ndefer_retry_secs = 0\n{RELEASES}"
    );
    assert_eq!(Params::parse(&text).unwrap().defer_retry_secs, 0);
}
