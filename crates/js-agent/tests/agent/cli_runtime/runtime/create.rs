//! The `create` call: the engine found by its version, the argv built for a job, the
//! refusals that reach the runtime not at all, the name of a container, and the failures
//! of a create or a start.

use std::fs;

use js_agent::cli_runtime::{CliError, CliRuntime, Engine};
use js_agent::lifecycle::{Body, Runtime, label};
use js_agent_wire::Isolation;
use sha2::{Digest, Sha256};

use super::super::scripted::{DOCKER, Scripted};
use super::super::{
    DOCKER_SCRATCH, HOST, NAME_R1, created, docker, in_another_pid_namespace, made_at, mount,
    no_file, open, open_at, open_with, platform, profile_option, sees_the_engines_pid, settings,
    spec,
};
use super::cleanup::other_agent;
use super::{start, strings, subs};
use crate::support::Scratch;

/// The `/tmp` of a container with the defaults.
const TMPFS: &str = "/tmp:rw,noexec,nosuid,nodev,size=64m";

/// Asserts that the program is told apart by what `--version` prints, whatever the file
/// is called: Docker, Podman (also when it emulates the Docker command line, which still
/// says Podman), and that anything else is refused, as is a program that cannot be run.
/// It catches Podman's GPU devices sent to Docker and a program that is neither being
/// driven with guessed arguments.
#[test]
fn the_engine_is_found_by_its_version() {
    let table = [
        ("Docker version 27.3.1, build ce12230", Some(Engine::Docker)),
        ("podman version 5.2.1", Some(Engine::Podman)),
        (
            "Emulate Docker CLI using podman.\npodman version 5",
            Some(Engine::Podman),
        ),
        ("nerdctl version 2.0.0", None),
        ("", None),
    ];
    for (text, engine) in table {
        assert_eq!(Engine::of_version(text), engine, "{text:?}");
        let world = Scripted::new(text);
        let opened = CliRuntime::open(
            settings(),
            HOST,
            &platform("linux"),
            made_at,
            Box::new(world),
        );
        match engine {
            Some(engine) => assert_eq!(opened.unwrap().engine(), engine),
            None => assert!(matches!(opened.err(), Some(CliError::NotAnEngine))),
        }
    }
    let world = docker();
    world.fail("--version", 1);
    let opened = CliRuntime::open(
        settings(),
        HOST,
        &platform("linux"),
        made_at,
        Box::new(world),
    );
    let Some(CliError::Call { call, why }) = opened.err() else {
        panic!("not a call failure");
    };
    assert_eq!(
        (call, why.as_str()),
        ("--version", "/bin/sh: exit status: 1")
    );
}

/// Asserts the whole `create` argv of a job with a GPU, mounts and labels: the name, the
/// order of the groups, that the agent's labels are there, that the hardening is spelled
/// out (a `/tmp` that cannot run programs, no core dumps, a private IPC namespace, no
/// pull), that a read-only bind is recursively read-only, and that the image and the
/// command line are the last entries after `--`. It is the shape the documentation
/// promises, and the order of calls: the mounts are read back before the start.
#[test]
fn create_builds_the_documented_argv() {
    let world = docker();
    let mut rt = open(&world);
    let mut job = spec("r1");
    job.gpu_indices = vec![0, 1];
    job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
    rt.create_and_start(&job).unwrap();
    let expected = strings(&[
        "create",
        "--name",
        NAME_R1,
        "--label",
        "jobscheduler.dev/agent=h-0123456789abcdef",
        "--label",
        "jobscheduler.dev/created-ms=1000000",
        "--label",
        "jobscheduler.dev/lease-id=lease-1",
        "--label",
        "jobscheduler.dev/run-id=r1",
        "--label",
        "team=ml",
        "--cpus",
        "1.500",
        "--memory",
        "1073741824",
        "--memory-swap",
        "1073741824",
        "--pids-limit",
        "100",
        "--gpus",
        "\"device=0,1\"",
        "--mount",
        DOCKER_SCRATCH,
        "--mount",
        "type=bind,src=/srv/in,dst=/mnt/data/in,readonly,bind-recursive=readonly,bind-propagation=rprivate",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--security-opt",
        &profile_option(),
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        TMPFS,
        "--ulimit",
        "core=0",
        "--ipc",
        "private",
        "--cgroupns",
        "private",
        "--log-driver",
        "json-file",
        "--log-opt",
        "max-size=1048576",
        "--log-opt",
        "max-file=2",
        "--pull",
        "never",
        "--user",
        "1000:1000",
        "--",
        "registry.example/job:1",
        "--go",
        "x y",
    ]);
    assert_eq!(created(&world, "r1"), expected);
    assert_eq!(world.calls_of("create"), [expected]);
    assert_eq!(
        subs(&world),
        [
            "--version",
            "info",
            "create",
            "inspect",
            "volume",
            "start",
            "inspect"
        ]
    );
}

