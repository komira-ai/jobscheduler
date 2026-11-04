//! The one job definition: a start named by its image, with a `run`, an `os` and an
//! `arch`, read the same way for both platforms; the refusals of the new fields; the
//! platform check against an image's config; and the legacy form a server writes for a host
//! that did not report `one-job`.

mod common;

use std::collections::BTreeMap;

use common::{expect, job, long};
use js_agent_wire::limits::{MAX_ARGS, MAX_ENV, MAX_PROGRAM_LEN};
use js_agent_wire::{
    Command, HeartbeatReply, ImagePlatform, InputRef, Isolation, JobSpec, Mount, NativeSpec,
    NetworkLevel, ONE_JOB_CAPABILITY, OutputKind, OutputSpec, Placement, Rule, RunSpec,
    check_platform, refusal,
};
use js_types::{Arch, Os};
use serde_json::json;

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The starts the previous release sends, one compact JSON object a line: a container
/// start, a native start and a native start with a bundle.
const PREVIOUS: &str = include_str!("data/previous_release_starts.jsonl");

fn previous(line: usize) -> (String, JobSpec) {
    let text = PREVIOUS.lines().nth(line).unwrap().to_owned();
    let spec = serde_json::from_str(&text).unwrap();
    (text, spec)
}

fn run() -> RunSpec {
    RunSpec {
        program: "@bundle/tool".into(),
        args: vec!["--version".into()],
        env: BTreeMap::from([("LANG".to_owned(), "C".to_owned())]),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
    }
}

/// The start of `common::job` named by its image: its command moved into a run, built for
/// `os` and `arch`.
fn start(os: Os, arch: Arch) -> JobSpec {
    let mut spec = job();
    spec.args = Vec::new();
    spec.run = Some(run());
    spec.os = Some(os);
    spec.arch = Some(arch);
    spec
}

fn no_caps() -> BTreeMap<String, String> {
    BTreeMap::new()
}

/// Asserts that one image-and-run start, decoded once with `os: linux` and once with
/// `os: macos`, validates and pins in both and round-trips, that `@bundle/tool` and an
/// absolute program are accepted on both, and that a run naming no platform is checked the
/// same way. Catches a `run` accepted only beside an empty image, and validation that reads
/// the platform.
#[test]
fn a_start_with_an_image_parses_on_both_platforms() {
    for os in ["linux", "macos"] {
        let wire = json!({
            "run_id": "run-1",
            "lease_id": "1-1",
            "image": format!("toolchain:1@sha256:{HEX}"),
            "run": {"program": "@bundle/tool", "args": ["--version"]},
            "os": os,
            "arch": "arm64",
            "limits": {"cpu_millis": 1000, "memory_bytes": 1024, "wall_ms": 60000}
        });
        let spec: JobSpec = serde_json::from_value(wire).unwrap();
        assert_eq!(spec.os.map(Os::name), Some(os));
        assert_eq!(spec.run.as_ref().unwrap().program, "@bundle/tool");
        assert_eq!(spec.native, None);
        assert_eq!(Command::Start(Box::new(spec.clone())).validate(), Ok(()));
        let text = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<JobSpec>(&text).unwrap(), spec);
        let mut absolute = spec.clone();
        absolute.run.as_mut().unwrap().program = "/usr/bin/make".into();
        assert_eq!(absolute.validate(), Ok(()), "{os}");
    }
    let mut unnamed = start(Os::Macos, Arch::Arm64);
    (unnamed.os, unnamed.arch) = (None, None);
    assert_eq!(unnamed.validate(), Ok(()));
    assert_eq!(unnamed.require_pinned(), Ok(()));
}

/// Asserts that a run beside an empty image is refused `invalid-image` naming `image`, and
/// the same with the `image` key left out. Catches a run that is taken as a job of its own,
/// which an older agent would read as an image start with no image.
#[test]
fn a_run_without_an_image_is_refused() {
    let mut spec = start(Os::Macos, Arch::Arm64);
    spec.image = String::new();
    expect(
        spec.validate().unwrap_err(),
        "image",
        Rule::Empty,
        refusal::INVALID_IMAGE,
    );
    let mut wire = serde_json::to_value(start(Os::Linux, Arch::X86_64)).unwrap();
    wire.as_object_mut().unwrap().remove("image");
    let decoded: JobSpec = serde_json::from_value(wire).unwrap();
    expect(
        decoded.validate().unwrap_err(),
        "image",
        Rule::Empty,
        refusal::INVALID_IMAGE,
    );
}

