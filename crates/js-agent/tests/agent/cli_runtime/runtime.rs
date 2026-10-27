//! `CliRuntime` over a runner that answers like the command line: every call, every
//! answer, and every failure.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::cli_runtime::{CliError, CliRuntime, Engine, hand_over, unix_ms};
use js_agent::lifecycle::{
    ContainerId, ContainerRuntime, Exit, Found, RuntimeError, Status, label,
};
use js_agent_wire::Isolation;

use super::scripted::{Bind, Change, Env, Scripted};
use super::{
    BIN, HOST, created, docker, made_at, mount, open, open_at, open_with, platform, settings, spec,
    thirty_seconds_on,
};
use crate::support::Scratch;

/// The `PATH` every call of the engine is given.
const PINNED_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The `/tmp` of a container with the defaults.
const TMPFS: &str = "/tmp:rw,noexec,nosuid,nodev,size=64m";

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Starts `run_id` in `world` and returns its container id.
fn start(rt: &mut CliRuntime, world: &Scripted, run_id: &str) -> ContainerId {
    let id = rt.create_and_start(&spec(run_id)).unwrap();
    assert_eq!(Some(id.0.clone()), world.id_of(run_id));
    id
}

/// The first subcommand of every call, in order.
fn subs(world: &Scripted) -> Vec<String> {
    world
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect()
}

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
        "js-r1",
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
        ["--version", "info", "create", "inspect", "start"]
    );
}

/// Asserts that a Podman runtime is given the same job with CDI devices in place of
/// `--gpus`, its own `--pid private` (Docker takes no such value), no Docker-only mount
/// option, and nothing else different; and that no `info` call is made for it.
#[test]
fn podman_gets_cdi_devices_and_the_rest_alike() {
    let podman = Scripted::new("podman version 5.2.1");
    let mut rt = CliRuntime::open(
        settings(),
        HOST,
        &platform("linux"),
        made_at,
        Box::new(podman.clone()),
    )
    .ok()
    .unwrap();
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
    assert!(args.contains(&"type=bind,src=/srv/in,dst=/mnt/data/in,readonly".to_owned()));
    assert_eq!(subs(&podman), ["--version", "create", "inspect", "start"]);
}

