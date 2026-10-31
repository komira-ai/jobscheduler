//! The declaration the hook reads: the keys it needs, the ones it ignores, the paths it refuses.

use std::path::{Path, PathBuf};

use js_agent::update_hook::{DECLARATION_PATH, Declaration, DeclarationError, Timeouts};
use js_testkit::dir::TestDir;

const GOOD: &str = r#"
name = "js-agent"
binary = "js-agent"
uid = "js-agent"
registration_ttl_secs = 300
verify_deadline_secs = 9000
state_dir = "/var/lib/js-agent-update"

[hook]
user = "root"
group = "root"
capabilities = []
read_paths = ["/opt/js-agent", "/var/lib/js-agent"]
write_paths = ["/opt/js-agent", "/opt/js-agent-link", "/var/lib/js-agent-update"]
units = ["js-agent-crun.service"]
address_families = ["AF_UNIX", "AF_INET", "AF_INET6"]
environment = { }

[hook.params]
cordon_lease_secs = 300
cordon_max_secs = 17355

[hook.timeout_secs]
prepare = 3900
cutover = 4320
verify = 615
rollback = 8220
cleanup = 120
"#;

fn refused(text: &str) -> DeclarationError {
    Declaration::parse(text).expect_err(text)
}

#[test]
fn the_keys_the_hook_needs_are_read() {
    let declaration = Declaration::parse(GOOD).unwrap();
    assert_eq!(
        declaration.state_dir,
        PathBuf::from("/var/lib/js-agent-update")
    );
    assert_eq!(declaration.units, vec!["js-agent-crun.service".to_owned()]);
    assert_eq!(declaration.write_paths.len(), 3);
    assert_eq!(
        declaration.timeouts,
        Timeouts {
            prepare: 3900,
            cutover: 4320,
            verify: 615,
            rollback: 8220,
            cleanup: 120,
        }
    );
}

#[test]
fn the_release_directories_are_the_write_paths_but_the_state_directory() {
    let declaration = Declaration::parse(GOOD).unwrap();
    let dirs: Vec<&Path> = declaration.release_dirs().collect();
    assert_eq!(
        dirs,
        vec![Path::new("/opt/js-agent"), Path::new("/opt/js-agent-link")]
    );
}

#[test]
fn keys_the_hook_does_not_read_are_ignored() {
    // A key after a table header belongs to that table, so the extras go last.
    let text = format!("{GOOD}\n[hook.future]\ny = true\n\n[extra]\nx = 2\n");
    assert_eq!(
        Declaration::parse(&text).unwrap(),
        Declaration::parse(GOOD).unwrap()
    );
}

#[test]
fn a_key_that_is_missing_or_of_the_wrong_type_is_refused() {
    for (name, text) in [
        ("state_dir", GOOD.replace("state_dir = ", "other_dir = ")),
        ("units", GOOD.replace("units = ", "unitz = ")),
        ("write_paths", GOOD.replace("write_paths = ", "wp = ")),
        (
            "timeouts",
            GOOD.replace("[hook.timeout_secs]", "[hook.other]"),
        ),
        ("one timeout", GOOD.replace("cleanup = 120", "")),
        (
            "negative timeout",
            GOOD.replace("cleanup = 120", "cleanup = -1"),
        ),
        (
            "text timeout",
            GOOD.replace("cleanup = 120", "cleanup = \"2m\""),
        ),
        (
            "units type",
            GOOD.replace(r#"units = ["js-agent-crun.service"]"#, "units = 3"),
        ),
    ] {
        assert!(
            matches!(refused(&text), DeclarationError::Parse(_)),
            "{name}"
        );
    }
    assert!(matches!(refused("not toml ="), DeclarationError::Parse(_)));
}

#[test]
fn a_relative_state_directory_or_write_path_is_refused() {
    let relative_state = GOOD.replace(
        "\"/var/lib/js-agent-update\"\n",
        "\"var/lib/js-agent-update\"\n",
    );
    assert!(matches!(
        refused(&relative_state),
        DeclarationError::Relative
    ));
    let relative_write = GOOD.replace("\"/opt/js-agent-link\"", "\"opt/js-agent-link\"");
    assert!(matches!(
        refused(&relative_write),
        DeclarationError::Relative
    ));
    assert_eq!(
        DeclarationError::Relative.to_string(),
        "state_dir and every write path must be absolute"
    );
}

#[test]
fn no_unit_is_refused() {
    let text = GOOD.replace(r#"units = ["js-agent-crun.service"]"#, "units = []");
    assert!(matches!(refused(&text), DeclarationError::NoUnits));
}

#[test]
fn a_declaration_with_no_directory_for_releases_is_refused() {
    let text = GOOD.replace(
        r#"write_paths = ["/opt/js-agent", "/opt/js-agent-link", "/var/lib/js-agent-update"]"#,
        r#"write_paths = ["/var/lib/js-agent-update"]"#,
    );
    assert!(matches!(refused(&text), DeclarationError::NoReleaseDir));
    let none = GOOD.replace(
        r#"write_paths = ["/opt/js-agent", "/opt/js-agent-link", "/var/lib/js-agent-update"]"#,
        "write_paths = []",
    );
    assert!(matches!(refused(&none), DeclarationError::NoReleaseDir));
}

#[test]
fn a_declaration_is_read_from_a_path_and_a_missing_one_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-declaration");
    let file = dir.path().join("js-agent.toml");
    std::fs::write(&file, GOOD).unwrap();
    assert_eq!(
        Declaration::read(&file).unwrap(),
        Declaration::parse(GOOD).unwrap()
    );
    let error = Declaration::read(&dir.path().join("absent.toml")).unwrap_err();
    assert!(matches!(error, DeclarationError::Read(_)));
    assert!(error.to_string().starts_with("cannot read the declaration"));
    assert_eq!(DECLARATION_PATH, "/etc/updater.d/js-agent.toml");
}