/// Asserts that a run beside a native body is refused `invalid-job` naming `run`, whether
/// the image is there or empty as a native start has it. Catches a start with two bodies
/// that an agent of either release would run as only one of them.
#[test]
fn run_beside_native_is_refused() {
    let mut spec = start(Os::Macos, Arch::Arm64);
    spec.native = Some(NativeSpec::from(run()));
    expect(
        spec.validate().unwrap_err(),
        "run",
        Rule::Inconsistent,
        refusal::INVALID_JOB,
    );
    spec.image = String::new();
    expect(
        spec.validate().unwrap_err(),
        "run",
        Rule::Inconsistent,
        refusal::INVALID_JOB,
    );
}

/// Asserts what else a run is held to: the job's own `args` beside it are refused naming
/// `args` (the command line is the run's), a legacy `bundle` beside it is refused as a
/// bundle with no native body, and the image must be pinned. Catches two command lines for
/// one job, and a run that names its bundle twice.
#[test]
fn a_run_is_the_only_command_line_and_names_its_bundle_by_the_image() {
    let mut args = start(Os::Linux, Arch::X86_64);
    args.args = vec!["make".into()];
    expect(
        args.validate().unwrap_err(),
        "args",
        Rule::Inconsistent,
        refusal::INVALID_JOB,
    );
    let mut bundle = start(Os::Macos, Arch::Arm64);
    bundle.bundle = Some(format!("toolchain:1@sha256:{HEX}"));
    expect(
        bundle.validate().unwrap_err(),
        "bundle",
        Rule::NeedsNative,
        refusal::INVALID_BUNDLE,
    );
    let mut tag = start(Os::Linux, Arch::X86_64);
    tag.image = "ubuntu:24.04".into();
    assert_eq!(tag.validate(), Ok(()));
    expect(
        tag.require_pinned().unwrap_err(),
        "image",
        Rule::NotPinned,
        refusal::IMAGE_DIGEST_REQUIRED,
    );
}

/// Asserts that the run body is held to the bounds of the native one, each fault reported
/// under its `run.*` field with the native body's code: a relative program, a program over
/// its cap, arguments at and over theirs, an argument with a NUL, a reserved and a bad
/// environment name, environment at and over its cap, a working directory that climbs, two
/// inputs at one place, an output inside an input and a stage id that is not a digest.
/// Catches a run body checked less than the native body it replaces, or reported under the
/// native body's names.
#[test]
fn a_run_body_is_held_to_the_native_bounds_under_its_own_names() {
    let good = start(Os::Linux, Arch::X86_64);
    let refused = |edit: Edit| {
        let mut spec = good.clone();
        edit(spec.run.as_mut().unwrap());
        spec.validate().unwrap_err()
    };
    let job_code = refusal::INVALID_JOB;
    let cases: [(Edit, &str, Rule, &str); 10] = [
        (
            &|run| run.program = "bin/tool".into(),
            "run.program",
            Rule::Escapes,
            refusal::INVALID_PROGRAM,
        ),
        (
            &|run| run.program = format!("/{}", long(MAX_PROGRAM_LEN)),
            "run.program",
            Rule::TooLong,
            refusal::INVALID_PROGRAM,
        ),
        (
            &|run| run.args = vec![String::new(); MAX_ARGS + 1],
            "run.args",
            Rule::TooMany,
            job_code,
        ),
        (
            &|run| run.args = vec!["a\0b".into()],
            "run.args",
            Rule::BadChars,
            job_code,
        ),
        (
            &|run| {
                run.env.insert("DYLD_INSERT_LIBRARIES".into(), "x".into());
            },
            "run.env",
            Rule::Reserved,
            refusal::INVALID_ENV,
        ),
        (
            &|run| {
                run.env.insert("1X".into(), "x".into());
            },
            "run.env",
            Rule::BadChars,
            refusal::INVALID_ENV,
        ),
        (
            &|run| {
                run.env = (0..=MAX_ENV)
                    .map(|n| (format!("V{n}"), String::new()))
                    .collect()
            },
            "run.env",
            Rule::TooMany,
            refusal::INVALID_ENV,
        ),
        (
            &|run| run.cwd = "a/../..".into(),
            "run.cwd",
            Rule::Escapes,
            job_code,
        ),
        (
            &|run| {
                run.inputs = vec![stage("in"), stage("IN")];
            },
            "run.inputs",
            Rule::Duplicate,
            job_code,
        ),
        (
            &|run| {
                run.inputs = vec![stage("in")];
                run.outputs = vec![OutputSpec {
                    path: "in/out".into(),
                    kind: OutputKind::File,
                }];
            },
            "run.outputs",
            Rule::Inconsistent,
            job_code,
        ),
    ];
    for (edit, field, rule, code) in cases {
        expect(refused(edit), field, rule, code);
    }
    let mut digest = stage("in");
    if let InputRef::Stage { stage_id, .. } = &mut digest {
        *stage_id = "sha256:abc".into();
    }
    expect(
        refused(&|run| run.inputs = vec![digest.clone()]),
        "run.inputs",
        Rule::BadChars,
        job_code,
    );
    let at_caps = |run: &mut RunSpec| {
        run.program = format!("/{}", long(MAX_PROGRAM_LEN - 1));
        run.args = vec![String::new(); MAX_ARGS];
        run.env = (1..=MAX_ENV)
            .map(|n| (format!("V{n}"), String::new()))
            .collect();
    };
    let mut spec = good.clone();
    at_caps(spec.run.as_mut().unwrap());
    assert_eq!(spec.validate(), Ok(()), "a run at every cap is accepted");
}