/// Asserts that the recursive read-only option is passed only to a Docker that takes it
/// (version 25 and later, read from `--version`), and that an older Docker, a version the
/// agent cannot read, and Podman are given the plain `readonly`. It catches the option
/// sent to an engine that rejects it, which would refuse every job with a read-only mount.
#[test]
fn only_a_docker_that_takes_it_gets_recursive_readonly_binds() {
    let readonly = "type=bind,src=/srv/in,dst=/mnt/data/in,readonly";
    let recursive = format!("{readonly},bind-recursive=readonly,bind-propagation=rprivate");
    for (version, expected) in [
        ("Docker version 25.0.0, build 1", recursive.as_str()),
        ("Docker version 100.1.2, build 1", recursive.as_str()),
        ("Docker version 24.0.7, build afdd53b", readonly),
        ("Docker version unknown", readonly),
        ("Docker 27", readonly),
        ("podman version 5.2.1", readonly),
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
        std::path::Path::new(super::BIN),
        Some("jobs"),
        Some("2000:3000"),
    )
    .unwrap()
    .with_pull(Some("missing"))
    .unwrap()
    .with_tmp_size(Some("1g"))
    .unwrap();
    let mut rt = CliRuntime::open(
        set,
        HOST,
        &platform("linux"),
        made_at,
        Box::new(world.clone()),
    )
    .ok()
    .unwrap();
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

/// Asserts that a container is named after its run, `js-<run id>`, right after `create`:
/// the name is what a failed create is cleaned up by. A run id with a character outside
/// `[A-Za-z0-9._-]`, an empty one, and one that would make the name longer than 128 bytes
/// are refused before any call, while the longest that fits is taken. It catches a name
/// the engine would refuse (a failure after the pull), and a name that is not derived from
/// the run, which cannot be found again.
#[test]
fn a_container_is_named_after_its_run() {
    let world = docker();
    let mut rt = open(&world);
    rt.create_and_start(&spec("r1")).unwrap();
    let args = created(&world, "r1");
    assert_eq!(args[..3], strings(&["create", "--name", "js-r1"])[..]);
    let longest = "a".repeat(125);
    for run in ["a.b_c-d", longest.as_str()] {
        rt.create_and_start(&spec(run)).unwrap();
        assert!(world.names().contains(&format!("js-{run}")), "{run}");
    }
    let creates = world.calls_of("create").len();
    let too_long = "a".repeat(126);
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

/// Asserts that a `create` whose outcome is not known leaves nothing behind: the call is
/// killed after the daemon made the container (a nonzero exit), or it exits 0 and prints
/// something that is no id (nothing, a message, an id of 12 digits). In each case the
/// container is removed by the name the agent gave it, `start` is never called, the error
/// is the create's, and a second start of the same run can use the name again. Before
/// this, a labelled container nobody knew stayed on the host, and a restarted agent
/// adopted it as a running job. It catches an orphan after a timeout.
#[test]
fn an_ambiguous_create_is_cleaned_up_by_name() {
    let cases: [(Result<&str, ()>, &str); 4] = [
        (Err(()), "create: /bin/sh: exit status: 1"),
        (Ok(""), "create: unreadable output"),
        (Ok("Unable to find image\n"), "create: unreadable output"),
        (Ok("0123456789ab\n"), "create: unreadable output"),
    ];
    for (outcome, text) in cases {
        let world = docker();
        let mut rt = open(&world);
        world.ambiguous_create(outcome);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(said.0, text, "{outcome:?}");
        assert!(
            world.ids().is_empty(),
            "{outcome:?}: the orphan was removed"
        );
        assert_eq!(
            world.calls_of("rm"),
            [strings(&["rm", "--force", "--volumes", "--", "js-r1"])],
            "{outcome:?}"
        );
        assert!(world.calls_of("start").is_empty(), "{outcome:?}");
        start(&mut rt, &world, "r1");
        assert_eq!(world.ids().len(), 1, "the name was free again");
    }
}

/// Asserts what the error says when the orphan cannot be removed: the removal failed and
/// the container is still listed (so the note names the failure), or the removal failed
/// and the listing failed too (the agent cannot say), and that the container then stays
/// visible to the next listing as the agent's own. It catches a cleanup failure that is
/// swallowed.
#[test]
fn a_cleanup_that_fails_is_reported() {
    let world = docker();
    let mut rt = open(&world);
    world.ambiguous_create(Err(()));
    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r1")).unwrap_err();
    assert_eq!(
        said.0,
        "create: /bin/sh: exit status: 1; the container could not be removed: rm: /bin/sh: exit status: 1"
    );
    assert_eq!(world.ids().len(), 1);

    world.ambiguous_create(Ok(""));
    world.fail("rm", 1);
    world.fail("ps", 1);
    let said = rt.create_and_start(&spec("r2")).unwrap_err();
    assert_eq!(
        said.0,
        "create: unreadable output; the container could not be removed: ps: /bin/sh: exit status: 1"
    );
}

/// Asserts that the mounts are read back after `create` and before `start`: every bind
/// the daemon made has the source the agent resolved, the destination and the access it
/// asked for, and there is no bind it did not ask for. A job that swapped a directory of
/// a mount root for a link while the daemon bound it (the daemon binds by path, and the
/// agent cannot give it a descriptor) leaves a different source; the container is then
/// removed, `start` is not called and the error says the mounts differ. Other mounts
/// (the tmpfs) are not looked at. It catches a mount outside its root that goes
/// unnoticed.
#[test]
fn the_mounts_are_read_back_before_the_start() {
    let swap = |index: usize, change: fn(&mut Bind)| {
        move |binds: &mut Vec<Bind>| change(&mut binds[index])
    };
    let cases: Vec<(&str, Change)> = vec![
        (
            "source swapped",
            Box::new(swap(1, |b| b.0 = "/etc".to_owned())),
        ),
        (
            "scratch moved",
            Box::new(swap(0, |b| b.0 = "/home/x".to_owned())),
        ),
        ("made writable", Box::new(swap(1, |b| b.2 = true))),
        (
            "other target",
            Box::new(swap(1, |b| b.1 = "/mnt/data/x".to_owned())),
        ),
        (
            "extra bind",
            Box::new(|binds: &mut Vec<Bind>| {
                binds.push(("/".to_owned(), "/host".to_owned(), true))
            }),
        ),
        (
            "bind missing",
            Box::new(|binds: &mut Vec<Bind>| {
                binds.pop();
            }),
        ),
    ];
    for (name, change) in cases {
        let world = docker();
        let mut rt = open(&world);
        world.tamper(change);
        let mut job = spec("r1");
        job.mounts = vec![mount("/srv/in", "/mnt/data/in", true)];
        let said = rt.create_and_start(&job).unwrap_err();
        assert_eq!(
            said.0, "the mounts of the container are not the ones the agent resolved",
            "{name}"
        );
        assert!(world.ids().is_empty(), "{name}: the container was removed");
        assert!(world.calls_of("start").is_empty(), "{name}");
        assert_eq!(world.calls_of("rm").len(), 1, "{name}");
    }
}

/// Asserts the call that reads the mounts back (the container type, the format that asks
/// for them as JSON, the id after `--`), that a container whose mounts are as asked is
/// started, and what happens when the answer cannot be used: not JSON, entries without
/// the fields, a container the runtime lost in between, or a removal that fails after a
/// mismatch. It catches a read-back that trusts an answer it did not understand.
#[test]
fn the_mount_read_back_and_its_failures() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    let inspect = &world.calls_of("inspect")[0];
    assert_eq!(
        inspect[..],
        strings(&[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{json .Mounts}}",
            "--",
            &id.0
        ])[..]
    );

    for printed in [
        "garbage",
        "{\"a\":1}",
        "[1]",
        "[{}]",
        "[{\"Type\":\"bind\"}]",
        "[{\"Type\":\"bind\",\"Source\":5,\"Destination\":\"/b\",\"RW\":true}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"RW\":true}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"Destination\":\"/b\"}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"Destination\":\"/b\",\"RW\":\"yes\"}]",
        "[{\"Type\":\"bind\",\"Source\":\"/a\",\"RW\":true,\"Destination\":5}]",
        "[{\"Type\":1,\"Source\":\"/a\",\"Destination\":\"/b\",\"RW\":true}]",
    ] {
        let world = docker();
        let mut rt = open(&world);
        world.say("inspect", printed);
        let said = rt.create_and_start(&spec("r2")).unwrap_err();
        assert_eq!(said.0, "inspect: unreadable output", "{printed:?}");
        assert!(world.ids().is_empty(), "{printed:?}");
    }

    let world = docker();
    let mut rt = open(&world);
    world.say("inspect", "null\n");
    let said = rt.create_and_start(&spec("r3")).unwrap_err();
    assert_eq!(
        said.0,
        "the mounts of the container are not the ones the agent resolved"
    );

    let world = docker();
    let mut rt = open(&world);
    world.fail("inspect", 1);
    let said = rt.create_and_start(&spec("r4")).unwrap_err();
    assert_eq!(said.0, "inspect: /bin/sh: exit status: 1");
    assert!(world.ids().is_empty(), "removed when the read-back failed");

    // The runtime lost the container between the create and the read-back.
    let world = docker();
    let mut rt = open(&world);
    world.fail("inspect", 1);
    world.say("ps", "");
    let said = rt.create_and_start(&spec("r6")).unwrap_err();
    assert_eq!(said.0, "inspect: unreadable output");
    assert!(world.calls_of("start").is_empty());

    let world = docker();
    let mut rt = open(&world);
    world.tamper(|binds| binds[0].0 = "/etc".to_owned());
    world.fail("rm", 1);
    let said = rt.create_and_start(&spec("r5")).unwrap_err();
    assert!(
        said.0
            .contains("; the container could not be removed: rm: "),
        "{}",
        said.0
    );
    assert_eq!(world.ids().len(), 1, "the leak is reported, not hidden");
}

