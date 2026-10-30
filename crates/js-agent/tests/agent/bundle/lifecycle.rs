//! A start that names a bundle, through the lifecycle: refused with a class on an agent
//! without a store, judged by the bundle's rules on one with a store, and, when every rule
//! passes, ending at the refusal that nothing can run a bundle yet. Each test says the defect
//! it catches.

use clap::Parser;
use js_agent::bundle::{Admission, Needs, Settings};
use js_agent::{Cli, Platform};
use js_agent_wire::refusal::{self, RefusalClass, bundle_class};
use js_agent_wire::{JobSpec, NativeSpec, Outcome};
use js_bundle::DottedVersion;

use super::fake::{FakeHost, digest};
use crate::lifecycle::{Harness, Options, job, names};
use crate::support::platform;

fn mac() -> Platform {
    platform("macos", "aarch64")
}

/// A native start of `@bundle/clang` that pins the bundle `a`.
fn bundled(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.image = String::new();
    spec.args = Vec::new();
    spec.native = Some(NativeSpec {
        program: "@bundle/clang".to_owned(),
        args: Vec::new(),
        env: Default::default(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: js_agent_wire::NetworkLevel::Off,
    });
    spec.bundle = Some(format!("toolchain@{}", digest('a')));
    spec
}

/// The rules of a farm host that holds the bundle `a` and lists it.
fn admission(listed: bool) -> Admission {
    let host = FakeHost::new();
    let allowed = if listed { digest('a') } else { digest('b') };
    host.file("/etc/js/allow", &format!("{allowed}\n"));
    host.set(|s| {
        s.held = Ok(vec![digest('a')]);
        s.needs.insert(
            digest('a'),
            Needs {
                os_version: DottedVersion::parse("14.0").unwrap(),
                xcode: None,
            },
        );
    });
    let args = [
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--bundle-store",
        "/Volumes/work/bundles",
        "--scratch-root",
        "/Volumes/work/scratch",
        "--bundle-budget-gib",
        "10",
        "--bundle-allow-file",
        "/etc/js/allow",
    ];
    let cli = Cli::try_parse_from(args).unwrap();
    let settings = Settings::from_cli(&cli, &mac(), &*host).unwrap().unwrap();
    Admission::new(settings, host)
}

fn harness(name: &str, bundle: Option<Admission>) -> Harness {
    Harness::with(
        name,
        Options {
            bundle,
            ..Options::default()
        },
    )
}

/// Nothing was created: no container, no scratch directory, no booking; and the result is a
/// held `failed_to_start` that carries `code`.
fn assert_refused_cleanly(h: &Harness, run_id: &str, code: &str) {
    assert!(h.log.borrow().is_empty());
    assert!(!h.scratch_of(run_id).exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(code));
}

/// An agent with no bundle store refuses a start that names a bundle `bundle-unsupported`
/// (the farm's fault: the job should not have been placed here) and creates nothing. It
/// catches an agent that would run the program with the host's tools while the job asked for
/// a toolchain, and a refusal that reads as the request's fault.
#[test]
fn an_agent_without_a_store_refuses_a_bundle_with_a_class() {
    let mut h = harness("bundle-unsupported", None);
    let events = h.start(bundled("r1"));
    assert_eq!(names(&events), ["refused r1 bundle-unsupported"]);
    assert_refused_cleanly(&h, "r1", refusal::BUNDLE_UNSUPPORTED);
    assert_eq!(
        bundle_class(refusal::BUNDLE_UNSUPPORTED),
        Some(RefusalClass::Farm)
    );
}

/// An agent with a store judges the start by the bundle's rules: a digest off the allow list
/// is refused `bundle-not-allowed` and creates nothing. It catches a store whose rules are
/// configured and not applied.
#[test]
fn an_agent_with_a_store_applies_the_bundle_rules() {
    let mut h = harness("bundle-not-allowed", Some(admission(false)));
    let events = h.start(bundled("r1"));
    assert_eq!(names(&events), ["refused r1 bundle-not-allowed"]);
    assert_refused_cleanly(&h, "r1", refusal::BUNDLE_NOT_ALLOWED);
}

/// A start that passes every rule of the bundle reaches the refusal that this agent cannot
/// run a bundle yet (`native-unsupported`), says in its explanation that the bundle was
/// admitted, and creates nothing; the agent that has no store and a native job with no
/// bundle keep the plain explanation. It catches a job started as something it is not, and a
/// refusal that hides which check the start passed.
#[test]
fn an_admitted_bundle_ends_at_the_point_where_nothing_can_run_it() {
    let mut h = harness("bundle-admitted", Some(admission(true)));
    let events = h.start(bundled("r1"));
    assert_eq!(names(&events), ["refused r1 native-unsupported"]);
    assert_refused_cleanly(&h, "r1", refusal::NATIVE_UNSUPPORTED);
    let js_agent::lifecycle::Event::Refused { why, .. } = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(
        why,
        "the bundle is admitted, but this agent cannot run a bundle yet"
    );
    let mut plain = harness("native-plain", Some(admission(true)));
    let mut spec = bundled("r2");
    spec.bundle = None;
    spec.native.as_mut().unwrap().program = "/usr/bin/true".to_owned();
    let events = plain.start(spec);
    let js_agent::lifecycle::Event::Refused { why, .. } = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(why, "this agent runs no native jobs");
}
