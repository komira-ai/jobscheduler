//! The arguments of `create`: the pure functions that turn a job into flags.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use js_agent::cli_runtime::{
    ArgsError, DEFAULT_PIDS, Engine, SCRATCH_TARGET, Settings, hardening_args, label_args,
    limits_to_args,
};

use super::{BIN, DOCKER_SCRATCH, mount, settings, spec};

/// The profile option's file in these tests.
const PROFILE: &str = "/scratch-root/seccomp+pinned.json";

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
        DOCKER_SCRATCH,
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
            "smallest cpu",
            row(1, 1 << 30, Some(100)),
            base("0.001", "1073741824", "100"),
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
        assert_eq!(
            limits_to_args(Engine::Docker, &job, true).unwrap(),
            expected,
            "{name}"
        );
        let mut podman = expected.clone();
        *podman.last_mut().unwrap() =
            "type=bind,src=/work/scratch/r1,dst=/scratch,nosuid,nodev".to_owned();
        assert_eq!(
            limits_to_args(Engine::Podman, &job, true).unwrap(),
            podman,
            "{name}"
        );
    }
}

/// Asserts that a limit of 0 is refused here and not only by the wire's range check:
/// the engines read 0 for CPUs, memory and processes as "no limit", so a 0 that reached
/// them would give the job the whole machine. It catches a limit that is dropped by the
/// engine and not enforced, whichever way the job reached the runtime.
#[test]
fn a_limit_of_zero_is_refused() {
    for (name, cpu_millis, memory_bytes, pids) in [
        ("cpu", 0, 1 << 30, Some(100)),
        ("memory", 1000, 0, Some(100)),
        ("pids", 1000, 1 << 30, Some(0)),
    ] {
        let mut job = spec("r1");
        (job.cpu_millis, job.memory_bytes, job.pids) = (cpu_millis, memory_bytes, pids);
        for engine in [Engine::Docker, Engine::Podman] {
            let said = limits_to_args(engine, &job, true);
            assert_eq!(said, Err(ArgsError::ZeroLimit), "{name}");
        }
    }
    assert!(
        ArgsError::ZeroLimit
            .to_string()
            .contains("cpu, memory and pids")
    );
}

/// Asserts that every contained job gets the hardening whatever it asked for: all
/// capabilities dropped, no new privileges, no network by default, a read-only root
/// with a `/tmp` that is a tmpfs which cannot run programs or hold devices or set-uid
/// files and is limited in size, no core dumps, a private IPC namespace (and for Podman,
/// the only one of the two that takes the value, a private PID namespace), no pull, a
/// user that is not root, and no `--rm` (the lifecycle removes the container after it
/// has read the result). It catches a flag dropped from the list, a default network that
/// reaches out, a `/tmp` that is not `noexec` on Podman, and a container that removes
/// itself before its result is read.
#[test]
fn hardening_flags_always_present() {
    let docker = hardening_args(&settings(), Engine::Docker, PROFILE);
    let common = [
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--security-opt",
        "seccomp=/scratch-root/seccomp+pinned.json",
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,nodev,size=64m",
        "--ulimit",
        "core=0",
        "--ipc",
        "private",
    ];
    let tail = ["--pull", "never", "--user", "65534:65534"];
    assert_eq!(docker, strings(&[&common[..], &tail[..]].concat()));
    assert!(!docker.iter().any(|arg| arg == "--rm" || arg == "--pid"));

    let podman = hardening_args(&settings(), Engine::Podman, PROFILE);
    let at = common.iter().position(|arg| *arg == "--ulimit").unwrap();
    let (before, after) = common.split_at(at);
    let tmpfs = [
        "--tmpfs",
        "/var/tmp:rw,noexec,nosuid,nodev,size=64m",
        "--tmpfs",
        "/run:rw,noexec,nosuid,nodev,size=64m",
    ];
    assert_eq!(
        podman,
        strings(
            &[
                before,
                &tmpfs[..],
                after,
                &["--pid", "private"][..],
                &tail[..]
            ]
            .concat()
        )
    );
}