/// Asserts what `poll` makes of each state the runtime prints, and what it does when it
/// cannot tell: running and the states that mean it still exists, exited with its code
/// and the memory controller's flag, a container the runtime no longer has as `Gone`
/// (not an error), and an error for a failure while it is there, for output that is not
/// three words, for a code that is not a number and for a state it does not know. A
/// `dead` container (one the daemon failed to remove) is a failure and never a success:
/// it keeps its code when it has one, and a code of 0 is not taken for a success, so it
/// is reported as gone, which ends the job as failed. It catches a gone container taken
/// for a failure (the job would be polled for ever), a failure taken for gone (the job
/// would end as failed while it runs), an unknown state taken for running and a dead
/// container reported `succeeded`.
#[test]
fn poll_maps_every_state() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    assert_eq!(rt.poll(&id), Ok(Status::Running));
    let ok = |code, oom_killed| Ok(Status::Exited(Exit { code, oom_killed }));
    let table = [
        ("running 0 false", Ok(Status::Running)),
        ("paused 0 false", Ok(Status::Running)),
        ("restarting 0 false", Ok(Status::Running)),
        ("removing 0 false", Ok(Status::Running)),
        ("created 0 false", Ok(Status::Running)),
        ("configured 0 false", Ok(Status::Running)),
        ("exited 3 false\n", ok(3, false)),
        ("dead 1 false", ok(1, false)),
        ("dead 0 false", Ok(Status::Gone)),
        ("dead 0 true", ok(0, true)),
        ("stopped 0 false", ok(0, false)),
        ("exited 137 true", ok(137, true)),
        ("exited -1 false", ok(-1, false)),
        (
            "dead x false",
            Err(RuntimeError("inspect: unreadable output".to_owned())),
        ),
        (
            "exited x false",
            Err(RuntimeError("inspect: unreadable output".to_owned())),
        ),
        (
            "exited 1",
            Err(RuntimeError("inspect: unreadable output".to_owned())),
        ),
        (
            "exited 1 false extra",
            Err(RuntimeError("inspect: unreadable output".to_owned())),
        ),
        (
            "weird 0 false",
            Err(RuntimeError("inspect: unreadable output".to_owned())),
        ),
        (
            "",
            Err(RuntimeError("inspect: unreadable output".to_owned())),
        ),
    ];
    for (printed, expected) in table {
        world.say("inspect", printed);
        assert_eq!(rt.poll(&id), expected, "{printed:?}");
    }
}