/// Asserts that a Podman runtime is given the same job with CDI devices in place of
/// `--gpus`, its own `--pid private` (Docker takes no such value), no Docker-only mount
/// option, and nothing else different; and that no `info` call is made for it.
#[test]
fn podman_gets_cdi_devices_and_the_rest_alike() {
    let podman = Scripted::new("podman version 5.2.1");
    let mut rt = open_with(&podman, "linux", settings())
        .with_proc_files(mountinfo_beside_srv_in, in_another_pid_namespace);
    let mut job = spec("r1");
    job.gpu_indices = vec![3];
    job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
    rt.create_and_start(&job).unwrap();
    let args = created(&podman, "r1");
    let at = args.iter().position(|a| a == "--device").unwrap();
    assert_eq!(args[at + 1], "nvidia.com/gpu=3");
    assert!(!args.contains(&"--gpus".to_owned()));
    assert!(args.contains(&"--read-only".to_owned()));
    let tmpfs: Vec<&String> = args
        .windows(2)
        .filter(|pair| pair[0] == "--tmpfs")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(
        tmpfs,
        [
            TMPFS,
            "/var/tmp:rw,noexec,nosuid,nodev,size=64m",
            "/run:rw,noexec,nosuid,nodev,size=64m"
        ]
    );
    let at = args.iter().position(|a| a == "--pid").unwrap();
    assert_eq!(args[at + 1], "private");
    let at = args.iter().position(|a| a == "--log-driver").unwrap();
    assert_eq!(args[at + 1], "k8s-file");
    assert!(args.contains(&"max-size=1048576".to_owned()));
    assert!(
        !args.contains(&"max-file=2".to_owned()),
        "Podman takes no max-file"
    );
    assert!(
        args.contains(&"type=bind,src=/srv/in,dst=/mnt/data/in,readonly,nosuid,nodev".to_owned())
    );
    assert_eq!(
        subs(&podman),
        ["--version", "create", "inspect", "start", "inspect"]
    );
    assert!(podman.volumes().is_empty(), "Podman's scratch stays a bind");
}

/// Asserts that the recursive read-only option is passed only to a Docker that takes it
/// (version 25 and later, read from `--version`), and that an older Docker, a version the
/// agent cannot read, and Podman are given the plain `readonly`. It catches the option
/// sent to an engine that rejects it, which would refuse every job with a read-only mount.
/// (Podman also gets `nosuid,nodev` on every mount; the plain `readonly` is its read-only.)
#[test]
fn only_a_docker_that_takes_it_gets_recursive_readonly_binds() {
    let readonly = "type=bind,src=/srv/in,dst=/mnt/data/in,readonly";
    let recursive = format!("{readonly},bind-recursive=readonly,bind-propagation=rprivate");
    let podman_readonly = format!("{readonly},nosuid,nodev");
    for (version, expected) in [
        ("Docker version 25.0.0, build 1", recursive.as_str()),
        ("Docker version 100.1.2, build 1", recursive.as_str()),
        ("Docker version 24.0.7, build afdd53b", readonly),
        ("Docker version unknown", readonly),
        ("Docker 27", readonly),
        ("podman version 5.2.1", podman_readonly.as_str()),
    ] {
        let world = Scripted::new(version);
        let mut rt = open_with(&world, "linux", settings())
            .with_proc_files(mountinfo_beside_srv_in, in_another_pid_namespace);
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
        rt.create_and_start(&job).unwrap();
        let args = created(&world, "r1");
        assert!(args.contains(&expected.to_owned()), "{version}: {args:?}");
    }
}

