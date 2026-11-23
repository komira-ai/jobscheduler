//! How a start follows from the job's operating system: a macOS job that names a bundle and no
//! execution mode is native, a mode the job names is honoured over its system, and every start,
//! native or container, carries the job's system and architecture.

use super::*;

/// A macOS job that names a bundle and no execution mode (as a client that sends only the
/// system and the architecture submits it) is started natively, with the same start as the one
/// that names `sandbox-exec`: the bundle's entrypoint as the run's program, no command, and the
/// system and architecture set. It catches a macOS bundle sent to a Mac as a container command,
/// which is how it was built while only `sandbox-exec` made a job native.
#[test]
fn a_macos_job_naming_a_bundle_and_no_exec_mode_starts_natively() {
    let (mut job, allocation) = native_records("reg/kbf-task");
    let named = native_start(&job, &allocation);
    job.spec.constraints.exec = None;
    let start = native_start(&job, &allocation);
    let run = start.run.as_ref().expect("a native start has a run");
    assert_eq!(run.program, "@bundle/kbf-task");
    assert_eq!(run.args, ["--task", "build"]);
    assert!(start.command.is_empty());
    assert_eq!((start.os, start.arch), (Some(Os::Macos), Some(Arch::Arm64)));
    assert_eq!(start, named);
}

/// A job that names `sandbox-exec` and a bundle, as an older client sends it, is native whatever
/// its constraints say of the system: none, or one this version does not know. It catches an
/// older client's native job turned into a container start because it names no macOS.
#[test]
fn a_job_naming_sandbox_exec_is_native_whatever_its_system() {
    for os in [None, Some("plan9")] {
        let (mut job, allocation) = native_records("reg/kbf-task");
        job.spec.constraints.os = os.map(str::to_owned);
        let start = native_start(&job, &allocation);
        assert!(start.run.is_some(), "{os:?}");
        assert!(start.command.is_empty(), "{os:?}");
        assert_eq!(start.os, None, "{os:?}");
    }
}

/// A container start carries the job's system and architecture as a native one does: a Linux
/// job on x86_64, and a macOS job on arm64 that names `container`, whose start stays a
/// container start. A job that names neither leaves both out. It catches a container start
/// that drops what an agent of more than one platform would choose its runtime from.
#[test]
fn a_container_start_carries_the_jobs_system_and_architecture() {
    let cases = [
        (
            Some("linux"),
            Some("x86_64"),
            None,
            (Some(Os::Linux), Some(Arch::X86_64)),
        ),
        (
            Some("macos"),
            Some("arm64"),
            Some(ExecMode::Container),
            (Some(Os::Macos), Some(Arch::Arm64)),
        ),
        (None, None, None, (None, None)),
    ];
    for (os, arch, exec, carried) in cases {
        let (mut job, allocation) = records();
        job.spec.constraints.os = os.map(str::to_owned);
        job.spec.constraints.arch = arch.map(str::to_owned);
        job.spec.constraints.exec = exec;
        let start = build(&job, &allocation, &allow_list()).unwrap();
        assert_eq!(start.run, None, "{os:?}");
        assert_eq!(start.command, ["run", "--fast"], "{os:?}");
        assert_eq!((start.os, start.arch), carried, "{os:?}");
    }
}

/// A container start names the system and the architecture only in the spellings every agent
/// that has the fields decodes, `linux` and `x86_64` on the wire: an architecture constraint
/// in another spelling (`amd64`, which submit stores as given) and a system in another case
/// (`Linux`, which submit refuses) leave the field out, and `aarch64` is sent as `arm64`. An
/// agent of the release on the build nodes reads a start with a value it does not know as a
/// command it does not know and runs nothing. It catches a constraint's text copied onto the
/// wire.
#[test]
fn a_container_start_names_its_platform_only_in_the_spellings_agents_decode() {
    let (mut job, allocation) = records();
    job.spec.constraints.os = Some("linux".to_owned());
    job.spec.constraints.arch = Some("x86_64".to_owned());
    let start = build(&job, &allocation, &allow_list()).unwrap();
    let wire = serde_json::to_value(&start).unwrap();
    assert_eq!(
        (&wire["os"], &wire["arch"]),
        (&serde_json::json!("linux"), &serde_json::json!("x86_64"))
    );
    for (os, arch, carried) in [
        ("Linux", "amd64", (None, None)),
        ("linux", "aarch64", (Some(Os::Linux), Some(Arch::Arm64))),
    ] {
        job.spec.constraints.os = Some(os.to_owned());
        job.spec.constraints.arch = Some(arch.to_owned());
        let start = build(&job, &allocation, &allow_list()).unwrap();
        assert_eq!((start.os, start.arch), carried, "{os} {arch}");
        let wire = serde_json::to_value(&start).unwrap();
        let named = carried.1.map(|_| serde_json::json!("arm64"));
        assert_eq!(wire.get("arch").cloned(), named, "{os} {arch}");
    }
}