/// Asserts that on Podman, whose `--read-only` also mounts writable tmpfs mounts at
/// `/var/tmp` and at the image's run path (4.9.3 sets `--read-only-tmpfs` on by default,
/// and those mounts have neither `noexec` nor a size), the agent names both itself, with
/// the options and the size of `/tmp` (so the size setting reaches them), and that
/// Docker, whose `--read-only` adds no such mounts, is given only `/tmp`. It catches a
/// writable `/var/tmp` or `/run` where a job can run a program of its own making, or fill
/// the memory, and a Docker container given writable directories it did not have.
#[test]
fn podman_hardens_var_tmp_and_run_like_tmp() {
    let set = Settings::new(Path::new(BIN), None, None)
        .unwrap()
        .with_tmp_size(Some("2g"))
        .unwrap();
    let tmpfs_of = |engine| -> Vec<String> {
        let args = hardening_args(&set, engine, PROFILE);
        args.windows(2)
            .filter(|pair| pair[0] == "--tmpfs")
            .map(|pair| pair[1].clone())
            .collect()
    };
    assert_eq!(
        tmpfs_of(Engine::Podman),
        [
            "/tmp:rw,noexec,nosuid,nodev,size=2g",
            "/var/tmp:rw,noexec,nosuid,nodev,size=2g",
            "/run:rw,noexec,nosuid,nodev,size=2g",
        ]
    );
    assert_eq!(
        tmpfs_of(Engine::Docker),
        ["/tmp:rw,noexec,nosuid,nodev,size=2g"]
    );
    let podman = hardening_args(&set, Engine::Podman, PROFILE);
    assert!(
        !podman
            .iter()
            .any(|arg| arg.starts_with("--read-only-tmpfs"))
    );
}