/// Asserts that nothing a job names can become an option or a second command: its image
/// and arguments (`--privileged`, `-v /:/host`, `--network=host`, `--rm`, a shell line)
/// come only after `--`, in order and untouched, as separate entries; and the
/// hardening flags are present however the job's own fields look. It catches an
/// argument that reaches the runtime as one of its flags, and a hardening flag that
/// goes missing for some kind of job.
#[test]
fn the_job_cannot_reach_the_runtimes_options() {
    let hostile = [
        "--privileged",
        "-v",
        "/:/host",
        "--network=host",
        "--rm",
        "--",
        "; rm -rf /",
        "$(id)",
        "",
    ];
    let world = docker();
    let mut rt = open(&world);
    let mut kinds = Vec::new();
    for (n, kind) in ["plain", "gpu", "mounts", "no-pids", "hostile"]
        .iter()
        .enumerate()
    {
        let mut job = spec(&format!("r{n}"));
        match *kind {
            "gpu" => job.gpu_indices = vec![0],
            "mounts" => job.mounts = vec![mount("/srv/in", "/mnt/data/in", false)],
            "no-pids" => job.pids = None,
            "hostile" => {
                job.body = Body::Container {
                    image: "-privileged".to_owned(),
                    args: hostile.iter().map(|a| (*a).to_owned()).collect(),
                };
            }
            _ => {}
        }
        rt.create_and_start(&job).unwrap();
        let Body::Container { image, args } = job.body.clone();
        kinds.push((job.run_id.clone(), image, args));
    }
    let profile = profile_option();
    for (run, image, job_args) in kinds {
        let args = created(&world, &run);
        let dashes = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dashes + 1], image);
        assert_eq!(args[dashes + 2..], job_args[..], "{run}");
        let before = &args[..dashes];
        for flag in ["--privileged", "--rm", "--network=host", "-v"] {
            assert!(!before.contains(&flag.to_owned()), "{run}: {flag}");
        }
        let pairs = [
            ("--cap-drop", "ALL"),
            ("--security-opt", "no-new-privileges"),
            ("--network", "none"),
            ("--tmpfs", TMPFS),
            ("--ulimit", "core=0"),
            ("--ipc", "private"),
            ("--cgroupns", "private"),
            ("--pull", "never"),
            ("--user", "1000:1000"),
        ];
        for (flag, value) in pairs {
            let at = before.iter().position(|a| a == flag).unwrap();
            assert_eq!(before[at + 1], value, "{run}: {flag}");
        }
        assert_eq!(
            security_options(before),
            ["no-new-privileges", profile.as_str()],
            "{run}"
        );
        assert!(before.contains(&"--read-only".to_owned()), "{run}");
        assert!(before.contains(&"--pids-limit".to_owned()), "{run}");
    }
}

/// Asserts that a container runs as the user of its slot: `--user` is the slot's uid and
/// gid (1000 plus the slot by default), one flag, for the first slot, the next and a later
/// one. It catches runs in different slots that share a user, and a user that is not the
/// slot's.
#[test]
fn each_slot_runs_as_its_own_user() {
    let world = docker();
    let mut rt = open(&world);
    for (run, slot) in [("r1", 0), ("r2", 1), ("r3", 7)] {
        let mut job = spec(run);
        job.slot = slot;
        rt.create_and_start(&job).unwrap();
    }
    let users = |run: &str| -> Vec<String> {
        let args = created(&world, run);
        let before = &args[..args.iter().position(|arg| arg == "--").unwrap()];
        before
            .windows(2)
            .filter(|pair| pair[0] == "--user")
            .map(|pair| pair[1].clone())
            .collect()
    };
    assert_eq!(users("r1"), ["1000:1000"]);
    assert_eq!(users("r2"), ["1001:1001"]);
    assert_eq!(users("r3"), ["1007:1007"]);
}