/// Asserts the `Gone` rule and its guard: a container removed from outside is `Gone`
/// (the failed `inspect` is followed by a look at `ps`, which no longer lists it); a
/// failed `inspect` of a container that is still listed is an error; when `ps` fails too
/// the runtime cannot say; and an id that is not a container id is refused before any
/// call. It catches a missing container read as a transient failure, and the reverse.
#[test]
fn a_missing_container_is_gone_and_a_failing_one_is_an_error() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    world.fail("inspect", 1);
    let said = rt.poll(&id).unwrap_err();
    assert_eq!(said.0, "inspect: /bin/sh: exit status: 1");
    world.fail("inspect", 1);
    world.fail("ps", 1);
    let said = rt.poll(&id).unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
    world.vanish("r1");
    assert_eq!(rt.poll(&id), Ok(Status::Gone));
    let ps = world.calls_of("ps");
    let last = ps.last().unwrap();
    assert_eq!(
        last[1..],
        strings(&[
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            &format!("id={}", id.0)
        ])[..]
    );

    let calls = world.calls().len();
    for bad in ["", "r1", "--all", "ABC123456789", "0123456789abcdefg"] {
        let said = rt.poll(&ContainerId(bad.to_owned())).unwrap_err();
        assert_eq!(said.0, "not a container id", "{bad:?}");
    }
    assert_eq!(world.calls().len(), calls, "no call with a bad id");
}

/// Asserts that an id of fewer than 64 digits is refused before any call by every verb,
/// even when it is the front of the id of a container that is there. `ps --no-trunc`
/// prints 64 digits and the agent compares whole lines, so a short id whose `inspect`,
/// `stop` or `rm` failed would be read as "gone" while the container runs. It catches that
/// confusion, and a listing that accepts a short id the engine printed.
#[test]
fn a_short_id_is_refused_while_the_container_is_there() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    let short = ContainerId(id.0[..12].to_owned());
    let calls = world.calls().len();
    assert_eq!(rt.poll(&short).unwrap_err().0, "not a container id");
    let stop = rt.stop(&short, Duration::from_secs(1)).unwrap_err();
    assert_eq!(stop.0, "not a container id");
    assert_eq!(rt.remove(&short).unwrap_err().0, "not a container id");
    assert_eq!(world.calls().len(), calls, "no call was made");
    assert_eq!(
        world.ids(),
        std::slice::from_ref(&id.0),
        "the container is still there"
    );

    world.say_once("ps", &format!("{}\n", short.0));
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "not a container id");
}