/// Asserts, for both engines, the properties of the hardening list that keep a job
/// from gaining privilege: every capability dropped, `no-new-privileges`, and exactly one
/// seccomp profile that names a file (never `unconfined`, never the engine's built-in
/// default, which changes with the engine's version). It catches a job that runs with the
/// engine's own syscall filter, and a list that loses either of the older flags when the
/// profile is added.
#[test]
fn hardening_pins_privileges_and_the_syscall_filter() {
    for engine in [Engine::Docker, Engine::Podman] {
        let args = hardening_args(&settings(), engine, PROFILE);
        let values = |flag: &str| -> Vec<String> {
            args.windows(2)
                .filter(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .collect()
        };
        assert_eq!(values("--cap-drop"), ["ALL"], "{engine:?}");
        let options = values("--security-opt");
        assert!(
            options.contains(&"no-new-privileges".to_owned()),
            "{engine:?}"
        );
        let profiles: Vec<&String> = options
            .iter()
            .filter(|option| option.starts_with("seccomp="))
            .collect();
        assert_eq!(profiles.len(), 1, "{engine:?}");
        assert_eq!(profiles[0], &format!("seccomp={PROFILE}"), "{engine:?}");
    }
}

/// Asserts that the settings reach the hardening list: the network, the user, the pull
/// policy and the size of `/tmp`. It catches a flag that is checked and not used.
#[test]
fn hardening_follows_the_settings() {
    let set = Settings::new(Path::new(BIN), Some("jobs"), Some("5:6"))
        .unwrap()
        .with_pull(Some("missing"))
        .unwrap()
        .with_tmp_size(Some("2g"))
        .unwrap();
    let args = hardening_args(&set, Engine::Docker, PROFILE);
    let value = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(value("--network"), "jobs");
    assert_eq!(value("--user"), "5:6");
    assert_eq!(value("--pull"), "missing");
    assert_eq!(value("--tmpfs"), "/tmp:rw,noexec,nosuid,nodev,size=2g");
}

/// Asserts the golden tail of the hardening list with `--actions-cgroup`: `--cgroup-parent`
/// comes last with the slice name for a slice and the path with a leading `/` otherwise, on
/// both engines, and the list without the flag has none. It catches a configured control
/// group that is checked and never reaches `create`, which leaves the containers outside
/// the cgroup the node's usage is read from.
#[test]
fn the_actions_cgroup_becomes_the_cgroup_parent() {
    let with = |dir: &str| {
        Settings::new(Path::new(BIN), None, None)
            .unwrap()
            .with_actions_cgroup(Some(Path::new(dir)))
            .unwrap()
    };
    let rows = [
        ("/sys/fs/cgroup/jobscheduler.slice", "jobscheduler.slice"),
        ("/sys/fs/cgroup/js/actions", "/js/actions"),
    ];
    for (dir, parent) in rows {
        for engine in [Engine::Docker, Engine::Podman] {
            let args = hardening_args(&with(dir), engine, PROFILE);
            assert_eq!(
                args[args.len() - 2..],
                strings(&["--cgroup-parent", parent])
            );
        }
    }
    let plain = hardening_args(&settings(), Engine::Docker, PROFILE);
    assert!(!plain.iter().any(|arg| arg == "--cgroup-parent"));
}

/// Asserts that the Docker flag names the exact indices and nothing else, that Podman
/// is given one CDI device for each index, that no job without a GPU gets a GPU flag,
/// and that no spelling of "all" is ever produced. It catches a job given every GPU of
/// the host, and an index list in the wrong shape for the runtime that reads it.
#[test]
fn gpu_flag_lists_exact_indices() {
    let mut job = spec("r1");
    job.gpu_indices = vec![0, 2];
    let docker = limits_to_args(Engine::Docker, &job, true).unwrap();
    let at = docker.iter().position(|arg| arg == "--gpus").unwrap();
    assert_eq!(docker[at + 1], "\"device=0,2\"");
    assert!(!docker.iter().any(|arg| arg.contains("all")));
    assert!(!docker.iter().any(|arg| arg == "--device"));

    let podman = limits_to_args(Engine::Podman, &job, false).unwrap();
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
    let one = limits_to_args(Engine::Docker, &job, true).unwrap();
    assert!(one.contains(&"\"device=1\"".to_owned()));

    job.gpu_indices = Vec::new();
    for engine in [Engine::Docker, Engine::Podman] {
        let none = limits_to_args(engine, &job, true).unwrap();
        assert!(!none.iter().any(|a| a == "--gpus" || a == "--device"));
    }
}

/// Asserts the exact argument pair Docker gets for one GPU and for two: `--gpus` and then
/// one argument that holds the quotes, `"device=1"` and `"device=0,1"`. The engine reads
/// the value as a CSV field, so the quotes are part of the argument (the agent starts the
/// engine without a shell that would strip them) and are what keeps the comma between two
/// indices from splitting the value. It catches a value passed bare, or split into two
/// arguments, which the engine would read as another option or as every GPU. Not verified
/// against a real engine yet.
#[test]
fn docker_gets_the_gpu_indices_as_one_quoted_argument() {
    let mut job = spec("r1");
    for (indices, value) in [(vec![1], "\"device=1\""), (vec![0, 1], "\"device=0,1\"")] {
        job.gpu_indices = indices;
        let args = limits_to_args(Engine::Docker, &job, true).unwrap();
        let pairs: Vec<&[String]> = args.windows(2).filter(|p| p[0] == "--gpus").collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ["--gpus".to_owned(), value.to_owned()]);
        // The quoted GPU value appears once; the scratch volume's `volume-opt=device=` is
        // another option and is not counted.
        assert_eq!(
            args.iter().filter(|a| a.starts_with("\"device=")).count(),
            1
        );
    }
}

fn mounts_of(args: &[String]) -> Vec<&String> {
    args.windows(2)
        .filter(|pair| pair[0] == "--mount")
        .map(|pair| &pair[1])
        .collect()
}

/// Asserts that a read-only mount is read-only and a read-write one is not, that the
/// scratch directory is mounted read-write at `/scratch`, that no `-v` is used, and that
/// a read-only bind is recursively read-only (and its propagation private) when the
/// engine takes the option, so that a mount inside it stays read-only too; for an engine
/// that does not, the plain `readonly` is all that is passed. It catches a read-only flag
/// that is dropped, which would let a job write to a directory it was only to read, and
/// the option sent to an engine that rejects it.
#[test]
fn mount_ro_honoured() {
    let mut job = spec("r1");
    job.mounts = vec![
        mount("/srv/data/in", "/mnt/data/in", true),
        mount("/srv/data/out", "/mnt/data/out", false),
    ];
    let args = limits_to_args(Engine::Docker, &job, true).unwrap();
    assert_eq!(
        mounts_of(&args),
        [
            DOCKER_SCRATCH,
            "type=bind,src=/srv/data/in,dst=/mnt/data/in,readonly,bind-recursive=readonly,bind-propagation=rprivate",
            "type=bind,src=/srv/data/out,dst=/mnt/data/out",
        ]
    );
    assert_eq!(SCRATCH_TARGET, "/scratch");
    assert!(!args.iter().any(|arg| arg == "-v" || arg == "--volume"));

    let old = limits_to_args(Engine::Docker, &job, false).unwrap();
    assert_eq!(
        mounts_of(&old),
        [
            DOCKER_SCRATCH,
            "type=bind,src=/srv/data/in,dst=/mnt/data/in,readonly",
            "type=bind,src=/srv/data/out,dst=/mnt/data/out",
        ]
    );
}

