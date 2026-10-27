//! The arguments of `create`: the pure functions that turn a job into flags.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use js_agent::cli_runtime::{
    ArgsError, DEFAULT_PIDS, Engine, SCRATCH_TARGET, hardening_args, label_args, limits_to_args,
};

use super::{mount, settings, spec};

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The limits of a spec without a GPU or a mount, plus the scratch mount every job has.
fn base(cpus: &str, memory: &str, pids: &str) -> Vec<String> {
    strings(&[
        "--cpus",
        cpus,
        "--memory",
        memory,
        "--memory-swap",
        memory,
        "--pids-limit",
        pids,
        "--mount",
        "type=bind,src=/work/scratch/r1,dst=/scratch",
    ])
}

/// Asserts one row for each limit: CPUs as exact decimals from thousandths, memory with
/// swap pinned to the same number (so the job cannot spill into swap), and the process
/// limit as given, or the default when the job names none. It catches a limit that is
/// missing from the flags, which lets the container exceed it, a swap allowance that
/// lets memory grow past its limit, and a CPU count rounded or scaled wrongly.
#[test]
fn limits_to_args_table() {
    let row = |cpu_millis, memory_bytes, pids| {
        let mut job = spec("r1");
        (job.cpu_millis, job.memory_bytes, job.pids) = (cpu_millis, memory_bytes, pids);
        job
    };
    let default_pids = DEFAULT_PIDS.to_string();
    let rows = [
        (
            "whole cpu",
            row(1000, 1 << 30, Some(100)),
            base("1.000", "1073741824", "100"),
        ),
        (
            "fraction",
            row(250, 1 << 30, Some(100)),
            base("0.250", "1073741824", "100"),
        ),
        (
            "many cpus",
            row(64_000, 1 << 30, Some(100)),
            base("64.000", "1073741824", "100"),
        ),
        (
            "small memory",
            row(1500, 6 << 20, Some(1)),
            base("1.500", "6291456", "1"),
        ),
        (
            "no pid bound still bounded",
            row(1500, 1 << 30, None),
            base("1.500", "1073741824", &default_pids),
        ),
    ];
    for (name, job, expected) in rows {
        for engine in [Engine::Docker, Engine::Podman] {
            assert_eq!(limits_to_args(engine, &job).unwrap(), expected, "{name}");
        }
    }
}

/// Asserts that every contained job gets the hardening whatever it asked for: all
/// capabilities dropped, no new privileges, no network by default, a read-only root with
/// a tmpfs `/tmp`, a user that is not root, and no `--rm` (the lifecycle removes the
/// container after it has read the result). It catches a flag dropped from the list, a
/// default network that reaches out, and a container that removes itself before its
/// result is read.
#[test]
fn hardening_flags_always_present() {
    let args = hardening_args(&settings());
    assert_eq!(
        args,
        strings(&[
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--network",
            "none",
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--user",
            "65534:65534"
        ])
    );
    assert!(!args.iter().any(|arg| arg == "--rm"));
}

/// Asserts that the Docker flag names the exact indices and nothing else, that Podman
/// is given one CDI device for each index, that no job without a GPU gets a GPU flag,
/// and that no spelling of "all" is ever produced. It catches a job given every GPU of
/// the host, and an index list in the wrong shape for the runtime that reads it.
#[test]
fn gpu_flag_lists_exact_indices() {
    let mut job = spec("r1");
    job.gpu_indices = vec![0, 2];
    let docker = limits_to_args(Engine::Docker, &job).unwrap();
    let at = docker.iter().position(|arg| arg == "--gpus").unwrap();
    assert_eq!(docker[at + 1], "\"device=0,2\"");
    assert!(!docker.iter().any(|arg| arg.contains("all")));
    assert!(!docker.iter().any(|arg| arg == "--device"));

    let podman = limits_to_args(Engine::Podman, &job).unwrap();
    let devices: Vec<&String> = podman
        .windows(2)
        .filter(|pair| pair[0] == "--device")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(devices, ["nvidia.com/gpu=0", "nvidia.com/gpu=2"]);
    assert!(
        !podman
            .iter()
            .any(|arg| arg == "--gpus" || arg.contains("all"))
    );

    job.gpu_indices = vec![1];
    let one = limits_to_args(Engine::Docker, &job).unwrap();
    assert!(one.contains(&"\"device=1\"".to_owned()));

    job.gpu_indices = Vec::new();
    for engine in [Engine::Docker, Engine::Podman] {
        let none = limits_to_args(engine, &job).unwrap();
        assert!(!none.iter().any(|a| a == "--gpus" || a == "--device"));
    }
}

