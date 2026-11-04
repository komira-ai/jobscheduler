//! The start a job start is made into: every field the job start names arrives in it, and
//! nothing the job start does not name is invented.

use std::collections::BTreeMap;

use js_agent_wire::{BookedResources, Isolation, JobStartSpec, NetworkLevel, RunFile, RunSpec};
use js_types::{Arch, Os};

use super::run_of;

/// A job start with every optional field set to a value other than its default.
fn full() -> JobStartSpec {
    JobStartSpec {
        job_id: "j1".to_owned(),
        allocation_id: "0123456789abcdef0123456789abcdef".to_owned(),
        image: "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        command: vec!["--go".to_owned()],
        run: Some(RunSpec {
            program: "/usr/bin/make".to_owned(),
            args: vec!["all".to_owned()],
            env: BTreeMap::new(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::Off,
        }),
        os: Some(Os::Macos),
        arch: Some(Arch::Arm64),
        env: BTreeMap::from([("LANG".to_owned(), "C".to_owned())]),
        files: vec![RunFile {
            name: "grant".to_owned(),
            contents: "token".to_owned(),
        }],
        booked: BookedResources {
            cpu_millis: 1500,
            memory_bytes: 2 << 30,
            disk_bytes: Some(1 << 30),
            gpu_indices: vec![1],
        },
        wall_ms: 10_000,
    }
}

/// Asserts that the start made from a job start keeps its `os`, `arch` and `run` as they
/// are, both platforms, and with them absent; that the job id and the allocation id become
/// the run id and the lease id, the command the arguments, and the booked resources and wall
/// limit the limits; and that it is contained, has no mounts, labels, bundle or native body,
/// and no `valid_for_ms` (no lease bounds it). It catches a field the conversion drops (an
/// image's platform lost on the way to the runtime) and a lease bound applied to a job.
#[test]
fn a_job_start_becomes_a_start_with_every_field_it_names() {
    let spec = full();
    let run = run_of(&spec);
    assert_eq!((run.os, run.arch), (Some(Os::Macos), Some(Arch::Arm64)));
    assert_eq!(run.run, spec.run);
    assert_eq!(run.run_id, "j1");
    assert_eq!(run.lease_id, spec.allocation_id);
    assert_eq!(run.image, spec.image);
    assert_eq!(run.args, ["--go"]);
    assert_eq!(run.files, spec.files);
    assert_eq!(run.limits.cpu_millis, 1500);
    assert_eq!(run.limits.memory_bytes, 2 << 30);
    assert_eq!(run.limits.disk_bytes, Some(1 << 30));
    assert_eq!(run.limits.pids, None);
    assert_eq!(run.limits.wall_ms, 10_000);
    assert_eq!(run.gpu_indices, [1]);
    assert_eq!(run.valid_for_ms, 0);
    assert_eq!(run.heartbeat_seq, 0);
    assert_eq!(run.isolation, Isolation::Contained);
    assert!(run.mounts.is_empty());
    assert!(run.labels.is_empty());
    assert_eq!((run.native, run.bundle), (None, None));

    let mut other = full();
    other.os = Some(Os::Linux);
    other.arch = Some(Arch::X86_64);
    let run = run_of(&other);
    assert_eq!((run.os, run.arch), (Some(Os::Linux), Some(Arch::X86_64)));

    let mut bare = full();
    bare.os = None;
    bare.arch = None;
    bare.run = None;
    let run = run_of(&bare);
    assert_eq!((run.os, run.arch, run.run), (None, None, None));
}
