//! Small public accessors and a parser branch no other test of this crate reaches: the
//! drivers a node runs, the exact value a request names, and blank lines in macOS
//! `sysctl` text.

use js_caps::{CpuCaps, NodeCaps, Request};

const SYSCTL: &str = include_str!("fixtures/apple_m3_ultra.sysctl");

/// Catches: `with_drivers` dropping a driver or replacing the set rather than adding to it.
#[test]
fn with_drivers_adds_to_the_drivers_a_node_runs() {
    let cpu = CpuCaps::from_macos_sysctl(SYSCTL).unwrap();
    let node = NodeCaps::new(cpu);
    assert!(node.drivers.is_empty());
    let node = node
        .with_drivers(["container", "native"])
        .with_drivers(["native", "vm"]);
    let drivers: Vec<&str> = node.drivers.iter().map(String::as_str).collect();
    assert_eq!(drivers, ["container", "native", "vm"]);
}

/// Catches: `exact` returning a value for a key the request does not name, or none for
/// one it does.
#[test]
fn a_request_reports_the_exact_value_it_names() {
    let request = Request::parse([("os", "linux"), ("cpus", "4")]).unwrap();
    assert_eq!(request.exact("os"), Some("linux"));
    assert_eq!(request.exact("cpu.model"), None);
    assert_eq!(request.exact("cpus"), None);
}

/// Catches: a malformed `sysctl` line accepted, or its error lost on the way out of the
/// feature reader.
#[test]
fn a_malformed_sysctl_line_is_refused() {
    let spoiled = format!("{SYSCTL}hw.optional.bogus 1\n");
    assert!(CpuCaps::from_macos_sysctl(&spoiled).is_err());
    assert!(js_caps::gpus_from_macos_sysctl(&spoiled).is_err());
}

/// Catches: blank lines in `sysctl` text refused, or read as an entry.
#[test]
fn blank_lines_in_sysctl_text_are_skipped() {
    let spaced = format!("\n  \n{}\n\n", SYSCTL.replace('\n', "\n\n"));
    assert_eq!(
        CpuCaps::from_macos_sysctl(&spaced).unwrap(),
        CpuCaps::from_macos_sysctl(SYSCTL).unwrap()
    );
}
