//! The params file of `drain` and `resume`: the manifest's table with the updater's
//! maintenance keys, of which these steps read seven and ignore the rest.

use std::path::{Path, PathBuf};

use js_agent::update_hook::{
    CORDON_LEASE_KEY, DEFER_RETRY_KEY, DRAIN_DEADLINE_KEY, DrainParams, FLAGS_FILE_KEY,
    MIN_CORDON_LEASE_SECS, ParamsError, STATE_ROOT_KEY, TOKEN_FILE_KEY, UNIT_KEY,
};
use js_testkit::dir::TestDir;

/// The keys these steps read, in a table that holds them all and no deadline.
const KEYS: [(&str, &str); 6] = [
    ("unit", "\"js-agent-crun.service\""),
    ("flags_file", "\"/etc/js-agent/flags\""),
    ("token_file", "\"/etc/js-agent/token\""),
    ("state_root", "\"/var/lib/js-agent/state\""),
    ("cordon_lease_secs", "300"),
    ("defer_retry_secs", "120"),
];

/// The table with `key` set to `value` (the line `key = value`), or without it when `value` is
/// `None`; a key the table lacks is added.
fn table_with(key: &str, value: Option<&str>) -> String {
    let mut text = String::new();
    let mut seen = false;
    for (name, default) in KEYS {
        if name == key {
            seen = true;
            if let Some(value) = value {
                text.push_str(&format!("{name} = {value}\n"));
            }
        } else {
            text.push_str(&format!("{name} = {default}\n"));
        }
    }
    if let (false, Some(value)) = (seen, value) {
        text.push_str(&format!("{key} = {value}\n"));
    }
    text
}

fn full() -> String {
    table_with("anything", None)
}

/// Asserts that the table with every key it reads is read exactly, with no deadline. It catches
/// a field read from the wrong key.
#[test]
fn a_table_with_every_key_is_read_value_for_value() {
    assert_eq!(
        DrainParams::parse(&full()).unwrap(),
        DrainParams {
            unit: "js-agent-crun.service".to_owned(),
            flags_file: PathBuf::from("/etc/js-agent/flags"),
            token_file: PathBuf::from("/etc/js-agent/token"),
            state_root: PathBuf::from("/var/lib/js-agent/state"),
            cordon_lease_secs: 300,
            defer_retry_secs: 120,
            drain_deadline_unix: None,
        }
    );
}

/// Asserts that each key these steps read is `Missing` by its own name when it is absent. It
/// catches a key that has a silent default.
#[test]
fn every_key_these_steps_read_is_required() {
    for (key, _) in KEYS {
        let got = DrainParams::parse(&table_with(key, None));
        assert!(
            matches!(&got, Err(ParamsError::Missing(missing)) if *missing == key),
            "{key}: {got:?}"
        );
    }
    let keys = [
        UNIT_KEY,
        FLAGS_FILE_KEY,
        TOKEN_FILE_KEY,
        STATE_ROOT_KEY,
        CORDON_LEASE_KEY,
        DEFER_RETRY_KEY,
    ];
    assert_eq!(keys, KEYS.map(|(key, _)| key));
}

/// Asserts that a text key given a number is `NotText` and a seconds key given text is
/// `NotSeconds`, each naming the key. It catches a value read by coercion.
#[test]
fn a_value_of_the_wrong_type_is_refused_by_key() {
    for key in ["unit", "flags_file", "token_file", "state_root"] {
        let got = DrainParams::parse(&table_with(key, Some("7")));
        assert!(
            matches!(&got, Err(ParamsError::NotText(name)) if name == key),
            "{key}: {got:?}"
        );
    }
    for key in ["cordon_lease_secs", "defer_retry_secs"] {
        let got = DrainParams::parse(&table_with(key, Some("\"300\"")));
        assert!(
            matches!(&got, Err(ParamsError::NotSeconds(name)) if name == key),
            "{key}: {got:?}"
        );
    }
}