/// Asserts the `inspect` call itself: a container type, the format that asks for the
/// state, and the id after `--`. It catches an id that could be read as an option.
#[test]
fn poll_asks_for_the_container_state_after_dashes() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    rt.poll(&id).unwrap();
    let inspect = world.calls_of("inspect").pop().unwrap();
    assert_eq!(
        inspect[..4],
        strings(&["inspect", "--type", "container", "--format"])[..]
    );
    assert_eq!(
        inspect[4],
        "{{.State.Status}} {{.State.ExitCode}} {{.State.OOMKilled}}"
    );
    assert_eq!(inspect[5..], strings(&["--", &id.0])[..]);
}

/// Asserts `stop`: the grace in whole seconds rounded up (so a container is never given
/// less than it was promised), the id after `--`, the container exits, a stop of a
/// container that is gone succeeds, and one that fails while the container is there is
/// an error (and so is one whose `ps` also fails). It catches a stop that waits less
/// than the grace, one that fails on a missing container, which the lifecycle would
/// retry for ever, and one that hides a real failure.
#[test]
fn stop_rounds_the_grace_up_and_a_gone_container_is_stopped() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    for (grace, secs) in [
        (Duration::from_secs(10), "10"),
        (Duration::from_millis(1500), "2"),
        (Duration::from_millis(1), "1"),
        (Duration::ZERO, "0"),
    ] {
        rt.stop(&id, grace).unwrap();
        let stop = world.calls_of("stop").pop().unwrap();
        assert_eq!(stop, strings(&["stop", "--time", secs, "--", &id.0]));
    }
    assert_eq!(
        rt.poll(&id),
        Ok(Status::Exited(Exit {
            code: 137,
            oom_killed: false
        }))
    );

    world.fail("stop", 1);
    let said = rt.stop(&id, Duration::from_secs(1)).unwrap_err();
    assert_eq!(said.0, "stop: /bin/sh: exit status: 1");
    world.fail("stop", 1);
    world.fail("ps", 1);
    let said = rt.stop(&id, Duration::from_secs(1)).unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");

    world.vanish("r1");
    rt.stop(&id, Duration::from_secs(1)).unwrap();
    let said = rt
        .stop(&ContainerId("x".to_owned()), Duration::ZERO)
        .unwrap_err();
    assert_eq!(said.0, "not a container id");
}

/// Asserts `remove`: it forces, takes anonymous volumes with it, names the id after
/// `--`, succeeds on a container that is gone, and fails when the container is still
/// there and the call failed. It catches a removal that leaves a volume behind, one
/// that fails on a missing container, and one that hides a real failure.
#[test]
fn remove_forces_and_a_gone_container_is_removed() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    world.fail("rm", 1);
    let said = rt.remove(&id).unwrap_err();
    assert_eq!(said.0, "rm: /bin/sh: exit status: 1");
    world.fail("rm", 1);
    world.fail("ps", 1);
    let said = rt.remove(&id).unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
    rt.remove(&id).unwrap();
    let rm = world.calls_of("rm").pop().unwrap();
    assert_eq!(rm, strings(&["rm", "--force", "--volumes", "--", &id.0]));
    assert!(world.ids().is_empty());
    rt.remove(&id).unwrap();
    let said = rt.remove(&ContainerId("x".to_owned())).unwrap_err();
    assert_eq!(said.0, "not a container id");
}

fn by_run(found: &[Found]) -> Vec<(String, u64)> {
    let mut runs: Vec<(String, u64)> = found
        .iter()
        .map(|f| (f.labels[label::RUN].clone(), f.running_ms))
        .collect();
    runs.sort();
    runs
}

/// Asserts that the list finds only the containers that carry the run label and this
/// agent's label: another agent's container on the same runtime, a container with only
/// the run label (no agent) and one with only the agent label are all left out; and the
/// `ps` call asks the runtime to filter on both labels. It catches an agent that adopts
/// and then stops or removes another agent's job.
#[test]
fn ps_filter_finds_only_labelled() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "mine");
    let others = [
        &[(label::RUN, "theirs"), (label::AGENT, "h-other")][..],
        &[(label::RUN, "unowned")][..],
        &[(label::AGENT, HOST)][..],
        &[("team", "ml")][..],
    ];
    for labels in others {
        world.leak(labels);
    }
    let found = rt.list_labelled().unwrap();
    assert_eq!(by_run(&found), [("mine".to_owned(), 0)]);
    let ps = world.calls_of("ps").pop().unwrap();
    assert_eq!(
        ps,
        strings(&[
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            "label=jobscheduler.dev/run-id",
            "--filter",
            "label=jobscheduler.dev/agent=h-0123456789abcdef",
        ])
    );
    assert_eq!(found[0].labels["team"], "ml", "the labels come back whole");
    assert_eq!(found[0].id.0, world.id_of("mine").unwrap());
    let inspect = world.calls_of("inspect").pop().unwrap();
    assert_eq!(inspect[4], "{{.State.Status}} {{json .Config.Labels}}");
}

