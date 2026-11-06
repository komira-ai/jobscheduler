//! The `--params` table: a flat table of scalars, the keys the package manifest gives
//! (`KEYS`, all required) and the reserved `updater.*` keys the updater adds.

use std::path::PathBuf;

use js_agent::update_hook::params::KEYS;
use js_agent::update_hook::{
    ConnectParams, MIN_CORDON_LEASE_SECS, POLL_SECS, Params, ParamsError, RELEASE_LINK_KEY,
    STATE_ROOT_KEY, Settings, TOKEN_FILE_KEY, Timeouts,
};
use js_testkit::dir::TestDir;

const CORDON: &str = concat!(
    "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
    "drain_wait_secs = 3600\ndefer_retry_secs = 300\n",
);
/// The manifest's keys but the cordon's: what the package says of the unit, the paths and the
/// bounds.
const SETTINGS: &str = concat!(
    "unit = \"js-agent-crun.service\"\nstate_dir = \"/var/lib/js-agent-update\"\n",
    "release_link = \"/var/lib/hostkeeper/packages/js-agent/current\"\n",
    "flags_file = \"/etc/js-agent/flags\"\ntoken_file = \"/etc/js-agent/token\"\n",
    "state_root = \"/var/lib/js-agent/state\"\nruntime = \"crun\"\n",
    "prepare_timeout_secs = 3900\ncutover_timeout_secs = 4320\nverify_timeout_secs = 615\n",
    "rollback_timeout_secs = 8220\ncleanup_timeout_secs = 120\nverify_deadline_secs = 9000\n",
);
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RELEASES: &str = concat!(
    "\"updater.artifact_digest\" = \"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    "\"updater.staged_release\" = \"/opt/js-agent/releases/new\"\n",
    "\"updater.previous_release\" = \"/opt/js-agent/releases/old\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
    "\"updater.previous_version\" = \"1.3.2\"\n",
    "unit = \"js-agent-crun.service\"\nstate_dir = \"/var/lib/js-agent-update\"\n",
    "release_link = \"/var/lib/hostkeeper/packages/js-agent/current\"\n",
    "flags_file = \"/etc/js-agent/flags\"\ntoken_file = \"/etc/js-agent/token\"\n",
    "state_root = \"/var/lib/js-agent/state\"\nruntime = \"crun\"\n",
    "prepare_timeout_secs = 3900\ncutover_timeout_secs = 4320\nverify_timeout_secs = 615\n",
    "rollback_timeout_secs = 8220\ncleanup_timeout_secs = 120\nverify_deadline_secs = 9000\n",
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

/// `good()` but for the line of `key`, which `replacement` (a whole line, or nothing) stands
/// for.
fn good_but(key: &str, replacement: &str) -> String {
    good()
        .lines()
        .map(|line| {
            if line.starts_with(&format!("{key} =")) {
                replacement.to_owned()
            } else {
                line.to_owned()
            }
        })
        .map(|line| format!("{line}\n"))
        .collect()
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
    let mut text = format!("{CORDON}{SETTINGS}");
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
fn the_manifest_keys_and_the_reserved_release_keys_are_read() {
    assert_eq!(
        Params::parse(&good()).unwrap(),
        Params {
            settings: Settings {
                state_dir: PathBuf::from("/var/lib/js-agent-update"),
                unit: "js-agent-crun.service".to_owned(),
                runtime: "crun".to_owned(),
                timeouts: Timeouts {
                    prepare: 3900,
                    cutover: 4320,
                    verify: 615,
                    rollback: 8220,
                    cleanup: 120,
                },
                verify_deadline_secs: 9000,
            },
            cordon_lease_secs: 300,
            cordon_max_secs: 17355,
            drain_wait_secs: 3600,
            defer_retry_secs: 300,
            artifact_digest: DIGEST.to_owned(),
            promotion_ceiling_secs: None,
            staged_release: PathBuf::from("/opt/js-agent/releases/new"),
            previous_release: Some(PathBuf::from("/opt/js-agent/releases/old")),
            release_link: PathBuf::from("/var/lib/hostkeeper/packages/js-agent/current"),
            flags_file: PathBuf::from("/etc/js-agent/flags"),
            target_version: "1.4.0".to_owned(),
            previous_version: Some("1.3.2".to_owned()),
            state_root: PathBuf::from("/var/lib/js-agent/state"),
            token_file: PathBuf::from("/etc/js-agent/token"),
        }
    );
}

/// `KEYS` is what a manifest must give: dropping any one of them is refused naming it, and a
/// table with all of them and one more is read. xtask holds the manifests to the same list.
///
/// Catches a key the hook reads that `KEYS` leaves out (a manifest could then omit it), and a key
/// `KEYS` lists that the hook never requires.
#[test]
fn every_key_of_the_list_is_required_and_nothing_else_is() {
    assert_eq!(KEYS.len(), 17);
    for key in KEYS {
        match refused(&good_but(key, "")) {
            ParamsError::Missing(missing) => assert_eq!(missing, key),
            other => panic!("{key}: {other}"),
        }
    }
    let extra = format!("{}something_else = \"x\"\n", good());
    assert!(Params::parse(&extra).is_ok());
    assert_eq!(RELEASE_LINK_KEY, "release_link");
}

#[test]
fn the_release_link_is_required_and_read_as_given() {
    let named = good_but(
        "release_link",
        "release_link = \"/opt/js-agent-link/current\"",
    );
    assert_eq!(
        Params::parse(&named).unwrap().release_link,
        PathBuf::from("/opt/js-agent-link/current")
    );
}

#[test]
fn a_relative_or_non_text_release_link_is_refused() {
    let relative = good_but("release_link", "release_link = \"current\"");
    assert!(matches!(refused(&relative), ParamsError::Relative(key) if key == RELEASE_LINK_KEY));
    let number = good_but("release_link", "release_link = 7");
    assert!(matches!(refused(&number), ParamsError::NotText(key) if key == RELEASE_LINK_KEY));
}

#[test]
fn a_first_install_has_no_previous_release_or_version() {
    let params = Params::parse(&format!("{CORDON}{FIRST_INSTALL}{SETTINGS}")).unwrap();
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

/// The state root and the token file are the agent's own flags by name; the server, the
/// certificate and the host id are not params keys (they come from the flags file).
#[test]
fn the_state_root_and_the_token_file_are_params_and_the_server_is_not() {
    assert_eq!(
        [STATE_ROOT_KEY, TOKEN_FILE_KEY],
        ["state_root", "token_file"]
    );
    let params = Params::parse(&good()).unwrap();
    assert_eq!(params.state_root, PathBuf::from("/var/lib/js-agent/state"));
    assert_eq!(params.token_file, PathBuf::from("/etc/js-agent/token"));
    // A `server` key is just an unknown key: the flags file is where the server is named.
    let text = format!("{}server = \"https://leases.example:7300\"\n", good());
    assert!(Params::parse(&text).is_ok());
    assert_eq!(ConnectParams::default().server, None);
}

/// A path that is not absolute, or is not text, is refused by key.
#[test]
fn the_paths_of_the_table_must_be_absolute_text() {
    for key in [
        "state_dir",
        "release_link",
        "flags_file",
        "token_file",
        "state_root",
    ] {
        let relative = good_but(key, &format!("{key} = \"relative/path\""));
        assert!(
            matches!(refused(&relative), ParamsError::Relative(k) if k == key),
            "{key}"
        );
        let number = good_but(key, &format!("{key} = 7"));
        assert!(
            matches!(refused(&number), ParamsError::NotText(k) if k == key),
            "{key}"
        );
    }
}

/// The record's directory must be spelt plainly: it is where the hook's lock and record are, and
/// a `..` or a doubled slash could name another place than the one that was checked.
///
/// Catches a state directory that is taken as given.
#[test]
fn the_state_directory_must_be_plain() {
    for bad in [
        "/var/lib/../js-agent-update",
        "/var/lib//js-agent-update",
        "/var/lib/js-agent-update/",
        "/var/lib/./js-agent-update",
    ] {
        let text = good_but("state_dir", &format!("state_dir = \"{bad}\""));
        match refused(&text) {
            ParamsError::NotPlain(key) => assert_eq!(key, "state_dir", "{bad}"),
            other => panic!("{bad}: {other}"),
        }
    }
    assert_eq!(
        ParamsError::NotPlain("state_dir".to_owned()).to_string(),
        "params key \"state_dir\" must be an absolute path of plain components"
    );
}

#[test]
fn the_unit_is_text_and_not_empty() {
    let empty = good_but("unit", "unit = \"\"");
    assert!(matches!(refused(&empty), ParamsError::Empty(key) if key == "unit"));
    let number = good_but("unit", "unit = 7");
    assert!(matches!(refused(&number), ParamsError::NotText(key) if key == "unit"));
    assert_eq!(
        ParamsError::Empty("unit".to_owned()).to_string(),
        "params key \"unit\" must not be empty"
    );
}

/// The runtime is one an agent lists on its `--version`.
///
/// Catches a runtime that is taken as given, which would make the staged agent's check pass
/// or fail for a word no agent has.
#[test]
fn the_runtime_is_one_of_crun_docker_native() {
    for runtime in ["crun", "docker", "native"] {
        let text = good_but("runtime", &format!("runtime = \"{runtime}\""));
        assert_eq!(Params::parse(&text).unwrap().settings.runtime, runtime);
    }
    let error = refused(&good_but("runtime", "runtime = \"podman\""));
    assert!(matches!(&error, ParamsError::UnknownRuntime(r) if r == "podman"));
    assert_eq!(
        error.to_string(),
        "runtime \"podman\" is not one of crun, docker, native"
    );
}

#[test]
fn each_bound_is_a_whole_number_of_seconds() {
    for key in [
        "prepare_timeout_secs",
        "cutover_timeout_secs",
        "verify_timeout_secs",
        "rollback_timeout_secs",
        "cleanup_timeout_secs",
        "verify_deadline_secs",
    ] {
        for value in ["\"60\"", "-1", "1.5"] {
            let text = good_but(key, &format!("{key} = {value}"));
            assert!(
                matches!(refused(&text), ParamsError::NotSeconds(k) if k == key),
                "{key} = {value}"
            );
        }
    }
}

/// The updater rolls a verify back at the deadline: it must leave the verify bound and the
/// rollback bound room, or a verify that is still allowed to run would be rolled back.
///
/// Catches a deadline compared with one bound only, or off by one.
#[test]
fn the_verify_deadline_covers_the_verify_bound_and_the_rollback_bound() {
    let text = |deadline: u64| {
        good_but(
            "verify_deadline_secs",
            &format!("verify_deadline_secs = {deadline}"),
        )
    };
    assert_eq!(
        Params::parse(&text(8835))
            .unwrap()
            .settings
            .verify_deadline_secs,
        8835
    );
    let error = refused(&text(8834));
    assert!(matches!(
        error,
        ParamsError::VerifyDeadline {
            deadline: 8834,
            verify: 615,
            rollback: 8220
        }
    ));
    assert_eq!(
        error.to_string(),
        "verify_deadline_secs 8834 is less than the verify bound 615 plus the rollback bound 8220"
    );
}