/// Asserts that on Podman every mount, the scratch directory first, is `nosuid,nodev`
/// (after `readonly` where it is read-only), so a set-uid file or a device node a job
/// leaves there has no effect; and that on Docker, whose `--mount type=bind` takes no such
/// option, the scratch directory is a volume of the local driver that binds the directory
/// `nosuid,nodev` (the option list in one quoted CSV field, no copy of the image's
/// `/scratch` into it) while the other mounts stay plain binds. It catches a scratch
/// directory that can hold a working set-uid binary or device node, and the option sent
/// to a bind mount that rejects it.
#[test]
fn podman_mounts_are_nosuid_nodev() {
    let mut job = spec("r1");
    job.mounts = vec![
        mount("/srv/data/in", "/mnt/data/in", true),
        mount("/srv/data/out", "/mnt/data/out", false),
    ];
    let args = limits_to_args(Engine::Podman, &job, false).unwrap();
    assert_eq!(
        mounts_of(&args),
        [
            "type=bind,src=/work/scratch/r1,dst=/scratch,nosuid,nodev",
            "type=bind,src=/srv/data/in,dst=/mnt/data/in,readonly,nosuid,nodev",
            "type=bind,src=/srv/data/out,dst=/mnt/data/out,nosuid,nodev",
        ]
    );
    let docker = limits_to_args(Engine::Docker, &job, false).unwrap();
    let docker = mounts_of(&docker);
    assert_eq!(docker[0], DOCKER_SCRATCH);
    assert!(!docker[1..].iter().any(|m| m.contains("nosuid")));
}

/// Asserts that a mount path that could add an option to the comma-separated `--mount`
/// value is refused, and nothing is built: a comma, a double quote, a colon (the
/// separator of the short form, which the mount roots refuse too, so the claim that it is
/// refused twice holds), a newline, a path that is relative, one that is not UTF-8, in
/// the source and in the target, and in the scratch directory. The wire refuses such
/// paths already; this is the second check. It catches `src=/data,src=/etc` and
/// `dst=/x,readonly=false` reaching the runtime.
#[test]
fn mount_injection_is_refused() {
    let bad_host = [
        "/srv/a,src=/etc",
        "/srv/a\",readonly=false",
        "/srv/a\nb",
        "srv/relative",
        "/srv/a,",
        "/srv/a:b",
    ];
    for host in bad_host {
        let mut job = spec("r1");
        job.mounts = vec![mount(host, "/mnt/data/a", false)];
        let said = limits_to_args(Engine::Docker, &job, true);
        assert_eq!(said, Err(ArgsError::MountPath), "{host:?}");
    }
    for target in [
        "/mnt/a,dst=/etc",
        "/mnt/a\"",
        "/mnt/a\0",
        "mnt/relative",
        "/mnt/a:b",
    ] {
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/a", target, true)];
        let said = limits_to_args(Engine::Podman, &job, true);
        assert_eq!(said, Err(ArgsError::MountPath), "{target:?}");
    }
    let mut job = spec("r1");
    job.mounts = vec![mount("/srv/a", "/mnt/a", false)];
    job.mounts[0].host = PathBuf::from(OsStr::from_bytes(b"/srv/\xff"));
    assert_eq!(
        limits_to_args(Engine::Docker, &job, true),
        Err(ArgsError::MountPath)
    );
    let mut job = spec("r1");
    job.scratch = PathBuf::from("/work/scratch,x=y/r1");
    assert_eq!(
        limits_to_args(Engine::Docker, &job, true),
        Err(ArgsError::MountPath)
    );
    job.scratch = PathBuf::from("/work/scratch:x/r1");
    assert_eq!(
        limits_to_args(Engine::Docker, &job, true),
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