/// A change to a run body.
type Edit<'a> = &'a dyn Fn(&mut RunSpec);

fn stage(dest: &str) -> InputRef {
    InputRef::Stage {
        stage_id: format!("sha256:{HEX}"),
        dest: dest.into(),
        placement: Placement::ReadOnly,
    }
}

/// Asserts that `check_platform` accepts `linux`/`x86_64` against an image config of
/// `linux/amd64` and `macos`/`arm64` (sent as `aarch64`) against `darwin/arm64`; refuses
/// each of the four single-field mismatches with `platform-mismatch` naming the field, and
/// `os` first when both differ; passes a field the start leaves out; and that an image
/// platform outside the closed set, compared exactly with case, is refused naming the
/// image's field. Catches either side ignored, and a comparison that folds case or takes
/// the start's spelling for the image's.
#[test]
fn os_and_arch_must_match_the_image_platform() {
    let linux = ImagePlatform::from_oci("linux", "amd64").unwrap();
    assert_eq!(
        linux,
        ImagePlatform {
            os: Os::Linux,
            arch: Arch::X86_64
        }
    );
    let mac = ImagePlatform::from_oci("darwin", "arm64").unwrap();
    assert_eq!(
        mac,
        ImagePlatform {
            os: Os::Macos,
            arch: Arch::Arm64
        }
    );
    let mut sent = serde_json::to_value(start(Os::Macos, Arch::Arm64)).unwrap();
    sent["arch"] = json!("aarch64");
    let aliased: JobSpec = serde_json::from_value(sent).unwrap();
    assert_eq!(check_platform(&aliased, &mac), Ok(()));
    assert_eq!(
        check_platform(&start(Os::Linux, Arch::X86_64), &linux),
        Ok(())
    );
    let mismatches = [
        (start(Os::Macos, Arch::X86_64), linux, "os"),
        (start(Os::Linux, Arch::Arm64), linux, "arch"),
        (start(Os::Linux, Arch::Arm64), mac, "os"),
        (start(Os::Macos, Arch::X86_64), mac, "arch"),
        (start(Os::Macos, Arch::Arm64), linux, "os"),
    ];
    for (spec, image, field) in mismatches {
        expect(
            check_platform(&spec, &image).unwrap_err(),
            field,
            Rule::Inconsistent,
            refusal::PLATFORM_MISMATCH,
        );
    }
    assert_eq!(check_platform(&job(), &linux), Ok(()));
    assert_eq!(check_platform(&job(), &mac), Ok(()));
    let mut os_only = start(Os::Linux, Arch::X86_64);
    os_only.arch = None;
    let linux_arm = ImagePlatform {
        os: Os::Linux,
        arch: Arch::Arm64,
    };
    assert_eq!(check_platform(&os_only, &linux_arm), Ok(()));
    let mut arch_only = start(Os::Linux, Arch::Arm64);
    arch_only.os = None;
    assert_eq!(check_platform(&arch_only, &mac), Ok(()));
    let outside = [
        ("windows", "amd64", "image.os"),
        ("Linux", "amd64", "image.os"),
        ("macos", "arm64", "image.os"),
        ("", "arm64", "image.os"),
        ("linux", "x86_64", "image.architecture"),
        ("darwin", "aarch64", "image.architecture"),
        ("linux", "AMD64", "image.architecture"),
        ("linux", "riscv64", "image.architecture"),
    ];
    for (os, architecture, field) in outside {
        expect(
            ImagePlatform::from_oci(os, architecture).unwrap_err(),
            field,
            Rule::OutOfRange,
            refusal::PLATFORM_MISMATCH,
        );
    }
}

/// Asserts that a start sent with `arch: aarch64` decodes as `arm64` and re-encodes as
/// `arm64`, the one spelling, and that `arm64` itself round-trips unchanged. Catches a start
/// that refuses the alias, or echoes `aarch64` back to a peer that compares names.
#[test]
fn an_aarch64_start_decodes_and_reencodes_as_arm64() {
    let mut sent = serde_json::to_value(start(Os::Macos, Arch::Arm64)).unwrap();
    assert_eq!(sent["arch"], json!("arm64"));
    sent["arch"] = json!("aarch64");
    let decoded: JobSpec = serde_json::from_value(sent).unwrap();
    assert_eq!(decoded.arch, Some(Arch::Arm64));
    let again = serde_json::to_value(&decoded).unwrap();
    assert_eq!(again["arch"], json!("arm64"));
    assert_eq!(serde_json::from_value::<JobSpec>(again).unwrap(), decoded);
}