/// Asserts that the agent's labels are the agent's: a job label that tries to name
/// another agent or another creation time is replaced, and the job's own labels stay.
/// It catches a job that makes another agent adopt its container.
#[test]
fn the_agents_own_labels_win() {
    let world = docker();
    let mut rt = open(&world);
    let mut job = spec("r1");
    job.labels
        .insert(label::AGENT.to_owned(), "h-other".to_owned());
    job.labels
        .insert(label::CREATED_MS.to_owned(), "1".to_owned());
    rt.create_and_start(&job).unwrap();
    let args = created(&world, "r1");
    let labels: Vec<&String> = args
        .windows(2)
        .filter(|pair| pair[0] == "--label")
        .map(|pair| &pair[1])
        .collect();
    assert!(labels.contains(&&format!("{}={HOST}", label::AGENT)));
    assert!(labels.contains(&&format!("{}=1000000", label::CREATED_MS)));
    assert!(labels.contains(&&"team=ml".to_owned()));
    assert_eq!(labels.len(), 5);
}

/// Asserts that the network, user, pull policy and `/tmp` size flags reach the
/// container: a named network instead of none, the configured uid and gid, `--pull
/// missing`, and the size in the tmpfs option. It catches a flag that is checked and then
/// ignored.
#[test]
fn a_configured_network_and_user_are_used() {
    let world = docker();
    let set = js_agent::cli_runtime::Settings::new(
        std::path::Path::new(super::super::BIN),
        Some("jobs"),
        Some("2000:3000"),
    )
    .unwrap()
    .with_pull(Some("missing"))
    .unwrap()
    .with_tmp_size(Some("1g"))
    .unwrap();
    let mut rt = open_with(&world, "linux", set);
    rt.create_and_start(&spec("r1")).unwrap();
    let args = created(&world, "r1");
    let value = |flag: &str| {
        let at = args.iter().position(|a| a == flag).unwrap();
        args[at + 1].clone()
    };
    assert_eq!(value("--network"), "jobs");
    assert_eq!(value("--user"), "2000:3000");
    assert_eq!(value("--pull"), "missing");
    assert_eq!(value("--tmpfs"), "/tmp:rw,noexec,nosuid,nodev,size=1g");
}

/// Asserts the refusals that create nothing and run nothing: a host job, a mount path
/// that is not safe, a label key that is not, and a run id that cannot be a container
/// name. It catches a host job run in a container, and a refused job that still reaches
/// the runtime.
#[test]
fn refused_jobs_reach_the_runtime_not_at_all() {
    let world = docker();
    let mut rt = open(&world);
    let before = world.calls().len();
    let mut host = spec("r1");
    host.isolation = Isolation::Host;
    let mut bad_mount = spec("r2");
    bad_mount.mounts = vec![mount("/srv/a,src=/etc", "/mnt/a", false)];
    let mut bad_label = spec("r3");
    bad_label.labels.insert("a=b".to_owned(), "v".to_owned());
    let bad_name = spec("r 4");
    let said =
        [host, bad_mount, bad_label, bad_name].map(|job| rt.create_and_start(&job).unwrap_err());
    assert_eq!(said[0].0, "only a contained job runs in a container");
    assert!(said[1].0.contains("mount path"), "{}", said[1].0);
    assert!(said[2].0.contains("label key"), "{}", said[2].0);
    assert_eq!(said[3].0, "the run id cannot be made a container name");
    assert_eq!(
        world.calls().len(),
        before,
        "only the opening calls were run"
    );
}

