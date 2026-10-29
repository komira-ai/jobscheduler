//! Names, versions, environment names and the Xcode need.

mod common;

use common::golden;
use js_bundle::{
    DiffId, DottedVersion, EnvNameError, NetworkLevel, TreeName, XcodeComponent, check_env_name,
};

/// Tree names are `[a-z][a-z0-9-]{0,31}`.
#[test]
fn tree_names_follow_the_grammar() {
    let long = "a".repeat(32);
    for ok in ["a", "job", "zig-0", "a0", long.as_str()] {
        assert_eq!(TreeName::parse(ok).unwrap().as_str(), ok);
    }
    let too_long = "a".repeat(33);
    for bad in [
        "",
        "0a",
        "-a",
        "A",
        "aB",
        "a_b",
        "a b",
        "a/b",
        "\u{e9}",
        too_long.as_str(),
    ] {
        assert!(TreeName::parse(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        TreeName::parse("A").unwrap_err().to_string(),
        "invalid tree name: \"A\""
    );
    assert_eq!(String::from(TreeName::parse("job").unwrap()), "job");
}

/// A diff id is `sha256:` and 64 lowercase hex digits; the hex names the directory.
#[test]
fn diff_ids_follow_the_grammar() {
    let ok = format!("sha256:{}", "0123456789abcdef".repeat(4));
    let id = DiffId::parse(&ok).unwrap();
    assert_eq!(id.as_str(), ok);
    assert_eq!(id.hex(), "0123456789abcdef".repeat(4));
    assert_eq!(String::from(id), ok);
    let hex = "a".repeat(64);
    for bad in [
        String::new(),
        hex.clone(),
        format!("sha512:{hex}"),
        format!("sha256:{}", "a".repeat(63)),
        format!("sha256:{}", "a".repeat(65)),
        format!("sha256:{}", "A".repeat(64)),
        format!("sha256:{}g", "a".repeat(63)),
    ] {
        assert!(DiffId::parse(&bad).is_err(), "{bad:?}");
    }
}

/// Versions compare numerically and write back normalised.
#[test]
fn versions_compare_numerically_and_normalise() {
    let v = |t: &str| DottedVersion::parse(t).unwrap();
    assert!(v("14.10") > v("14.9"));
    assert!(v("14.0.1") > v("14.0"));
    assert_eq!(v("14.0"), v("14.0.0"));
    assert_eq!(
        (
            v("27.1.3").major(),
            v("27.1.3").minor(),
            v("27.1.3").patch()
        ),
        (27, 1, 3)
    );
    assert_eq!(String::from(v("14.0.0")), "14.0");
    assert_eq!(String::from(v("14.2.0")), "14.2");
    assert_eq!(String::from(v("14.2.5")), "14.2.5");
    assert_eq!(v("0.0"), v("0.0.0"));
    for bad in [
        "",
        "14",
        "14.",
        ".1",
        "1.2.3.4",
        "01.2",
        "1.02",
        "1.-2",
        "+1.2",
        "1. 2",
        "a.b",
        "1234567890.1",
        "1..2",
    ] {
        assert!(DottedVersion::parse(bad).is_err(), "{bad:?}");
    }
}

/// Environment names: the grammar, the loader and sandbox prefixes, the names the
/// agent owns.
#[test]
fn env_names_are_checked() {
    for ok in [
        "PATH",
        "_X",
        "a1",
        "ZIG_LIB_DIR",
        "LDX",
        "DYLD",
        "HOMEX",
        "JS",
    ] {
        assert_eq!(check_env_name(ok), Ok(()), "{ok}");
    }
    for bad in ["", "1A", "A-B", "A B", "A=B", "\u{e9}"] {
        assert_eq!(check_env_name(bad), Err(EnvNameError::Malformed), "{bad:?}");
    }
    for forbidden in [
        "DYLD_FALLBACK_LIBRARY_PATH",
        "LD_PRELOAD",
        "SANDBOX_FOO",
        "JS_ANYTHING",
        "HOME",
        "TMPDIR",
        "CFFIXED_USER_HOME",
        "XDG_CACHE_HOME",
        "CLANG_MODULE_CACHE_PATH",
        "DEVELOPER_DIR",
    ] {
        assert_eq!(
            check_env_name(forbidden),
            Err(EnvNameError::Forbidden),
            "{forbidden}"
        );
    }
}

/// The network levels order from off to on, so a clamp is `min`.
#[test]
fn network_levels_are_ordered() {
    assert!(NetworkLevel::Off < NetworkLevel::Loopback);
    assert!(NetworkLevel::Loopback < NetworkLevel::On);
    assert_eq!(
        NetworkLevel::On.min(NetworkLevel::Loopback),
        NetworkLevel::Loopback
    );
}

/// An Xcode meets the need when its version is at least `min` and it has every
/// component.
#[test]
fn xcode_need_is_satisfied_by_version_and_components() {
    let need = golden().xcode.unwrap();
    let v = |t: &str| DottedVersion::parse(t).unwrap();
    let both = [
        XcodeComponent::Clang,
        XcodeComponent::Swift,
        XcodeComponent::Metal,
    ];
    assert!(need.satisfied_by(&v("27.1"), &both));
    assert!(need.satisfied_by(&v("28.0"), &[XcodeComponent::Swift, XcodeComponent::Clang]));
    assert!(!need.satisfied_by(&v("27.0.9"), &both));
    assert!(!need.satisfied_by(&v("27.1"), &[XcodeComponent::Clang]));
    assert!(!need.satisfied_by(&v("27.1"), &[]));
}
