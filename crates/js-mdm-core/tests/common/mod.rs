//! Shared fixtures. `config.example` is the neutral site every golden is rendered from.

#![allow(dead_code)]

use js_mdm_core::acme::Acme;
use js_mdm_core::config::MdmConfig;

/// The documented example settings, every key set.
pub const EXAMPLE: &str = include_str!("../../config.example");

/// The example, parsed.
pub fn config() -> MdmConfig {
    MdmConfig::parse(EXAMPLE).expect("the example is valid")
}

/// The example's ACME settings.
pub fn acme() -> Acme {
    Acme::parse(EXAMPLE).expect("the example is valid")
}

/// The keys the example sets, in file order.
pub fn example_keys() -> Vec<&'static str> {
    EXAMPLE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect()
}

/// The example with the line of `key` replaced by `KEY=value`.
pub fn with(key: &str, value: &str) -> String {
    let mut seen = false;
    let text: String = EXAMPLE
        .lines()
        .map(|l| match l.split_once('=') {
            Some((k, _)) if k == key => {
                seen = true;
                format!("{key}={value}\n")
            }
            _ => format!("{l}\n"),
        })
        .collect();
    assert!(seen, "the example sets {key}");
    text
}

/// The example without the line of `key`.
pub fn without(key: &str) -> String {
    let text: String = EXAMPLE
        .lines()
        .filter(|l| l.split_once('=').is_none_or(|(k, _)| k != key))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_ne!(text.len(), EXAMPLE.len(), "the example sets {key}");
    text
}

/// The example with only the keys that have no default: what a site must set.
pub fn minimal() -> String {
    const REQUIRED: [&str; 16] = [
        "MDM_DNS_NAME",
        "MDM_BIND",
        "MDM_LAN_CIDR",
        "MDM_USER",
        "MDM_STATE",
        "MDM_ETC",
        "MDM_BIN",
        "MDM_NANOHUB_PORT",
        "MDM_SCEP_PORT",
        "MDM_DEP_PORT",
        "MDM_MEMORY_MAX",
        "MDM_CPU_QUOTA",
        "MDM_TASKS_MAX",
        "MDM_DEP_NAME",
        "MDM_ORG",
        "MDM_PROFILE_ID",
    ];
    EXAMPLE
        .lines()
        .filter(|l| {
            l.split_once('=')
                .is_some_and(|(k, _)| REQUIRED.contains(&k))
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

/// A file under `tests/`.
pub fn read(path: &str) -> String {
    let full = format!("{}/tests/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

/// A file under `tests/`, as bytes.
pub fn read_bytes(path: &str) -> Vec<u8> {
    let full = format!("{}/tests/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}
