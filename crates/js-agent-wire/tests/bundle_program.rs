//! The program of a native job may name an entrypoint of the job's bundle, and the refusals
//! of a bundle start carry a class. Each test says the defect it catches.

mod common;

use common::{expect, job};
use js_agent_wire::refusal::{self, RefusalClass, bundle_class};
use js_agent_wire::{
    BUNDLE_ENTRYPOINT_MAX, BUNDLE_PROGRAM_PREFIX, Isolation, JobSpec, NativeSpec, Rule,
};
use serde_json::json;

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A native start of `program`, with a bundle when `bundled`.
fn spec(program: &str, bundled: bool) -> JobSpec {
    let mut spec = job();
    spec.image = String::new();
    spec.args = Vec::new();
    spec.gpu_indices = Vec::new();
    spec.mounts = Vec::new();
    spec.isolation = Isolation::Contained;
    spec.native =
        Some(serde_json::from_value::<NativeSpec>(json!({ "program": program })).unwrap());
    spec.bundle = bundled.then(|| format!("toolchain@sha256:{HEX}"));
    spec
}

/// Entrypoint names that are accepted beside a bundle: letters, digits and `. _ + -`, the
/// first a letter or digit, up to the cap. It catches an entrypoint of a real tool
/// (`c++`, `clang-18`, `ld.lld`) that is refused.
#[test]
fn a_bundle_entrypoint_program_is_accepted_beside_a_bundle() {
    let longest = format!(
        "{BUNDLE_PROGRAM_PREFIX}{}",
        "a".repeat(BUNDLE_ENTRYPOINT_MAX)
    );
    let names = [
        "@bundle/clang",
        "@bundle/c++",
        "@bundle/clang-18",
        "@bundle/ld.lld",
        "@bundle/Tool_1",
        "@bundle/0",
        longest.as_str(),
    ];
    for program in names {
        let spec = spec(program, true);
        assert_eq!(spec.validate(), Ok(()), "{program}");
        assert_eq!(spec.require_pinned(), Ok(()), "{program}");
    }
}

/// Entrypoint names that are refused with `invalid-program` naming `native.program`: empty,
/// a path, a climb, a leading dot, dash or plus, a space, a NUL, a name over the cap. It
/// catches an entrypoint that reaches outside the bundle's table or smuggles a separator.
#[test]
fn a_malformed_bundle_entrypoint_is_refused() {
    let long = format!(
        "{BUNDLE_PROGRAM_PREFIX}{}",
        "a".repeat(BUNDLE_ENTRYPOINT_MAX + 1)
    );
    let cases = [
        ("@bundle/", Rule::Empty),
        ("@bundle/a/b", Rule::BadChars),
        ("@bundle/..", Rule::BadChars),
        ("@bundle/../x", Rule::BadChars),
        ("@bundle/.hidden", Rule::BadChars),
        ("@bundle/-x", Rule::BadChars),
        ("@bundle/+x", Rule::BadChars),
        ("@bundle/a b", Rule::BadChars),
        ("@bundle/a\0b", Rule::BadChars),
        (long.as_str(), Rule::TooLong),
    ];
    for (program, rule) in cases {
        let err = spec(program, true).validate().unwrap_err();
        expect(err, "native.program", rule, refusal::INVALID_PROGRAM);
    }
}

/// A program inside a bundle in a job that has no bundle is refused `invalid-program`,
/// `Inconsistent`: the agent would otherwise look for it on the host. A host path beside a
/// bundle stays valid (a host tool next to the bundle's), and `@bundle` without the slash
/// is an ordinary relative path, refused as one. It catches a bundle program that runs
/// without its bundle, and a host program that a bundle forbids.
#[test]
fn a_bundle_program_needs_its_bundle_and_a_host_program_does_not() {
    let err = spec("@bundle/clang", false).validate().unwrap_err();
    expect(
        err,
        "native.program",
        Rule::Inconsistent,
        refusal::INVALID_PROGRAM,
    );
    assert_eq!(spec("/usr/bin/clang", true).validate(), Ok(()));
    let err = spec("@bundle", true).validate().unwrap_err();
    expect(
        err,
        "native.program",
        Rule::Escapes,
        refusal::INVALID_PROGRAM,
    );
}

/// The class of each refusal of a bundle start: the node's and the rollout's faults are the
/// farm's, a malformed request is the request's, and any other code has no class. It catches
/// a refusal that would be cached as the action's result, and one that would be retried
/// forever.
#[test]
fn every_bundle_refusal_has_the_class_the_design_gives_it() {
    let farm = [
        refusal::BUNDLE_UNSUPPORTED,
        refusal::BUNDLE_PENDING,
        refusal::BUNDLE_NOT_HELD,
        refusal::BUNDLE_NO_SPACE,
        refusal::BUNDLE_OS_TOO_OLD,
        refusal::BUNDLE_XCODE_MISMATCH,
        refusal::BUNDLE_NOT_ALLOWED,
        refusal::BUNDLE_DENIED,
        refusal::BUNDLE_REGISTRY_NOT_ALLOWED,
        refusal::BUNDLE_FETCH_FAILED,
    ];
    let request = [
        refusal::BUNDLE_DIGEST_REQUIRED,
        refusal::INVALID_BUNDLE,
        refusal::BUNDLE_INDEX,
        refusal::INVALID_ENV,
    ];
    for code in farm {
        assert_eq!(bundle_class(code), Some(RefusalClass::Farm), "{code}");
    }
    for code in request {
        assert_eq!(bundle_class(code), Some(RefusalClass::Request), "{code}");
    }
    for code in [
        refusal::INVALID_IMAGE,
        refusal::NATIVE_UNSUPPORTED,
        "",
        "bundle",
    ] {
        assert_eq!(bundle_class(code), None, "{code:?}");
    }
}

/// The spelling of each new code, which a deployed peer matches on. It catches a rename.
#[test]
fn the_bundle_refusal_codes_are_spelled_as_published() {
    let codes = [
        refusal::BUNDLE_UNSUPPORTED,
        refusal::BUNDLE_PENDING,
        refusal::BUNDLE_NOT_HELD,
        refusal::BUNDLE_NO_SPACE,
        refusal::BUNDLE_OS_TOO_OLD,
        refusal::BUNDLE_XCODE_MISMATCH,
        refusal::BUNDLE_NOT_ALLOWED,
        refusal::BUNDLE_DENIED,
        refusal::BUNDLE_REGISTRY_NOT_ALLOWED,
        refusal::BUNDLE_FETCH_FAILED,
        refusal::BUNDLE_INDEX,
    ];
    assert_eq!(
        codes.join(" "),
        "bundle-unsupported bundle-pending bundle-not-held bundle-no-space bundle-os-too-old \
         bundle-xcode-mismatch bundle-not-allowed bundle-denied bundle-registry-not-allowed \
         bundle-fetch-failed bundle-index"
    );
}