/// Asserts that a relative path in any of the three path keys is `Relative`, naming the key. It
/// catches a path that would be taken against the updater's working directory.
#[test]
fn a_relative_path_is_refused_by_key() {
    for key in ["flags_file", "token_file", "state_root"] {
        let got = DrainParams::parse(&table_with(key, Some("\"relative/path\"")));
        assert!(
            matches!(&got, Err(ParamsError::Relative(name)) if name == key),
            "{key}: {got:?}"
        );
    }
}

/// Asserts that a lease of less than three polls and a lease of zero are `LeaseTooShort` with the
/// lease and the minimum, and that the minimum is accepted. It catches a cordon whose lease
/// lapses between two polls.
#[test]
fn a_cordon_lease_shorter_than_three_polls_is_refused() {
    for lease in [0, 1, MIN_CORDON_LEASE_SECS - 1] {
        let got = DrainParams::parse(&table_with("cordon_lease_secs", Some(&lease.to_string())));
        assert!(
            matches!(
                got,
                Err(ParamsError::LeaseTooShort { lease: l, min })
                    if l == lease && min == MIN_CORDON_LEASE_SECS
            ),
            "{lease}"
        );
    }
    let at_min = table_with(
        "cordon_lease_secs",
        Some(&MIN_CORDON_LEASE_SECS.to_string()),
    );
    assert_eq!(
        DrainParams::parse(&at_min).unwrap().cordon_lease_secs,
        MIN_CORDON_LEASE_SECS
    );
}

/// Asserts that an empty unit is `Empty` naming the key. It catches a restart of a unit with no
/// name.
#[test]
fn an_empty_unit_is_refused() {
    let got = DrainParams::parse(&table_with("unit", Some("\"\"")));
    assert!(
        matches!(&got, Err(ParamsError::Empty(name)) if name == "unit"),
        "{got:?}"
    );
}

/// Asserts that a negative deadline, a text deadline and a fractional one are `NotSeconds` naming
/// the deadline key. It catches a deadline that wraps or is read as text.
#[test]
fn a_deadline_that_is_not_whole_seconds_is_refused() {
    for value in ["-1", "\"soon\"", "1.5", "true"] {
        let got = DrainParams::parse(&table_with(
            &format!("\"{DRAIN_DEADLINE_KEY}\""),
            Some(value),
        ));
        assert!(
            matches!(&got, Err(ParamsError::NotSeconds(name)) if name == "updater.drain_deadline_unix"),
            "{value}: {got:?}"
        );
    }
    assert_eq!(DRAIN_DEADLINE_KEY, "updater.drain_deadline_unix");
}

/// Asserts that a deadline is `None` when absent and `Some` of its seconds when present, zero
/// included. It catches a deadline that is always set or never read.
#[test]
fn the_deadline_is_none_when_absent_and_its_seconds_when_given() {
    assert_eq!(
        DrainParams::parse(&full()).unwrap().drain_deadline_unix,
        None
    );
    for (value, want) in [("1893456000", 1_893_456_000), ("0", 0)] {
        let text = table_with(&format!("\"{DRAIN_DEADLINE_KEY}\""), Some(value));
        assert_eq!(
            DrainParams::parse(&text).unwrap().drain_deadline_unix,
            Some(want),
            "{value}"
        );
    }
}

/// Asserts that text that is not TOML is `Syntax`. It catches a parser that reads a broken file
/// as an empty table.
#[test]
fn text_that_is_not_toml_is_refused() {
    let got = DrainParams::parse("unit = = 3\n");
    assert!(matches!(got, Err(ParamsError::Syntax(_))), "{got:?}");
}

/// Asserts that an array or a table value is `NotScalar` naming its key, even a key these steps
/// do not read. It catches a table the updater could not have written.
#[test]
fn a_value_that_is_not_a_scalar_is_refused_by_key() {
    for (line, key) in [("extra = [1, 2]\n", "extra"), ("[extra]\na = 1\n", "extra")] {
        let text = format!("{}{line}", full());
        let got = DrainParams::parse(&text);
        assert!(
            matches!(&got, Err(ParamsError::NotScalar(name)) if name == key),
            "{line:?}: {got:?}"
        );
    }
}