/// Asserts that a container is named after its agent and run, `js-<16 digits of the host
/// id's hash>-<run id>`, right after `create`: the name is what a failed create is cleaned
/// up by, and the agent part keeps two agents on one runtime from naming alike (another
/// host id gives another name for the same run). A run id with a character outside
/// `[A-Za-z0-9._-]`, an empty one, and one that would make the name longer than 128 bytes
/// (108 bytes of run id) are refused before any call, while the longest that fits is
/// taken. It catches a name the engine would refuse (a failure after the pull), a name
/// that is not derived from the run, which cannot be found again, and one that two
/// agents share.
#[test]
fn a_container_is_named_after_its_run() {
    let world = docker();
    let mut rt = open(&world);
    rt.create_and_start(&spec("r1")).unwrap();
    let args = created(&world, "r1");
    assert_eq!(args[..3], strings(&["create", "--name", NAME_R1])[..]);
    let tag = &NAME_R1[3..19];
    let other = other_agent(&world).create_and_start(&spec("r1")).unwrap();
    let other_name = world.create_of(&other.0)[2].clone();
    assert_ne!(other_name, NAME_R1);
    assert!(other_name.ends_with("-r1") && other_name.starts_with("js-"));
    assert!(!other_name.contains(tag));
    let longest = "a".repeat(108);
    for run in ["a.b_c-d", longest.as_str()] {
        rt.create_and_start(&spec(run)).unwrap();
        assert!(world.names().contains(&format!("js-{tag}-{run}")), "{run}");
    }
    let creates = world.calls_of("create").len();
    let too_long = "a".repeat(109);
    for run in ["", "a b", "a/b", "a:b", "a=b", "\u{e9}", too_long.as_str()] {
        let said = rt.create_and_start(&spec(run)).unwrap_err();
        assert_eq!(
            said.0, "the run id cannot be made a container name",
            "{run:?}"
        );
    }
    assert_eq!(world.calls_of("create").len(), creates);
}

/// Asserts that a GPU job on a Mac is refused whether the lifecycle asks first
/// (`supports_gpus`) or the runtime is called anyway, that nothing is created, and that
/// a job without a GPU runs there, as does a GPU job on Linux. It catches a GPU job
/// started on a runtime that cannot give it the GPU.
#[test]
fn gpu_limit_on_macos_refused() {
    let world = docker();
    let mac = open_at(&world, "macos", made_at);
    let linux = open_at(&world, "linux", made_at);
    assert!(!mac.supports_gpus());
    assert!(linux.supports_gpus());
    let mut mac = mac;
    let mut job = spec("r1");
    job.gpu_indices = vec![0];
    let said = mac.create_and_start(&job).unwrap_err();
    assert_eq!(said.0, "containers on this host cannot use a GPU");
    assert!(world.ids().is_empty());
    assert_eq!(world.calls_of("create").len(), 0);
    mac.create_and_start(&spec("r2")).unwrap();
    open(&world).create_and_start(&job).unwrap();
    assert_eq!(world.ids().len(), 2);
}

/// Asserts that `start` follows `create`, and that when `start` fails the container is
/// removed again: nothing is left behind, and the error says the start failed. When the
/// removal fails as well, the error says so too. It catches a container left created
/// and never started, which a restarted agent would adopt as a running job.
#[test]
fn a_failed_start_removes_the_container() {
    let world = docker();
    let mut rt = open(&world);
    world.fail("start", 1);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert_eq!(said.0, "start: /bin/sh: exit status: 1");
    assert!(world.ids().is_empty());
    assert_eq!(world.calls_of("rm").len(), 1);

    world.fail("start", 1);
    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r2")).unwrap_err();
    assert!(
        said.0
            .contains("; the container could not be removed: rm: "),
        "{}",
        said.0
    );
    assert_eq!(world.ids().len(), 1, "the leak is reported, not hidden");
}

/// Asserts that a `create` that fails (the nonzero exit is the error, its text is the
/// program's and its status) tries nothing else but the cleanup by name, and that a
/// `create` that prints something other than an id is an error before `start` is called,
/// for each kind of wrong output: empty, not hexadecimal, too short, too long, an id of
/// 12 digits (only the whole 64 are accepted, because every later comparison is against
/// the 64 the engine lists). It catches an id taken from output that is not one and then
/// put in later arguments.
#[test]
fn create_failures_are_typed_and_start_nothing() {
    let world = docker();
    let mut rt = open(&world);
    world.fail("create", 1);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert_eq!(said.0, "create: /bin/sh: exit status: 1");
    assert_eq!(world.calls_of("start").len(), 0);
    let long = "a".repeat(65);
    for printed in [
        "",
        "\n",
        "not-an-id\n",
        "abc123\n",
        "0123456789ab\n",
        long.as_str(),
        "ABCDEF0123456789",
    ] {
        world.say("create", printed);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(said.0, "create: unreadable output", "{printed:?}");
    }
    assert_eq!(world.calls_of("start").len(), 0);
    assert!(world.ids().is_empty());
}