/// Asserts that a container that was created and never started is removed when the agent
/// lists its containers, and is not handed to the lifecycle: it holds no process and no
/// job, but its labels say it is a running one, so taking it over would hold its booking
/// until the wall time and report a job that never ran as timed out. That is what an
/// ambiguous create whose cleanup failed leaves behind. A removal that fails is an error
/// (the list is not complete), and Podman's own name for the state is treated the same.
/// It catches an orphan adopted as running.
#[test]
fn a_container_never_started_is_removed_on_adoption() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "live");
    let orphan = world.leak_created(&[
        (label::RUN, "orphan"),
        (label::AGENT, HOST),
        (label::CREATED_MS, "1000000"),
    ]);
    let found = rt.list_labelled().unwrap();
    assert_eq!(by_run(&found), [("live".to_owned(), 0)]);
    assert!(!world.ids().contains(&orphan), "the orphan was removed");
    assert_eq!(
        world.calls_of("rm"),
        [strings(&["rm", "--force", "--volumes", "--", &orphan])]
    );

    world.leak_created(&[(label::RUN, "again"), (label::AGENT, HOST)]);
    world.fail("rm", 1);
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "rm: /bin/sh: exit status: 1");
    assert_eq!(
        rt.list_labelled().unwrap().len(),
        1,
        "removed at the next listing"
    );

    let podman = Scripted::new("podman version 5.2.1");
    let mut rt = open_with(&podman, "linux", settings());
    let id = podman.leak(&[(label::RUN, "p"), (label::AGENT, HOST)]);
    podman.say("inspect", "configured {}\n");
    assert_eq!(rt.list_labelled().unwrap(), []);
    assert!(!podman.ids().contains(&id));
}

/// Asserts that the age of a listed container is the time since the agent made it, on
/// the wall clock: a runtime opened 30 seconds later reports 30000 ms; a clock that went
/// back reports 0 and not a huge number, however far back (a creation time of the
/// largest number) and a clock far ahead does not overflow; and a container without a
/// creation time (or with one that is not a number) is as old as it can be, so its job is
/// over its limit and not given a fresh one. The stamp is the wall clock because the
/// engines print their own times in formats that differ; a step of the clock therefore
/// moves the age (documented). It catches a restart that gives every job a new wall time
/// and arithmetic that wraps.
#[test]
fn the_age_of_a_listed_container_comes_from_its_creation_time() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "r1");
    let later = open_at(&world, "linux", thirty_seconds_on)
        .list_labelled()
        .unwrap();
    assert_eq!(by_run(&later), [("r1".to_owned(), 30_000)]);
    fn back() -> u64 {
        10
    }
    let earlier = open_at(&world, "linux", back).list_labelled().unwrap();
    assert_eq!(by_run(&earlier), [("r1".to_owned(), 0)]);
    fn far_ahead() -> u64 {
        u64::MAX
    }
    let ahead = open_at(&world, "linux", far_ahead).list_labelled().unwrap();
    assert_eq!(by_run(&ahead), [("r1".to_owned(), u64::MAX - 1_000_000)]);
    let max = u64::MAX.to_string();
    world.leak(&[
        (label::RUN, "from-the-future"),
        (label::AGENT, HOST),
        (label::CREATED_MS, &max),
    ]);
    world.leak(&[(label::RUN, "no-time"), (label::AGENT, HOST)]);
    world.leak(&[
        (label::RUN, "bad-time"),
        (label::AGENT, HOST),
        (label::CREATED_MS, "soon"),
    ]);
    let all = rt.list_labelled().unwrap();
    assert_eq!(
        by_run(&all),
        [
            ("bad-time".to_owned(), u64::MAX),
            ("from-the-future".to_owned(), 0),
            ("no-time".to_owned(), u64::MAX),
            ("r1".to_owned(), 0),
        ]
    );
}