/// Asserts that a read-only mount is read-only and a read-write one is not, that the
/// scratch directory is mounted read-write at `/scratch`, and that a path with `:` (the
/// separator `-v` would split on) is carried whole. It catches a read-only flag that is
/// dropped, which would let a job write to a directory it was only to read.
#[test]
fn mount_ro_honoured() {
    let mut job = spec("r1");
    job.mounts = vec![
        mount("/srv/data/in", "/mnt/data/in", true),
        mount("/srv/data/out", "/mnt/data/out", false),
        mount("/srv/a:b", "/mnt/data/a:b", true),
    ];
    let args = limits_to_args(Engine::Docker, &job).unwrap();
    let mounts: Vec<&String> = args
        .windows(2)
        .filter(|pair| pair[0] == "--mount")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(
        mounts,
        [
            "type=bind,src=/work/scratch/r1,dst=/scratch",
            "type=bind,src=/srv/data/in,dst=/mnt/data/in,readonly",
            "type=bind,src=/srv/data/out,dst=/mnt/data/out",
            "type=bind,src=/srv/a:b,dst=/mnt/data/a:b,readonly",
        ]
    );
    assert_eq!(SCRATCH_TARGET, "/scratch");
    assert!(!args.iter().any(|arg| arg == "-v" || arg == "--volume"));
}

/// Asserts that a mount path that could add an option to the comma-separated `--mount`
/// value is refused, and nothing is built: a comma, a double quote, a newline, a path
/// that is relative, one that is not UTF-8, in the source and in the target, and in the
/// scratch directory. The wire refuses such paths already; this is the second check. It
/// catches `src=/data,src=/etc` and `dst=/x,readonly=false` reaching the runtime.
#[test]
fn mount_injection_is_refused() {
    let bad_host = [
        "/srv/a,src=/etc",
        "/srv/a\",readonly=false",
        "/srv/a\nb",
        "srv/relative",
        "/srv/a,",
    ];
    for host in bad_host {
        let mut job = spec("r1");
        job.mounts = vec![mount(host, "/mnt/data/a", false)];
        let said = limits_to_args(Engine::Docker, &job);
        assert_eq!(said, Err(ArgsError::MountPath), "{host:?}");
    }
    for target in ["/mnt/a,dst=/etc", "/mnt/a\"", "/mnt/a\0", "mnt/relative"] {
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/a", target, true)];
        let said = limits_to_args(Engine::Podman, &job);
        assert_eq!(said, Err(ArgsError::MountPath), "{target:?}");
    }
    let mut job = spec("r1");
    job.mounts = vec![mount("/srv/a", "/mnt/a", false)];
    job.mounts[0].host = PathBuf::from(OsStr::from_bytes(b"/srv/\xff"));
    assert_eq!(
        limits_to_args(Engine::Docker, &job),
        Err(ArgsError::MountPath)
    );
    let mut job = spec("r1");
    job.scratch = PathBuf::from("/work/scratch,x=y/r1");
    assert_eq!(
        limits_to_args(Engine::Docker, &job),
        Err(ArgsError::MountPath)
    );
}

/// Asserts that labels are one `--label key=value` each in key order, that a value may
/// hold `=` and spaces, and that a key that is empty or holds `=` is refused (it would
/// move the split of the pair). It catches a label that changes another label's key.
#[test]
fn labels_are_pairs_and_keys_are_checked() {
    let labels: BTreeMap<String, String> = [("b", "x=y z"), ("a", "1")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    assert_eq!(
        label_args(&labels).unwrap(),
        strings(&["--label", "a=1", "--label", "b=x=y z"])
    );
    for key in ["", "a=b"] {
        let bad: BTreeMap<String, String> =
            [(key.to_owned(), "v".to_owned())].into_iter().collect();
        assert_eq!(label_args(&bad), Err(ArgsError::LabelKey), "{key:?}");
    }
    assert!(label_args(&BTreeMap::new()).unwrap().is_empty());
}