/// Asserts that the id of a container is the last line `create` printed, so the lines a
/// pull prints before it do not matter.
#[test]
fn the_id_is_the_last_line_of_create() {
    let world = docker();
    let mut rt = open(&world);
    world.preface("create", "Unable to find image locally\nPulling fs layer\n");
    let id = start(&mut rt, &world, "r1");
    assert_eq!(id.0.len(), 64);
}

/// A `/proc/self/mountinfo` with the mount points below, one line each, and no other file.
fn mountinfo_with(points: &[&str]) -> Option<String> {
    Some(
        points
            .iter()
            .map(|point| format!("36 25 0:30 / {point} rw,relatime shared:1 - ext4 /dev/sda1 rw\n"))
            .collect(),
    )
}

fn mountinfo_below_srv_in(path: &str) -> Option<String> {
    match path {
        "/proc/self/mountinfo" => mountinfo_with(&["/", "/srv/in/sub", "/srv/input"]),
        _ => None,
    }
}

pub(super) fn mountinfo_beside_srv_in(path: &str) -> Option<String> {
    match path {
        "/proc/self/mountinfo" => mountinfo_with(&["/", "/srv/in", "/srv/input", "/srv"]),
        _ => None,
    }
}

fn mountinfo_below_a_name_with_a_space(path: &str) -> Option<String> {
    match path {
        "/proc/self/mountinfo" => mountinfo_with(&["/", "/srv/my\\040data/sub"]),
        _ => None,
    }
}

/// Asserts that a read-only mount that holds another mount point is refused, before any
/// `create`, on an engine without recursive read-only (Podman, Docker before 25), because
/// the mount below it would stay writable; that Docker 25 and later, which makes the
/// mount read-only all the way down, takes it; that a read-write mount, a mount whose
/// neighbours merely share its name as a prefix, and the mount point itself are not
/// reasons to refuse; and that a space in the path (escaped in `mountinfo`) is read. It
/// catches a read-only operator mount with a writable submount, a prefix match on text
/// instead of on path components, and a refusal that fires for the mount's own entry.
#[test]
fn a_readonly_mount_with_a_submount_is_refused_where_it_cannot_be_recursive() {
    let try_create = |version: &str,
                      host: &str,
                      ro: bool,
                      read: fn(&str) -> Option<String>|
     -> (Result<(), String>, usize) {
        let world = Scripted::new(version);
        let mut rt =
            open_with(&world, "linux", settings()).with_proc_files(read, in_another_pid_namespace);
        let mut job = spec("r1");
        job.mounts = vec![mount(host, "/mnt/data/in", ro)];
        let made = rt
            .create_and_start(&job)
            .map(|_| ())
            .map_err(|why| why.to_string());
        (made, world.calls_of("create").len())
    };
    for version in ["podman version 5.2.1", "Docker version 24.0.7, build 1"] {
        let (made, creates) = try_create(version, "/srv/in", true, mountinfo_below_srv_in);
        let why = made.unwrap_err();
        assert!(why.contains("holds another mount"), "{version}: {why}");
        assert_eq!(creates, 0, "{version}");
        let (made, _) = try_create(version, "/srv/in", false, mountinfo_below_srv_in);
        assert!(made.is_ok(), "{version}: read-write is not refused");
        let (made, _) = try_create(version, "/srv/in", true, mountinfo_beside_srv_in);
        assert!(made.is_ok(), "{version}: no mount below it");
        let (made, _) = try_create(
            version,
            "/srv/my data",
            true,
            mountinfo_below_a_name_with_a_space,
        );
        assert!(
            made.unwrap_err().contains("holds another mount"),
            "{version}"
        );
    }
    let (made, creates) = try_create(
        "Docker version 27.3.1, build 1",
        "/srv/in",
        true,
        mountinfo_below_srv_in,
    );
    assert!(made.is_ok() && creates == 1);
}

fn mountinfo_below_escaped_names(path: &str) -> Option<String> {
    match path {
        "/proc/self/mountinfo" => mountinfo_with(&[
            "/",
            "/srv/in\\0431/sub",
            "/srv/a\\011b/sub",
            "/srv/odd/\\777",
        ]),
        _ => None,
    }
}

