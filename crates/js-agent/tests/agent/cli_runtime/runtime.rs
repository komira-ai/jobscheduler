//! `CliRuntime` over a runner that answers like the command line: every call, every
//! answer, and every failure.

use std::time::Duration;

use js_agent::cli_runtime::{CliError, CliRuntime, Engine, unix_ms};
use js_agent::lifecycle::{
    ContainerId, ContainerRuntime, Exit, Found, RuntimeError, Status, label,
};
use js_agent_wire::Isolation;

use super::scripted::Scripted;
use super::{
    HOST, created, docker, made_at, mount, open, open_at, platform, settings, spec,
    thirty_seconds_on,
};

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Starts `run_id` in `world` and returns its container id.
fn start(rt: &mut CliRuntime, world: &Scripted, run_id: &str) -> ContainerId {
    let id = rt.create_and_start(&spec(run_id)).unwrap();
    assert_eq!(Some(id.0.clone()), world.id_of(run_id));
    id
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

/// Asserts the whole `create` argv of a job with a GPU, mounts and labels: the order of
/// the groups, that the agent's labels are there, and that the image and the command
/// line are the last entries after `--`. It is the shape the documentation promises.
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
        "type=bind,src=/srv/in,dst=/mnt/data/in,readonly",
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
        "65534:65534",
        "--",
        "registry.example/job:1",
        "--go",
        "x y",
    ]);
    assert_eq!(created(&world, "r1"), expected);
    let calls = world.calls();
    assert_eq!(calls[1], expected, "after the --version call");
    assert_eq!(calls[2][0], "start");
}

/// Asserts that a Podman runtime is given the same job with CDI devices in place of
/// `--gpus`, and nothing else different.
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
    rt.create_and_start(&job).unwrap();
    let args = created(&podman, "r1");
    let at = args.iter().position(|a| a == "--device").unwrap();
    assert_eq!(args[at + 1], "nvidia.com/gpu=3");
    assert!(!args.contains(&"--gpus".to_owned()));
    assert!(args.contains(&"--read-only".to_owned()));
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
            ("--tmpfs", "/tmp"),
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

/// Asserts that the network and user flags reach the container: a named network instead
/// of none, and the configured uid and gid.
#[test]
fn a_configured_network_and_user_are_used() {
    let world = docker();
    let set = js_agent::cli_runtime::Settings::new(
        std::path::Path::new(super::BIN),
        Some("jobs"),
        Some("2000:3000"),
    )
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
}

/// Asserts the refusals that create nothing and run nothing: a host job, a mount path
/// that is not safe, and a label key that is not. It catches a host job run in a
/// container, and a refused job that still reaches the runtime.
#[test]
fn refused_jobs_reach_the_runtime_not_at_all() {
    let world = docker();
    let mut rt = open(&world);
    let mut host = spec("r1");
    host.isolation = Isolation::Host;
    let mut bad_mount = spec("r2");
    bad_mount.mounts = vec![mount("/srv/a,src=/etc", "/mnt/a", false)];
    let mut bad_label = spec("r3");
    bad_label.labels.insert("a=b".to_owned(), "v".to_owned());
    let said = [host, bad_mount, bad_label].map(|job| rt.create_and_start(&job).unwrap_err());
    assert_eq!(said[0].0, "only a contained job runs in a container");
    assert!(said[1].0.contains("mount path"), "{}", said[1].0);
    assert!(said[2].0.contains("label key"), "{}", said[2].0);
    assert_eq!(world.calls().len(), 1, "only --version was run");
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
/// program's and its status) leaves nothing and tries nothing else, and that a `create`
/// that prints something other than an id is an error before `start` is called, for
/// each kind of wrong output: empty, not hexadecimal, too short, too long. It catches an
/// id taken from output that is not one and then put in later arguments.
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
        long.as_str(),
        "ABCDEF0123456789",
    ] {
        world.say("create", printed);
        let said = rt.create_and_start(&spec("r1")).unwrap_err();
        assert_eq!(said.0, "create: unreadable output", "{printed:?}");
    }
    assert_eq!(world.calls_of("start").len(), 0);
    let id = "0123456789ab";
    world.say("create", &format!("Unable to find image locally\n{id}\n"));
    world.say("start", "");
    let started = rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(started.0, id, "the id is the last line");
}

/// Asserts what `poll` makes of each state the runtime prints, and what it does when it
/// cannot tell: running and the states that mean it still exists, exited with its code
/// and the memory controller's flag, a container the runtime no longer has as `Gone`
/// (not an error), and an error for a failure while it is there, for output that is not
/// three words, for a code that is not a number and for a state it does not know. It
/// catches a gone container taken for a failure (the job would be polled for ever), a
/// failure taken for gone (the job would end as failed while it runs) and an unknown
/// state taken for running.
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
        ("stopped 0 false", ok(0, false)),
        ("exited 137 true", ok(137, true)),
        ("exited -1 false", ok(-1, false)),
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

/// Asserts the `inspect` call itself: a container type, the format that asks for the
/// state, and the id after `--`. It catches an id that could be read as an option.
#[test]
fn poll_asks_for_the_container_state_after_dashes() {
    let world = docker();
    let mut rt = open(&world);
    let id = start(&mut rt, &world, "r1");
    rt.poll(&id).unwrap();
    let inspect = &world.calls_of("inspect")[0];
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
}

/// Asserts that the age of a listed container is the time since the agent made it, on
/// the wall clock: a runtime opened 30 seconds later reports 30000 ms; a clock that went
/// back reports 0 and not a huge number; and a container without a creation time (or
/// with one that is not a number) is as old as it can be, so its job is over its limit
/// and not given a fresh one. It catches a restart that gives every job a new wall time.
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
            ("no-time".to_owned(), u64::MAX),
            ("r1".to_owned(), 0),
        ]
    );
}

/// Asserts the list's failures: `ps` failing is an error (the agent must not say it
/// holds nothing), an id that `ps` prints which is not an id is an error and never goes
/// into an argument, labels that are not a JSON object of text are an error, and `null`
/// (a container with no labels) reads as none. A container that disappears between the
/// two calls is skipped. It catches a list that silently drops containers it could not
/// read and so lets the server requeue running work.
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

    for printed in ["[1,2]", "{\"a\":1}", "garbage", ""] {
        world.say("inspect", printed);
        let said = rt.list_labelled().unwrap_err();
        assert_eq!(said.0, "inspect: unreadable output", "{printed:?}");
    }
    world.say("inspect", "null\n");
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