/// Asserts the list's failures: `ps` failing is an error (the agent must not say it
/// holds nothing), an id that `ps` prints which is not an id is an error and never goes
/// into an argument, an answer that is not a state and the labels as JSON, or labels
/// that are not a JSON object of text, are an error, and `null` (a container with no
/// labels) reads as none. A container that disappears between the two calls is skipped.
/// It catches a list that silently drops containers it could not read and so lets the
/// server requeue running work.
#[test]
fn list_failures_are_errors_and_a_vanished_container_is_skipped() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "r1");
    world.fail("ps", 1);
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
    world.say_once("ps", "--all\n");
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "not a container id");

    for printed in [
        "running [1,2]",
        "running {\"a\":1}",
        "running garbage",
        "running ",
        "garbage",
        "",
    ] {
        world.say("inspect", printed);
        let said = rt.list_labelled().unwrap_err();
        assert_eq!(said.0, "inspect: unreadable output", "{printed:?}");
    }
    world.say("inspect", "running null\n");
    let found = rt.list_labelled().unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].labels.is_empty());

    let world = docker();
    let mut rt = open(&world);
    let id2 = start(&mut rt, &world, "r2");
    world.fail("inspect", 1);
    // The listing saw the container; it is gone by the time it is read.
    world.say_once("ps", &format!("{}\n", id2.0));
    world.vanish("r2");
    assert_eq!(rt.list_labelled().unwrap(), []);
}

/// Asserts that the engine is run with an environment of its own: every call, the
/// `--version` that opens the runtime included, goes through the runner's environment
/// call with exactly one variable, the pinned `PATH`, and with an engine host also
/// `DOCKER_HOST` and `CONTAINER_HOST` set to it, in that order and nothing else. None
/// goes through the call that passes the agent's environment on. It catches `LD_PRELOAD`,
/// `DOCKER_CONFIG`, `DOCKER_CONTEXT` or a `PATH` of the agent's steering the engine.
#[test]
fn the_engine_runs_with_a_pinned_path_and_nothing_inherited() {
    let exercise = |world: &Scripted, rt: &mut CliRuntime| {
        let id = start(rt, world, "r1");
        rt.poll(&id).unwrap();
        rt.stop(&id, Duration::from_secs(1)).unwrap();
        rt.list_labelled().unwrap();
        rt.remove(&id).unwrap();
    };
    let pinned = |extra: &[(&str, &str)]| -> Env {
        let mut env = vec![("PATH".to_owned(), PINNED_PATH.to_owned())];
        env.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        Some(env)
    };

    let world = docker();
    let mut rt = open(&world);
    exercise(&world, &mut rt);
    let envs = world.envs();
    assert_eq!(envs.len(), world.calls().len());
    assert!(envs.len() > 8);
    assert!(envs.iter().all(|env| *env == pinned(&[])), "{envs:?}");

    let world = docker();
    let uri = "unix:///run/engine/engine.sock";
    let set = settings().with_host(Some(uri)).unwrap();
    let mut rt = open_with(&world, "linux", set);
    exercise(&world, &mut rt);
    let expected = pinned(&[("DOCKER_HOST", uri), ("CONTAINER_HOST", uri)]);
    let envs = world.envs();
    assert!(envs.len() > 8);
    assert!(envs.iter().all(|env| *env == expected), "{envs:?}");
}

/// Asserts that the system runner started with an environment gives the program exactly
/// that environment: the program prints what it received, and nothing of the test
/// process's own environment (`HOME`, `PATH`, `CARGO_*`) is there. It catches a runner
/// that adds the given variables to the inherited ones.
#[test]
fn the_system_runner_gives_a_program_only_the_environment_it_is_given() {
    use js_agent::CommandRunner;
    let runner = js_agent::cli_runtime::system_runner(Duration::from_secs(10));
    let said = runner
        .run_in("/usr/bin/env", &[], &[("PATH", "/p"), ("A", "1")])
        .unwrap();
    let mut lines: Vec<&str> = said.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["A=1", "PATH=/p"]);
    let none = runner.run_in("/usr/bin/env", &[], &[]).unwrap();
    assert_eq!(none, "");
    let inherited = runner.run("/usr/bin/env", &[]).unwrap();
    assert!(
        inherited.contains("PATH="),
        "run still passes the environment on"
    );
}

thread_local! {
    static GIVEN: RefCell<Vec<(PathBuf, u32, u32)>> = const { RefCell::new(Vec::new()) };
}

