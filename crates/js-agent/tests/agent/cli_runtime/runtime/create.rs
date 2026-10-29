//! The `create` call: the engine found by its version, the argv built for a job, the
//! refusals that reach the runtime not at all, the name of a container, and the failures
//! of a create or a start.

use js_agent::cli_runtime::{CliError, CliRuntime, Engine};
use js_agent::lifecycle::{ContainerRuntime, label};
use js_agent_wire::Isolation;

use super::super::scripted::Scripted;
use super::super::{
    HOST, NAME_R1, created, docker, in_another_pid_namespace, made_at, mount, open, open_at,
    open_with, platform, settings, spec,
};
use super::cleanup::other_agent;
use super::{start, strings, subs};

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
        "type=bind,src=/work/scratch/r1,dst=/scratch",
        "--mount",
        "type=bind,src=/srv/in,dst=/mnt/data/in,readonly,bind-recursive=readonly,bind-propagation=rprivate",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        TMPFS,
        "--ulimit",
        "core=0",
        "--ipc",
        "private",
        "--pull",
        "never",
        "--user",
        "65534:65534",
        "--",
        "registry.example/job:1",
        "--go",
        "x y",
    ]);
    assert_eq!(created(&world, "r1"), expected);
    assert_eq!(world.calls_of("create"), [expected]);
    assert_eq!(
        subs(&world),
        ["--version", "info", "create", "inspect", "start", "inspect"]
    );
}

/// Asserts that a Podman runtime is given the same job with CDI devices in place of
/// `--gpus`, its own `--pid private` (Docker takes no such value), no Docker-only mount
/// option, and nothing else different; and that no `info` call is made for it.
#[test]
fn podman_gets_cdi_devices_and_the_rest_alike() {
    let podman = Scripted::new("podman version 5.2.1");
    let mut rt = open_with(&podman, "linux", settings());
    let mut job = spec("r1");
    job.gpu_indices = vec![3];
    job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
    rt.create_and_start(&job).unwrap();
    let args = created(&podman, "r1");
    let at = args.iter().position(|a| a == "--device").unwrap();
    assert_eq!(args[at + 1], "nvidia.com/gpu=3");
    assert!(!args.contains(&"--gpus".to_owned()));
    assert!(args.contains(&"--read-only".to_owned()));
    let at = args.iter().position(|a| a == "--pid").unwrap();
    assert_eq!(args[at + 1], "private");
    assert!(
        args.contains(&"type=bind,src=/srv/in,dst=/mnt/data/in,readonly,nosuid,nodev".to_owned())
    );
    assert_eq!(
        subs(&podman),
        ["--version", "create", "inspect", "start", "inspect"]
    );
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
        let mut rt = open_with(&world, "linux", settings());
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
                job.args = hostile.iter().map(|a| (*a).to_owned()).collect();
                job.image = "-privileged".to_owned();
            }
            _ => {}
        }
        rt.create_and_start(&job).unwrap();
        kinds.push((job.run_id.clone(), job.image.clone(), job.args.clone()));
    }
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
            ("--pull", "never"),
            ("--user", "65534:65534"),
        ];
        for (flag, value) in pairs {
            let at = before.iter().position(|a| a == flag).unwrap();
            assert_eq!(before[at + 1], value, "{run}: {flag}");
        }
        assert!(before.contains(&"--read-only".to_owned()), "{run}");
        assert!(before.contains(&"--pids-limit".to_owned()), "{run}");
    }
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

fn mountinfo_beside_srv_in(path: &str) -> Option<String> {
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
