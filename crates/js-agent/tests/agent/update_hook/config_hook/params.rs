//! The params table of the config hook: the eight keys it requires, the paths and bounds it
//! refuses, the updater's previous version it reads, the other keys of the updater it ignores
//! and any other key it refuses.

use js_agent::update_hook::config::{
    CUTOVER_MIN_SECS, KEYS, Params, ParamsError, ROLLBACK_MIN_SECS, VERIFY_MIN_SECS,
};

use super::rig::text;
use crate::support::Scratch;

const ROOT: &str = "/var/lib/js-agent-config-test";

fn good() -> String {
    text(ROOT, 600, 300, 600)
}

fn without(key: &str) -> String {
    good()
        .lines()
        .filter(|line| !line.starts_with(&format!("{key} =")))
        .map(|line| format!("{line}\n"))
        .collect()
}

fn refusal(text: &str) -> String {
    Params::parse(text).unwrap_err().to_string()
}

/// Asserts that a table with the eight keys reads as its values say, from a file as from text.
///
/// Catches a key read into the wrong field.
#[test]
fn a_table_with_the_eight_keys_is_read() {
    let params = Params::parse(&good()).unwrap();
    assert_eq!(params.unit, "js-agent.service");
    assert_eq!(
        params.flags_file.to_str(),
        Some("/var/lib/js-agent-config-test/flags")
    );
    assert_eq!(
        params.token_file.to_str(),
        Some("/var/lib/js-agent-config-test/token")
    );
    assert_eq!(
        params.state_root.to_str(),
        Some("/var/lib/js-agent-config-test/state-root")
    );
    assert_eq!(
        params.state_dir.to_str(),
        Some("/var/lib/js-agent-config-test/state")
    );
    assert_eq!(
        (
            params.cutover_secs,
            params.verify_secs,
            params.rollback_secs
        ),
        (600, 300, 600)
    );
    assert_eq!(params.previous_version, None);
    assert_eq!(KEYS.len(), 8);
    let files = Scratch::new("config-hook-params-file");
    let file = files.write("params.toml", &good(), 0o600);
    assert_eq!(Params::read(&file).unwrap(), params);
    let missing = Params::read(&files.path("absent.toml"))
        .unwrap_err()
        .to_string();
    assert!(
        missing.starts_with("cannot read the params file: "),
        "{missing}"
    );
}

/// Asserts that every key is required, and that the refusal names the key.
///
/// Catches a hook that runs with a default for a path or a bound the package did not give.
#[test]
fn each_missing_key_is_refused_by_name() {
    for key in KEYS {
        assert_eq!(
            refusal(&without(key)),
            format!("params key {key:?} is required"),
            "{key}"
        );
    }
}

/// Asserts the refusals of a value: a relative path, a path not plainly spelt, an empty unit,
/// a unit or a bound of the wrong kind, a table that is not TOML or not scalar.
///
/// Catches a path that could be read as another by a `..` or a double slash, and a hook that
/// takes a number for a name.
#[test]
fn a_value_of_the_wrong_kind_or_spelling_is_refused_by_name() {
    let set = |key: &str, value: &str| {
        let line = format!("{key} = {value}\n");
        format!("{}{line}", without(key))
    };
    assert_eq!(
        refusal(&set("flags_file", "\"flags\"")),
        "params key \"flags_file\" must be an absolute path"
    );
    for key in ["flags_file", "token_file", "state_root", "state_dir"] {
        assert_eq!(
            refusal(&set(key, "\"/a/../b\"")),
            format!("params key {key:?} must be an absolute path of plain components"),
            "{key}"
        );
    }
    assert_eq!(
        refusal(&set("unit", "\"\"")),
        "params key \"unit\" must not be empty"
    );
    assert_eq!(
        refusal(&set("unit", "4")),
        "params key \"unit\" must be a string"
    );
    assert_eq!(
        refusal(&set("verify_timeout_secs", "\"300\"")),
        "params key \"verify_timeout_secs\" must be a whole number of seconds, not negative"
    );
    assert!(refusal("unit = [").starts_with("the params file is not a TOML table: "));
    assert_eq!(
        refusal(&format!("{}[table]\n", good())),
        "params key \"table\" is not a scalar (a dotted key must be quoted)"
    );
}

/// Asserts the least of each bound: one second under it is refused, the least is read.
///
/// Catches a bound that leaves no time to stop, or none to look, once the margins are taken
/// off.
#[test]
fn a_bound_shorter_than_its_waits_is_refused_and_the_least_is_read() {
    let limits = [
        ("cutover_timeout_secs", CUTOVER_MIN_SECS),
        ("verify_timeout_secs", VERIFY_MIN_SECS),
        ("rollback_timeout_secs", ROLLBACK_MIN_SECS),
    ];
    let with = |bounds: [u64; 3]| text(ROOT, bounds[0], bounds[1], bounds[2]);
    let least = [CUTOVER_MIN_SECS, VERIFY_MIN_SECS, ROLLBACK_MIN_SECS];
    assert!(Params::parse(&with(least)).is_ok());
    for (at, (key, min)) in limits.into_iter().enumerate() {
        let mut short = least;
        short[at] = min - 1;
        assert_eq!(
            refusal(&with(short)),
            format!(
                "params key {key:?} is {} s, and must be at least {min} s",
                min - 1
            )
        );
    }
    assert_eq!(
        (CUTOVER_MIN_SECS, VERIFY_MIN_SECS, ROLLBACK_MIN_SECS),
        (181, 16, 241)
    );
}

/// Asserts that the updater's keys (`updater.*`) are ignored but for the previous version, which
/// is read (an update's, as against a first install's, where it is absent) and must be text, and
/// that any other key is refused by name.
///
/// Catches a hook that refuses what the updater adds, and one that lets a misspelt key of the
/// package go unseen.
#[test]
fn the_updaters_keys_are_ignored_and_any_other_key_is_refused() {
    let with_updater = format!(
        "{}\"updater.staged_release\" = \"/r\"\n\"updater.future_key\" = 3\n",
        good()
    );
    assert_eq!(
        Params::parse(&with_updater).unwrap(),
        Params::parse(&good()).unwrap()
    );
    let update = format!("{}\"updater.previous_version\" = \"1.4.0\"\n", good());
    assert_eq!(
        Params::parse(&update).unwrap().previous_version.as_deref(),
        Some("1.4.0")
    );
    let not_text = format!("{}\"updater.previous_version\" = 3\n", good());
    assert_eq!(
        refusal(&not_text),
        "params key \"updater.previous_version\" must be a string"
    );
    let error = Params::parse(&format!("{}cutover_timeout = 5\n", good())).unwrap_err();
    assert!(matches!(&error, ParamsError::Unknown(key) if key == "cutover_timeout"));
    assert_eq!(
        error.to_string(),
        "params key \"cutover_timeout\" is not one this hook reads"
    );
}
