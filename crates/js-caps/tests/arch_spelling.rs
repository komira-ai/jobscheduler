//! One spelling of 64-bit Arm: `arm64`, with `aarch64` read as it in a hello and in a
//! request, and always printed as `arm64`.

use js_caps::{Arch, ArmVersion, NodeCaps, ReportError, Request, UnknownArch, Unmet};

fn report(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    NodeCaps::from_report(entries.iter().copied())
}

fn req(props: &[(&str, &str)]) -> Request {
    Request::parse(props.iter().copied()).unwrap()
}

/// An Arm node's report: the architecture as spelt by `arch`, the features of
/// `armv8.2-a`, Linux.
fn arm_report(arch: &str) -> Result<NodeCaps, ReportError> {
    let mut entries = vec![("arch", arch), ("os", "linux"), ("cpus", "4")];
    for version in &ArmVersion::ALL[..3] {
        entries.extend(version.adds().iter().map(|f| ("cpu.features", *f)));
    }
    report(&entries)
}

/// Asserts that a hello from an agent that reports `aarch64` seats as `arm64`: the node's
/// architecture is `Arch::Arm64`, an `arch=arm64` request matches it, an `arch=x86_64`
/// one does not, and the same report spelt `arm64` gives an identical node. Catches a
/// hello path that refuses `aarch64` as an unknown architecture, or keeps it apart from
/// `arm64`, either of which leaves the host of an older agent unused by `arm64` work.
#[test]
fn aarch64_is_normalised_in_a_hello() {
    let old = arm_report("aarch64").unwrap();
    assert_eq!(old.cpu.arch(), Arch::Arm64);
    assert!(req(&[("arch", "arm64")]).matches(&old));
    assert!(req(&[("arch", "arm64"), ("isa_level", "armv8.2-a")]).matches(&old));
    assert!(!req(&[("arch", "x86_64")]).matches(&old));
    let new = arm_report("arm64").unwrap();
    assert_eq!(old, new);
    assert_eq!(
        report(&[("arch", "sparc")]),
        Err(ReportError::UnknownArch(UnknownArch("sparc".to_owned())))
    );
}

/// Asserts that a request may name `arch=aarch64`, that it is the request `arch=arm64`
/// (equal, matching the same node, and written back and shown unmet as `arch=arm64`).
/// Catches a request parser that refuses the alias or echoes it, which would make one
/// requirement read two ways.
#[test]
fn a_request_for_aarch64_is_a_request_for_arm64() {
    let alias = req(&[("arch", "aarch64")]);
    assert_eq!(alias, req(&[("arch", "arm64")]));
    assert_eq!(alias.properties(), vec![("arch", "arm64".to_owned())]);
    assert!(alias.matches(&arm_report("arm64").unwrap()));
    let x86 = report(&[("arch", "x86_64")]).unwrap();
    let unmet = alias.unmet(&x86);
    assert_eq!(unmet, [Unmet::Arch { want: Arch::Arm64 }]);
    assert_eq!(unmet[0].to_string(), "arch=arm64");
}
