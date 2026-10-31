//! The environment names a bundle and a job may set: the reserved list is the one in
//! `js-types`, and a name has a length cap.

mod common;

use common::{edited, set};
use js_bundle::{ConfigError, Defect, EnvNameError, check_env_name};
use js_types::env::{MAX_ENV_NAME_LEN, OWNED_ENV_NAMES, RESERVED_ENV_PREFIXES, SHELL_ENV_NAMES};
use serde_json::json;

/// `check_env_name` reads the shared list: `MallocStackLogging`, `OBJC_DEBUG`, `__XPC_X`
/// and `JS_FOO` are refused, as are the names the agent owns and every entry of the
/// shared list. It catches a copy of the list drifting from the shared one.
#[test]
fn names_are_refused_by_the_shared_reserved_list() {
    for name in [
        "MallocStackLogging",
        "MALLOC_CONF",
        "OBJC_DEBUG",
        "__XPC_X",
        "JS_FOO",
        "DYLD_INSERT_LIBRARIES",
        "HOME",
        "TMPDIR",
    ] {
        assert_eq!(check_env_name(name), Err(EnvNameError::Forbidden), "{name}");
    }
    for prefix in RESERVED_ENV_PREFIXES {
        let name = format!("{prefix}X");
        assert_eq!(
            check_env_name(&name),
            Err(EnvNameError::Forbidden),
            "{name}"
        );
    }
    for name in OWNED_ENV_NAMES {
        assert_eq!(check_env_name(name), Err(EnvNameError::Forbidden), "{name}");
    }
    for name in SHELL_ENV_NAMES {
        assert_eq!(check_env_name(name), Err(EnvNameError::Forbidden), "{name}");
    }
    for name in ["PATH", "LDX", "JS", "OBJC", "_A9"] {
        assert_eq!(check_env_name(name), Ok(()), "{name}");
    }
}

/// A name of the longest length is accepted, one byte more is refused.
#[test]
fn name_length_is_capped_at_the_shared_limit() {
    assert_eq!(check_env_name(&"A".repeat(MAX_ENV_NAME_LEN)), Ok(()));
    assert_eq!(
        check_env_name(&"A".repeat(MAX_ENV_NAME_LEN + 1)),
        Err(EnvNameError::TooLong {
            max: MAX_ENV_NAME_LEN
        })
    );
    assert_eq!(
        check_env_name(&"A".repeat(100_000)),
        Err(EnvNameError::TooLong {
            max: MAX_ENV_NAME_LEN
        })
    );
}

/// A bundle rule is refused by the same list, with its field.
#[test]
fn bundle_rules_are_refused_by_the_shared_reserved_list() {
    for name in ["MallocStackLogging", "OBJC_DEBUG", "__XPC_X", "JS_FOO"] {
        let got = edited(|v| set(v, "/env/0/name", json!(name)));
        assert_eq!(
            got,
            Err(ConfigError::Invalid {
                field: "env[0].name".to_string(),
                defect: Defect::EnvName(EnvNameError::Forbidden)
            }),
            "{name}"
        );
    }
}
