//! The `sysctl hw.optional` reader, through its public entry point.

use std::collections::BTreeSet;

use js_caps::{CpuCaps, ParseError};

/// Catches: a parser that accepts a line it cannot read (a missing value, a
/// foreign prefix, a non-integer) instead of naming it, or that lets a repeated
/// key silently overwrite the first.
#[test]
fn rejects_malformed_lines() {
    for bad in [
        "hw.optional.neon",
        "kern.version: 1",
        "hw.optional.neon: yes",
        "hw.optional.: 1",
    ] {
        assert!(
            matches!(
                CpuCaps::from_macos_sysctl(bad),
                Err(ParseError::SysctlLine { line: 1, .. })
            ),
            "{bad:?}"
        );
    }
    assert_eq!(
        CpuCaps::from_macos_sysctl("hw.optional.neon: 1\nhw.optional.neon: 0").unwrap_err(),
        ParseError::SysctlDuplicate("hw.optional.neon".to_owned())
    );
}

/// Catches: a feature set that drops a `1` key, keeps a `0` key, or loses the
/// kernel-name aliases of a key; and a blank line taken for a malformed one.
#[test]
fn reads_features_and_aliases() {
    let got = CpuCaps::from_macos_sysctl(
        "hw.optional.arm64: 1\nhw.optional.neon: 1\n\nhw.optional.arm.FEAT_AES: 1\n\
         hw.optional.arm.FEAT_SME: 0\nhw.optional.unknown_key: 1\n",
    )
    .unwrap();
    let want: BTreeSet<String> = ["arm64", "neon", "asimd", "FEAT_AES", "aes", "unknown_key"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(got.features(), &want);
}

/// Catches: treating an Intel Mac (or a truncated capture) as arm64.
#[test]
fn requires_arm64() {
    assert_eq!(
        CpuCaps::from_macos_sysctl("hw.optional.avx2_0: 1\n").unwrap_err(),
        ParseError::NotArm64Mac
    );
}