/// Asserts that an `os` or `arch` this version cannot name fails to decode, and inside a
/// reply reads as an unknown command while the stop beside it still arrives; an image
/// start without either decodes as a legacy one. Catches a start for a platform the agent
/// cannot name that runs anyway, and one that costs the commands beside it.
#[test]
fn an_unknown_platform_reads_as_an_unknown_command() {
    for (key, value) in [("os", "windows"), ("arch", "amd64"), ("os", "Linux")] {
        let mut wire =
            serde_json::to_value(Command::Start(Box::new(start(Os::Linux, Arch::X86_64)))).unwrap();
        wire[key] = json!(value);
        assert!(serde_json::from_value::<JobSpec>(wire.clone()).is_err());
        let reply: HeartbeatReply = serde_json::from_value(json!({
            "commands": [wire, {"type": "stop", "run_id": "r2"}]
        }))
        .unwrap();
        assert_eq!(
            reply.commands,
            [
                Command::Unknown,
                Command::Stop {
                    run_id: "r2".into()
                }
            ],
            "{key}={value}"
        );
    }
    let legacy = serde_json::to_value(job()).unwrap();
    assert!(legacy.get("os").is_none() && legacy.get("arch").is_none());
    assert!(legacy.get("run").is_none());
    let decoded: JobSpec = serde_json::from_value(legacy).unwrap();
    assert_eq!((decoded.os, decoded.arch, decoded.run), (None, None, None));
}

/// Asserts that for a host without `one-job` a run beside an image built for `macos`
/// becomes exactly the previous release's native start with a bundle (the frozen fixture,
/// byte for byte); that a host that reported `one-job` as `true` gets the start unchanged
/// and one that reported another value does not; that every previous-release start passes
/// unchanged and an image start without a run only loses `os` and `arch`; and that a start
/// with no legacy spelling is refused `invalid-job`: a run beside a `linux` image or one that
/// names no `os`, naming `run`, and a run beside a mount, naming `mounts`. Catches a run
/// sent to an older agent that would run the image's own command instead, and a downgrade
/// that changes a legacy start.
#[test]
fn downgrade_for_a_host_without_one_job_uses_the_legacy_form() {
    let (bundle_text, bundle_start) = previous(2);
    let mut new = bundle_start.clone();
    new.image = new.bundle.take().unwrap();
    new.run = Some(RunSpec::from(new.native.take().unwrap()));
    (new.os, new.arch) = (Some(Os::Macos), Some(Arch::Arm64));
    assert_eq!(new.validate(), Ok(()));
    assert_eq!(new.require_pinned(), Ok(()));
    let down = new.downgrade_for(&no_caps()).unwrap();
    assert_eq!(serde_json::to_string(&down).unwrap(), bundle_text);
    assert_eq!(down, bundle_start);
    let one_job = |value: &str| BTreeMap::from([(ONE_JOB_CAPABILITY.to_owned(), value.to_owned())]);
    assert_eq!(new.downgrade_for(&one_job("true")), Ok(new.clone()));
    assert_eq!(new.downgrade_for(&one_job("yes")), Ok(bundle_start));
    for line in 0..3 {
        let (_, spec) = previous(line);
        assert_eq!(
            spec.downgrade_for(&no_caps()),
            Ok(spec.clone()),
            "line {line}"
        );
    }
    let (_, container) = previous(0);
    let mut named = container.clone();
    (named.os, named.arch) = (Some(Os::Linux), Some(Arch::X86_64));
    assert_eq!(named.downgrade_for(&no_caps()), Ok(container));
    let mut linux = new.clone();
    linux.os = Some(Os::Linux);
    let mut unnamed = new.clone();
    unnamed.os = None;
    for spec in [linux, unnamed] {
        assert_eq!(spec.validate(), Ok(()));
        expect(
            spec.downgrade_for(&no_caps()).unwrap_err(),
            "run",
            Rule::Inconsistent,
            refusal::INVALID_JOB,
        );
    }
    let mut mounted = new.clone();
    mounted.mounts = vec![Mount {
        root_name: "cache".into(),
        rel: "sdk".into(),
        ro: true,
    }];
    assert_eq!(mounted.validate(), Ok(()));
    expect(
        mounted.downgrade_for(&no_caps()).unwrap_err(),
        "mounts",
        Rule::Inconsistent,
        refusal::INVALID_JOB,
    );
    let mut host = new.clone();
    host.isolation = Isolation::Host;
    expect(
        host.downgrade_for(&no_caps()).unwrap_err(),
        "isolation",
        Rule::Inconsistent,
        refusal::INVALID_JOB,
    );
}