/// Asserts that keys these steps do not read are ignored, whatever their type, among them the
/// updater's `updater.service` and `updater.run_id`, and that the values read do not change. It
/// catches a parser that refuses a key of an update's table.
#[test]
fn keys_these_steps_do_not_read_are_ignored() {
    let extra = concat!(
        "\"updater.service\" = \"js-agent\"\n",
        "\"updater.run_id\" = \"run-7\"\n",
        "runtime = \"crun\"\ncordon_max_secs = \"not a number\"\nflag = true\nratio = 0.5\n",
        "unknown_key = 42\n",
    );
    let text = format!("{}{extra}", full());
    assert_eq!(
        DrainParams::parse(&text).unwrap(),
        DrainParams::parse(&full()).unwrap()
    );
}

/// Asserts that the table the js-agent package ships, with the three keys the updater adds to a
/// maintenance run, is accepted and read exactly. It catches a key the package ships that these
/// steps cannot read.
#[test]
fn the_table_the_package_ships_with_the_updaters_keys_is_accepted() {
    let text = concat!(
        "unit = \"js-agent-crun.service\"\nstate_dir = \"/var/lib/js-agent-update\"\n",
        "release_link = \"/opt/js-agent/current\"\nflags_file = \"/etc/js-agent/flags\"\n",
        "token_file = \"/etc/js-agent/token\"\nstate_root = \"/var/lib/js-agent/state\"\n",
        "runtime = \"crun\"\ncordon_lease_secs = 300\ncordon_max_secs = 17355\n",
        "drain_wait_secs = 3600\ndefer_retry_secs = 300\nprepare_timeout_secs = 3900\n",
        "cutover_timeout_secs = 4320\nverify_timeout_secs = 615\n",
        "rollback_timeout_secs = 8220\ncleanup_timeout_secs = 120\nverify_deadline_secs = 9000\n",
        "\"updater.service\" = \"js-agent\"\n\"updater.run_id\" = \"7c1f\"\n",
        "\"updater.drain_deadline_unix\" = 1893456000\n",
    );
    assert_eq!(
        DrainParams::parse(text).unwrap(),
        DrainParams {
            unit: "js-agent-crun.service".to_owned(),
            flags_file: PathBuf::from("/etc/js-agent/flags"),
            token_file: PathBuf::from("/etc/js-agent/token"),
            state_root: PathBuf::from("/var/lib/js-agent/state"),
            cordon_lease_secs: 300,
            defer_retry_secs: 300,
            drain_deadline_unix: Some(1_893_456_000),
        }
    );
}

/// Asserts that a table with none of the update's staged-release keys (no digest, no version, no
/// staged release) is accepted, which an update's table is not. It catches a drain that asks for
/// what only an update has.
#[test]
fn the_table_of_a_maintenance_run_names_no_release() {
    assert!(DrainParams::parse(&full()).is_ok());
    assert!(js_agent::update_hook::Params::parse(&full()).is_err());
}

/// Asserts that `read` reads a file on disk, and that a file that is not there is `Read`. It
/// catches a read that ends a missing file as a syntax error.
#[test]
fn a_params_file_is_read_from_disk_and_a_missing_one_is_a_read_error() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "drain-params-read");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, full()).unwrap();
    assert_eq!(
        DrainParams::read(&file).unwrap(),
        DrainParams::parse(&full()).unwrap()
    );
    let absent: &Path = &dir.path().join("absent.toml");
    let got = DrainParams::read(absent);
    assert!(
        matches!(&got, Err(ParamsError::Read(e)) if e.kind() == std::io::ErrorKind::NotFound),
        "{got:?}"
    );
}
