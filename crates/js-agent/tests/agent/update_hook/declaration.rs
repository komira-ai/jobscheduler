//! The declaration the hook reads: the keys it needs, the ones it ignores, the paths it refuses.

use std::path::PathBuf;

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
        DeclarationError::NotPlain
    ));
    let relative_write = GOOD.replace("\"/opt/js-agent-link\"", "\"opt/js-agent-link\"");
    assert!(matches!(
        refused(&relative_write),
        DeclarationError::NotPlain
    ));
    assert_eq!(
        DeclarationError::NotPlain.to_string(),
        "state_dir and every write path must be absolute paths of plain components"
    );
}

#[test]
fn no_unit_is_refused() {
    let text = GOOD.replace(r#"units = ["js-agent-crun.service"]"#, "units = []");
    assert!(matches!(refused(&text), DeclarationError::NoUnits));
}

#[test]
fn more_than_one_unit_is_refused() {
    let text = GOOD.replace(
        r#"units = ["js-agent-crun.service"]"#,
        r#"units = ["js-agent-crun.service", "js-agent-docker.service"]"#,
    );
    assert!(matches!(refused(&text), DeclarationError::ManyUnits));
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

const WRITE: &str =
    r#"write_paths = ["/opt/js-agent", "/opt/js-agent-link", "/var/lib/js-agent-update"]"#;

#[test]
fn paths_that_are_not_plain_are_refused() {
    for state in [
        "/var/lib/../lib/js-agent-update",
        "/var/lib/js-agent-update/",
        "/var/lib/./js-agent-update",
        "/var//lib/js-agent-update",
    ] {
        let text = GOOD
            .replace(
                "state_dir = \"/var/lib/js-agent-update\"",
                &format!("state_dir = \"{state}\""),
            )
            .replace(
                WRITE,
                &WRITE.replace("\"/var/lib/js-agent-update\"", &format!("\"{state}\"")),
            );
        assert!(
            matches!(refused(&text), DeclarationError::NotPlain),
            "{state}"
        );
    }
    let text = GOOD.replace("\"/opt/js-agent-link\"", "\"/opt/js-agent-link/../x\"");
    assert!(matches!(refused(&text), DeclarationError::NotPlain));
}

#[test]
fn the_state_directory_must_be_a_write_path() {
    let text = GOOD.replace(
        WRITE,
        r#"write_paths = ["/opt/js-agent", "/opt/js-agent-link"]"#,
    );
    assert!(matches!(
        refused(&text),
        DeclarationError::StateDirNotListed
    ));
    // Listed by another spelling of the same directory is not listed.
    let text = GOOD.replace(
        WRITE,
        r#"write_paths = ["/opt/js-agent", "/var/lib/js-agent-update2"]"#,
    );
    assert!(matches!(
        refused(&text),
        DeclarationError::StateDirNotListed
    ));
}

#[test]
fn the_state_directory_and_the_other_write_paths_do_not_nest() {
    for other in ["/var/lib", "/var", "/", "/var/lib/js-agent-update/releases"] {
        let text = GOOD.replace(
            WRITE,
            &format!(r#"write_paths = ["/opt/js-agent", "{other}", "/var/lib/js-agent-update"]"#),
        );
        assert!(
            matches!(refused(&text), DeclarationError::StateDirNested),
            "{other}"
        );
    }
    // Siblings that share a prefix are not nested, and the state directory listed twice is
    // still one directory.
    let text = GOOD.replace(
        WRITE,
        r#"write_paths = ["/var/lib/js-agent-update2", "/var/lib/js-agent-update", "/var/lib/js-agent-update"]"#,
    );
    assert!(Declaration::parse(&text).is_ok());
}

const UNITS_LINE: &str = r#"units = ["js-agent-crun.service"]"#;

fn with_runtime(line: &str) -> String {
    GOOD.replace(UNITS_LINE, &format!("{UNITS_LINE}\n{line}"))
}

#[test]
fn the_runtime_defaults_to_the_one_of_the_platform_the_hook_is_built_for() {
    let want = if cfg!(target_os = "macos") {
        "native"
    } else {
        "crun"
    };
    assert_eq!(Declaration::parse(GOOD).unwrap().runtime, want);
}

#[test]
fn a_declared_runtime_is_read() {
    for runtime in ["crun", "docker", "native"] {
        let text = with_runtime(&format!("runtime = \"{runtime}\""));
        assert_eq!(Declaration::parse(&text).unwrap().runtime, runtime);
    }
}

#[test]
fn a_runtime_the_agent_does_not_have_is_refused() {
    for runtime in ["", "Crun", "crun native", "podman", " native"] {
        let text = with_runtime(&format!("runtime = \"{runtime}\""));
        let why = refused(&text).to_string();
        assert!(why.contains("runtime"), "{runtime:?}: {why}");
    }
    let wrong_type = with_runtime("runtime = 3");
    assert!(Declaration::parse(&wrong_type).is_err());
}