fn record(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    GIVEN.with(|given| given.borrow_mut().push((path.to_path_buf(), uid, gid)));
    Ok(())
}

fn refuse(_path: &Path, _uid: u32, _gid: u32) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::PermissionDenied))
}

fn given() -> Vec<(PathBuf, u32, u32)> {
    GIVEN.with(|given| given.borrow().clone())
}

/// Asserts that an agent that runs as root gives the run's scratch directory to the
/// container's user before the container is created (the directory is `0700` and the
/// agent's, so the container's user could not write it), and that an agent whose own
/// user is the container's gives nothing away. A directory that cannot be given away is
/// an error before anything is created. It catches the default configuration mounting
/// `/scratch` read-write for a process that cannot write it.
#[test]
fn the_scratch_goes_to_the_containers_user_when_the_agent_is_root() {
    let world = docker();
    let root = js_agent::cli_runtime::Settings::new(Path::new(BIN), None, Some("2000:3000"))
        .unwrap()
        .with_agent(0, 0)
        .unwrap();
    let mut rt = open_with(&world, "linux", root.clone()).with_chown(record);
    rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(given(), [(PathBuf::from("/work/scratch/r1"), 2000, 3000)]);

    let own = root.with_agent(2000, 3000).unwrap();
    let mut rt = open_with(&world, "linux", own).with_chown(record);
    rt.create_and_start(&spec("r2")).unwrap();
    assert_eq!(given().len(), 1, "nothing was given away");

    let root = settings().with_agent(0, 0).unwrap();
    let mut rt = open_with(&world, "linux", root).with_chown(refuse);
    let creates = world.calls_of("create").len();
    let said = rt.create_and_start(&spec("r3")).unwrap_err();
    assert_eq!(said.0, "scratch: permission denied");
    assert_eq!(
        world.calls_of("create").len(),
        creates,
        "nothing was created"
    );
}

/// Asserts that the real hand-over works on a directory (to the owner it already has,
/// which any user may do), does not follow a link, and fails for a path that is not there.
#[test]
fn hand_over_changes_the_owner_of_the_directory_itself() {
    let dir = Scratch::new("cli-hand-over");
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let run = dir.path("run");
    std::fs::create_dir(&run).unwrap();
    hand_over(&run, uid, gid).unwrap();
    let link = dir.path("link");
    std::os::unix::fs::symlink(&run, &link).unwrap();
    hand_over(&link, uid, gid).unwrap();
    assert!(hand_over(&dir.path("absent"), uid, gid).is_err());
}

/// Asserts that a Docker engine that reports no swap limit support is warned about when
/// the runtime opens (`--memory-swap` is then ignored and a job can use swap beyond its
/// memory limit), and that nothing is said when it supports them, when the answer cannot
/// be had, or for Podman, whose `info` does not say. It catches a limit the operator
/// believes in that the host does not enforce.
#[test]
fn an_engine_without_swap_limits_is_warned_about() {
    let world = docker();
    world.say("info", "false\n");
    let rt = open(&world);
    assert_eq!(
        rt.warnings(),
        [
            "the engine reports no swap limit support: --memory-swap is ignored, so a job can use swap beyond its memory limit"
        ]
    );
    assert_eq!(
        world.calls_of("info"),
        [strings(&["info", "--format", "{{.SwapLimit}}"])]
    );
    for printed in ["true\n", "weird\n"] {
        let world = docker();
        world.say("info", printed);
        assert!(open(&world).warnings().is_empty(), "{printed:?}");
    }
    let world = docker();
    world.fail("info", 1);
    assert!(open(&world).warnings().is_empty());
    let podman = Scripted::new("podman version 5.2.1");
    assert!(
        open_with(&podman, "linux", settings())
            .warnings()
            .is_empty()
    );
    assert!(podman.calls_of("info").is_empty());
}

/// The wall clock the program stamps containers with reads a plausible time, and the
/// runner it gives the runtime is the bounded system runner.
#[test]
fn the_system_clock_and_runner_are_usable() {
    assert!(
        unix_ms() > 1_700_000_000_000,
        "milliseconds since the epoch"
    );
    let runner = js_agent::cli_runtime::system_runner(Duration::from_secs(10));
    let said = js_agent::CommandRunner::run(&runner, "/bin/sh", &["-c", "printf ok"]).unwrap();
    assert_eq!(said, "ok");
}