fn mountinfo_with_a_stray_backslash(path: &str) -> Option<String> {
    match path {
        "/proc/self/mountinfo" => mountinfo_with(&["/", "/srv/odd/x\\"]),
        _ => None,
    }
}

/// Asserts that a mount point is found whatever bytes its path has: mountinfo writes a
/// space, tab, newline, `#` and backslash as a three-digit octal escape (`\011` for a
/// tab, `\043` for `#`), and each is read back before the path is compared; an escape
/// that is not one byte (`\777`) or a lone backslash is kept as written. It catches a
/// submount under a path with a tab or a `#` that is not seen because its name is still
/// escaped, which would leave it writable below a read-only mount.
#[test]
fn a_submount_is_found_under_any_escaped_name() {
    let refused = |host: &str, read: fn(&str) -> Option<String>| {
        let world = Scripted::new("podman version 5.2.1");
        let mut rt =
            open_with(&world, "linux", settings()).with_proc_files(read, in_another_pid_namespace);
        let mut job = spec("r1");
        job.mounts = vec![mount(host, "/mnt/data/in", true)];
        let made = rt.create_and_start(&job);
        (
            made.err().map(|why| why.to_string()),
            world.calls_of("create").len(),
        )
    };
    for host in ["/srv/in#1", "/srv/a\tb", "/srv/odd"] {
        let (why, creates) = refused(host, mountinfo_below_escaped_names);
        assert!(why.unwrap().contains("holds another mount"), "{host}");
        assert_eq!(creates, 0, "{host}");
    }
    let (why, _) = refused("/srv/odd", mountinfo_with_a_stray_backslash);
    assert!(why.unwrap().contains("holds another mount"));
}

/// Asserts that where the check applies (Podman, Docker before 25, a Linux host) a
/// read-only mount is refused, with no `create` call, when `/proc/self/mountinfo` cannot
/// be read, and the error says so; that a job without a read-only mount does not need
/// the file; and that a macOS host, whose containers run in a VM the agent's mounts say
/// nothing about, is not asked for it. It catches a check that passes when it could not
/// look.
#[test]
fn an_unreadable_mountinfo_refuses_a_readonly_mount() {
    for version in ["podman version 5.2.1", "Docker version 24.0.7, build 1"] {
        let world = Scripted::new(version);
        let mut rt = open_with(&world, "linux", settings());
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
        let why = rt.create_and_start(&job).unwrap_err().to_string();
        assert!(why.contains("mountinfo"), "{version}: {why}");
        assert!(world.calls_of("create").is_empty(), "{version}");
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", false)];
        rt.create_and_start(&job).unwrap();
        let world = Scripted::new(version);
        let mut rt = open_with(&world, "macos", settings());
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
        rt.create_and_start(&job).unwrap();
    }
}

/// The sha256 of the pinned profile, spelled out here and not computed from the source
/// file, so that an edit of the profile fails this test until the pin is changed on
/// purpose.
const PROFILE_SHA256: &str = "e2bbdceda151ab5482a7ffeca6a8650ab5d111c8ed65ba7fb8f5ca2b95bcef67";

/// The file the agent keeps the profile in, beside the run directories.
const PROFILE_FILE: &str = "seccomp+pinned.json";

fn sha256_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A runtime over `world` that keeps its profile where it is told to in production: in
/// the directory above the run's scratch directory.
fn bare(world: &Scripted) -> CliRuntime {
    CliRuntime::open(
        settings(),
        HOST,
        &platform("linux"),
        made_at,
        Box::new(world.clone()),
    )
    .unwrap()
    .with_pid_seen(sees_the_engines_pid)
    .with_proc_files(no_file, in_another_pid_namespace)
}

/// The `--security-opt` values of a create argv.
fn security_options(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == "--security-opt")
        .map(|pair| pair[1].clone())
        .collect()
}

/// Asserts, for Docker and for Podman, that a create names a profile file that exists
/// when the engine is called, that the file is the pinned profile byte for byte (by
/// its sha256, and a default-deny `defaultAction`), that it sits in the scratch root
/// (the directory above the run's own, which only the agent may enter), and that
/// `no-new-privileges` is still there. It catches a create with no profile of its own,
/// a profile named but never written, a copy that differs from the pin, and a file
/// that a job could read or replace because it is inside the run's directory.
#[test]
fn create_passes_the_pinned_profile_file() {
    for (n, version) in [DOCKER, "podman version 5.2.1\n"].into_iter().enumerate() {
        let root = Scratch::new("seccomp-create");
        let world = Scripted::new(version);
        let mut rt = bare(&world);
        let run = format!("r{n}");
        let mut job = spec(&run);
        job.scratch = root.path(&run);
        rt.create_and_start(&job).unwrap();
        let options = security_options(&created(&world, &run));
        let profile = root.path(PROFILE_FILE);
        assert_eq!(
            options,
            [
                "no-new-privileges".to_owned(),
                format!("seccomp={}", profile.display())
            ]
        );
        let bytes = fs::read(&profile).unwrap();
        assert_eq!(sha256_of(&bytes), PROFILE_SHA256);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["defaultAction"], "SCMP_ACT_ERRNO");
    }
}

/// Asserts that a profile file that was replaced by something else is made right again
/// before the next create, and one that is right is left as it is. It catches an agent
/// that trusts a file because it exists.
#[test]
fn a_changed_profile_file_is_written_again() {
    let root = Scratch::new("seccomp-rewrite");
    let world = docker();
    let mut rt = bare(&world);
    let profile = root.path(PROFILE_FILE);
    fs::write(&profile, "{\"defaultAction\":\"SCMP_ACT_ALLOW\"}").unwrap();
    for run in ["a1", "a2"] {
        let mut job = spec(run);
        job.scratch = root.path(run);
        rt.create_and_start(&job).unwrap();
        assert_eq!(
            sha256_of(&fs::read(&profile).unwrap()),
            PROFILE_SHA256,
            "{run}"
        );
    }
}

/// Asserts that a create whose profile cannot be written fails before the engine is
/// asked to create anything, whether the scratch directory has no parent or the parent
/// is not a directory that exists, and that the error names the profile. It catches a
/// job started without its syscall filter because the file was missing.
#[test]
fn no_profile_no_container() {
    let world = docker();
    let mut rt = bare(&world);
    for scratch in ["/", "/no/such/root/r1"] {
        let mut job = spec("r1");
        job.scratch = scratch.into();
        let said = rt.create_and_start(&job).err().unwrap().to_string();
        assert!(
            said.starts_with("scratch: seccomp profile"),
            "{scratch}: {said}"
        );
    }
    assert!(world.calls_of("create").is_empty());
}

/// Asserts that a directory whose name is not UTF-8 is refused before anything is
/// written: the profile's path is an argv entry and must be text. It catches a path
/// that is mangled lossily into one that names another file.
#[test]
fn a_profile_directory_that_is_not_text_is_refused() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = std::path::Path::new(OsStr::from_bytes(b"/work/\xff"));
    let said = js_agent::cli_runtime::install_profile(dir).unwrap_err();
    assert_eq!(said.kind(), std::io::ErrorKind::InvalidInput);
}

/// Asserts that a profile directory whose path holds a comma is refused, by
/// `install_profile` itself (nothing is written there) and by a create (the engine is
/// never called): Docker splits a `--security-opt` value on commas, so the path would be
/// read as a profile and an option of the job's host. It catches a profile named by a
/// path that changes the meaning of the option.
#[test]
fn a_profile_directory_with_a_comma_is_refused() {
    let root = Scratch::new("seccomp-comma");
    let dir = root.path("a,b");
    fs::create_dir(&dir).unwrap();
    let said = js_agent::cli_runtime::install_profile(&dir).unwrap_err();
    assert_eq!(said.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!dir.join(PROFILE_FILE).exists());

    let world = docker();
    let mut rt = open(&world).with_profile_dir(&dir);
    let said = rt.create_and_start(&spec("r1")).err().unwrap().to_string();
    assert!(said.starts_with("scratch: seccomp profile"), "{said}");
    assert!(world.calls_of("create").is_empty());
}
