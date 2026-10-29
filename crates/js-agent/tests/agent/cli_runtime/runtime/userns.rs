//! Every job in a user namespace mapped onto one subordinate range: the maps Podman is
//! given, what Docker's daemon must be, the scratch directory's owner, and the refusals
//! when the runtime opens (which is before the agent says hello).

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

use js_agent::cli_runtime::{CliError, CliRuntime, Settings, SubidError};
use js_agent::integrity::{FakeHost, Integrity};
use js_agent::lifecycle::ContainerRuntime;

use super::super::scripted::Scripted;
use super::super::subid::host;
use super::super::{
    HOST, created, docker, in_another_pid_namespace, made_at, no_file, platform, profile_dir,
    sees_the_engines_pid, settings, spec,
};
use crate::support::{FakeFiles, Scratch};

/// What Podman prints for `--version`.
const PODMAN: &str = "podman version 5.2.0\n";

/// Docker's security options and data root when the daemon remaps onto the range of
/// `exact_host()`.
const REMAPPED: &str =
    "[\"name=seccomp,profile=builtin\",\"name=userns\"] /var/lib/docker/231072.462144\n";

/// `host()` with one entry of exactly 65536 ids for `jobmap` in each file, the allocation
/// Docker needs; the other user's range does not touch it.
fn exact_host() -> FakeFiles {
    host()
        .with("/etc/subuid", "alice:100000:65536\njobmap:231072:65536\n")
        .with("/etc/subgid", "alice:100000:65536\njobmap:462144:65536\n")
}

/// The settings of an agent that runs as root and maps its jobs onto `jobmap`'s range.
fn mapped() -> Settings {
    settings()
        .with_subuid("jobmap")
        .unwrap()
        .with_agent(0, 0)
        .unwrap()
}

fn podman() -> Scripted {
    Scripted::new(PODMAN)
}

/// Opens a runtime over `world` with `files` for `/etc` and `claims` for the claims.
fn open_in(
    world: &Scripted,
    set: Settings,
    files: &FakeFiles,
    claims: &Path,
) -> Result<CliRuntime, CliError> {
    CliRuntime::open_in(
        set,
        HOST,
        &platform("linux"),
        made_at,
        Box::new(world.clone()),
        files,
        claims,
    )
    .map(|rt| {
        rt.with_profile_dir(profile_dir())
            .with_pid_seen(sees_the_engines_pid)
            .with_proc_files(no_file, in_another_pid_namespace)
    })
}

/// The runtime of `mapped()` over `world`, with `/etc` as `exact_host()` has it.
fn open_mapped(world: &Scripted, claims: &Scratch) -> Result<CliRuntime, CliError> {
    open_in(world, mapped(), &exact_host(), &claims.path("claims"))
}

thread_local! {
    static GIVEN: RefCell<Vec<(PathBuf, u32, u32)>> = const { RefCell::new(Vec::new()) };
}

fn record(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    GIVEN.with(|given| given.borrow_mut().push((path.to_path_buf(), uid, gid)));
    Ok(())
}

fn maps(args: &[String]) -> Vec<String> {
    let at = args.iter().position(|arg| arg == "--uidmap");
    at.map(|at| args[at..at + 4].to_vec()).unwrap_or_default()
}

/// Asserts that every `create` for Podman carries `--uidmap 0:<start>:65536` and `--gidmap
/// 0:<start>:65536` of the range read from the id files (the uid and the gid start are
/// not the same number here), right before the `--` that ends the options, for the second
/// container as for the first, and that Podman's daemon is asked nothing. It catches a
/// container created in the host's user namespace and a gid map built from the uid
/// start.
#[test]
fn podman_creates_every_container_with_both_maps() {
    let world = podman();
    let dir = Scratch::new("userns-podman");
    let mut rt = open_mapped(&world, &dir).unwrap().with_chown(record);
    rt.create_and_start(&spec("r1")).unwrap();
    rt.create_and_start(&spec("r2")).unwrap();
    for run in ["r1", "r2"] {
        let args = created(&world, run);
        let want = ["--uidmap", "0:231072:65536", "--gidmap", "0:462144:65536"];
        assert_eq!(maps(&args), want, "{args:?}");
        let end = args.iter().position(|arg| arg == "--").unwrap();
        let at = args.iter().position(|arg| arg == "--uidmap").unwrap();
        assert_eq!(at + 4, end, "the maps are the last options: {args:?}");
        assert_eq!(args.iter().filter(|arg| *arg == "--user").count(), 1);
    }
    assert!(world.calls_of("info").is_empty());
}

/// Asserts that Docker's daemon is accepted when its security options name `userns` (in
/// any position, with or without the options that follow it) and its data root names
/// the first ids of the range, that the one question asked of the engine is the
/// documented one, and that no map is put in the `create` (Docker has none per
/// container). It catches a Docker `create` given maps it would refuse, and a daemon that
/// is asked something else than what the agent reads.
#[test]
fn docker_that_remaps_onto_the_range_is_accepted_and_given_no_maps() {
    for printed in [
        REMAPPED,
        "[\"name=userns\"] /srv/docker/231072.462144\n",
        "[\"name=userns,profile=x\",\"name=seccomp\"] /var/lib/docker/231072.462144\n",
    ] {
        let world = docker();
        world.say("info", printed);
        let dir = Scratch::new("userns-docker-ok");
        let mut rt = open_mapped(&world, &dir).unwrap().with_chown(record);
        rt.create_and_start(&spec("r1")).unwrap();
        let args = created(&world, "r1");
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("--uidmap") || arg.starts_with("--gidmap")),
            "{args:?}"
        );
        let asked = world.calls_of("info");
        let question = [
            "info",
            "--format",
            "{{json .SecurityOptions}} {{.DockerRootDir}}",
        ];
        assert!(asked.iter().any(|call| *call == question), "{asked:?}");
    }
}

/// Asserts that Docker without `userns-remap` is refused when the runtime opens, whatever
/// the way the options show it (no `userns`, a rootless daemon, `null`, a look-alike
/// name), with the message that names the fix: `"userns-remap": "<user>"` in `daemon.json`
/// and the user's entry in `/etc/subuid`. It catches jobs that run in the host's user
/// namespace on Docker.
#[test]
fn docker_without_userns_remap_is_refused_with_the_fix() {
    for printed in [
        "[\"name=seccomp,profile=builtin\"] /var/lib/docker\n",
        "[\"name=rootless\"] /var/lib/docker\n",
        "null /var/lib/docker\n",
        "[\"name=usernsx\",\"userns\"] /var/lib/docker/231072.462144\n",
    ] {
        let world = docker();
        world.say("info", printed);
        let dir = Scratch::new("userns-docker-off");
        let said = open_mapped(&world, &dir).err().unwrap();
        assert_eq!(
            said,
            CliError::NoUserns {
                user: "jobmap".to_owned()
            },
            "{printed}"
        );
        let text = said.to_string();
        assert!(
            text.contains("\"userns-remap\": \"jobmap\"")
                && text.contains("daemon.json")
                && text.contains("/etc/subuid"),
            "{text}"
        );
        assert!(world.calls_of("create").is_empty());
    }
}

/// Asserts that Docker is refused when it remaps onto another range than the one
/// `--container-subuid` names: the uid start differs, the gid start differs, or the data
/// root names no ids at all. The message names the data root, the range wanted and the
/// fix. It catches a daemon whose remap user is not the one the agent checked
/// `/etc/passwd` and `/etc/group` against.
#[test]
fn docker_that_remaps_elsewhere_is_refused() {
    for root in [
        "/var/lib/docker/100000.462144",
        "/var/lib/docker/231072.100000",
        "/var/lib/docker/231072",
        "/var/lib/docker/a.b",
        "/var/lib/docker",
    ] {
        let world = docker();
        world.say("info", &format!("[\"name=userns\"] {root}\n"));
        let dir = Scratch::new("userns-docker-elsewhere");
        let said = open_mapped(&world, &dir).err().unwrap();
        assert_eq!(
            said,
            CliError::RemapDiffers {
                user: "jobmap".to_owned(),
                want: "231072.462144".to_owned(),
                root: root.to_owned(),
            }
        );
        let text = said.to_string();
        assert!(
            text.contains(root) && text.contains("231072.462144") && text.contains("userns-remap"),
            "{text}"
        );
    }
}

/// Asserts that an answer of the engine the agent cannot read ends the open as an
/// unreadable `info` (no data root, no JSON), and a failing call as that call's failure.
/// It catches a daemon that cannot be asked being taken for one that remaps.
#[test]
fn docker_answers_the_agent_cannot_read_are_refused() {
    for printed in ["garbage\n", "not-json /var/lib/docker/231072.462144\n"] {
        let world = docker();
        world.say("info", printed);
        let dir = Scratch::new("userns-docker-garbage");
        let said = open_mapped(&world, &dir).err().unwrap();
        assert_eq!(said, CliError::Unreadable { call: "info" }, "{printed}");
    }
    let world = docker();
    world.fail("info", 2);
    let dir = Scratch::new("userns-docker-fails");
    let said = open_mapped(&world, &dir).err().unwrap();
    assert!(
        matches!(said, CliError::Call { call: "info", .. }),
        "{said:?}"
    );
}

/// Asserts that the run's scratch directory is given to the host ids the container user's
/// ids are mapped onto, the start of the range plus the id for the uid and for the gid
/// (the default user, and one other), by an agent that runs as root. It catches a scratch
/// directory the job cannot write because it belongs to the unmapped ids.
#[test]
fn the_scratch_is_given_to_the_mapped_ids() {
    let world = podman();
    let dir = Scratch::new("userns-scratch");
    let claims = dir.path("claims");
    let mut rt = open_in(&world, mapped(), &exact_host(), &claims)
        .unwrap()
        .with_chown(record);
    rt.create_and_start(&spec("r1")).unwrap();
    let other = Settings::new(Path::new(super::super::BIN), None, Some("2000:3000"))
        .unwrap()
        .with_subuid("jobmap")
        .unwrap()
        .with_agent(0, 0)
        .unwrap();
    drop(rt);
    let mut rt = open_in(&world, other, &exact_host(), &claims)
        .unwrap()
        .with_chown(record);
    rt.create_and_start(&spec("r2")).unwrap();
    let given = GIVEN.with(|given| given.borrow().clone());
    assert_eq!(
        given,
        [
            (
                PathBuf::from("/work/scratch/r1"),
                231_072 + 65_534,
                462_144 + 65_534
            ),
            (
                PathBuf::from("/work/scratch/r2"),
                231_072 + 2000,
                462_144 + 3000
            ),
        ]
    );
}

/// Asserts that an agent that is not root cannot map its jobs: it could not give the
/// scratch directory to the range, so the runtime does not open (and claims nothing).
/// It catches a job that starts with a scratch directory it cannot write.
#[test]
fn a_mapped_runtime_needs_an_agent_that_is_root() {
    let world = podman();
    let dir = Scratch::new("userns-not-root");
    let own = settings()
        .with_subuid("jobmap")
        .unwrap()
        .with_agent(65534, 65534)
        .unwrap();
    let said = open_in(&world, own, &exact_host(), &dir.path("claims"))
        .err()
        .unwrap();
    assert_eq!(said, CliError::AgentNotRoot);
    assert!(said.to_string().contains("must run as root"), "{said}");
    assert!(!dir.path("claims").exists());
}

/// Asserts that a range the id files do not allow stops the runtime before anything is
/// created or claimed, with the error of the range: a range from 0 (the identity map, a
/// container's root would be the host's), a range that holds a user of `/etc/passwd`, and a
/// user with no entry. It catches an agent that starts with a range that maps a container
/// id onto a host user.
#[test]
fn an_unusable_range_stops_the_runtime_when_it_opens() {
    let world = podman();
    let dir = Scratch::new("userns-bad-range");
    let claims = dir.path("claims");
    let identity = exact_host().with("/etc/subuid", "jobmap:0:65536\n");
    let said = open_in(&world, mapped(), &identity, &claims).err().unwrap();
    assert_eq!(
        said,
        CliError::Subid(SubidError::Identity {
            user: "jobmap".to_owned(),
            file: "/etc/subuid"
        })
    );
    let holds_a_user = exact_host().with("/etc/passwd", "alice:x:231100:1000::/:/bin/sh\n");
    let said = open_in(&world, mapped(), &holds_a_user, &claims)
        .err()
        .unwrap();
    assert!(
        matches!(
            said,
            CliError::Subid(SubidError::Overlaps { id: 231_100, .. })
        ),
        "{said:?}"
    );
    let nobody = settings()
        .with_subuid("nobody-here")
        .unwrap()
        .with_agent(0, 0)
        .unwrap();
    let said = open_in(&world, nobody, &exact_host(), &claims)
        .err()
        .unwrap();
    assert!(
        matches!(said, CliError::Subid(SubidError::NoEntry { .. })),
        "{said:?}"
    );
    assert!(!claims.exists(), "nothing was claimed");
    assert!(world.calls_of("create").is_empty());
}

/// Asserts that a second runtime whose range overlaps the range of a runtime that is open
/// is refused, whether it is the same user or another whose range shares an id, and that
/// the range can be used again when the first runtime is gone. It catches two agents on one
/// host running jobs as the same host ids.
#[test]
fn a_second_runtime_on_an_overlapping_range_is_refused() {
    let world = podman();
    let dir = Scratch::new("userns-second");
    let claims = dir.path("claims");
    let first = open_in(&world, mapped(), &exact_host(), &claims).unwrap();
    let other_files = exact_host()
        .with("/etc/subuid", "other:250000:65536\n")
        .with("/etc/subgid", "other:600000:65536\n");

    let again = open_in(&world, mapped(), &exact_host(), &claims)
        .err()
        .unwrap();
    assert_eq!(
        again,
        CliError::Subid(SubidError::Claimed("231072-462144".to_owned()))
    );
    let other = settings()
        .with_subuid("other")
        .unwrap()
        .with_agent(0, 0)
        .unwrap();
    let overlapping = open_in(&world, other.clone(), &other_files, &claims)
        .err()
        .unwrap();
    assert_eq!(
        overlapping,
        CliError::Subid(SubidError::Claimed("231072-462144".to_owned()))
    );
    assert!(
        overlapping.to_string().contains("another agent"),
        "{overlapping}"
    );

    drop(first);
    open_in(&world, other, &other_files, &claims).unwrap();
}

/// Asserts that Docker is refused when the user's allocation is not exactly one entry of
/// exactly 65536 ids in each file (a larger count, a second entry), since Docker maps the
/// whole allocation and the checks watch 65536 ids, and that Podman, whose map is
/// explicit, takes the same files. It catches Docker container ids that no check watches.
#[test]
fn docker_refuses_an_allocation_larger_than_the_range_and_podman_takes_it() {
    let larger = exact_host().with("/etc/subuid", "jobmap:231072:65537\n");
    let two = exact_host().with("/etc/subgid", "jobmap:462144:65536\njobmap:1000000:65536\n");
    for (files, file) in [(larger, "/etc/subuid"), (two, "/etc/subgid")] {
        let world = docker();
        world.say("info", REMAPPED);
        let dir = Scratch::new("userns-docker-exact");
        let said = open_in(&world, mapped(), &files, &dir.path("claims"))
            .err()
            .unwrap();
        assert_eq!(
            said,
            CliError::Subid(SubidError::NotExact {
                user: "jobmap".to_owned(),
                file
            })
        );
        assert!(!dir.path("claims").exists(), "nothing was claimed");
        let world = podman();
        open_in(&world, mapped(), &files, &dir.path("claims")).unwrap();
    }
}

/// Asserts that Docker's daemon is asked again before every create: after the daemon
/// stops remapping (or remaps elsewhere) the next create is refused with the reason, no
/// container is made and the scratch directory is not given away; the first container,
/// made while it remapped, is untouched. It catches jobs started outside the range after
/// a daemon restart.
#[test]
fn docker_is_asked_again_before_every_create() {
    let world = docker();
    world.say("info", REMAPPED);
    let dir = Scratch::new("userns-docker-again");
    let mut rt = open_mapped(&world, &dir).unwrap().with_chown(record);
    let before = GIVEN.with(|given| given.borrow().len());
    rt.create_and_start(&spec("r1")).unwrap();
    assert_eq!(GIVEN.with(|given| given.borrow().len()), before + 1);
    let asked = world.calls_of("info").len();

    world.say(
        "info",
        "[\"name=seccomp,profile=builtin\"] /var/lib/docker\n",
    );
    let said = rt.create_and_start(&spec("r2")).unwrap_err();
    assert!(
        said.0.contains("\"userns-remap\": \"jobmap\""),
        "{}",
        said.0
    );
    assert_eq!(
        world.calls_of("info").len(),
        asked + 1,
        "one call per create"
    );
    assert_eq!(world.calls_of("create").len(), 1);
    assert_eq!(world.ids().len(), 1);
    assert_eq!(GIVEN.with(|given| given.borrow().len()), before + 1);

    world.say("info", "[\"name=userns\"] /var/lib/docker/100000.100000\n");
    assert!(rt.create_and_start(&spec("r3")).is_err());
    world.say("info", REMAPPED);
    rt.create_and_start(&spec("r4")).unwrap();
}

/// Asserts that the checks watch the range the runtime resolved when it opened: the id
/// files read afterwards (here, edited to name another range) are not consulted, so a
/// process in the opened range is found and one in the edited range is not. It catches
/// checks that follow an edit of /etc/subuid while jobs run in the old range.
#[test]
fn the_checks_keep_the_range_the_runtime_opened_with() {
    let world = podman();
    let dir = Scratch::new("userns-resolved");
    let set = mapped();
    let _rt = open_in(&world, set.clone(), &exact_host(), &dir.path("claims")).unwrap();

    let edited = FakeHost::quiet();
    edited.set_file("/etc/subuid", "jobmap:900000:65536\n");
    edited.set_file("/etc/subgid", "jobmap:900000:65536\n");
    let mut checks = Integrity::start(Box::new(edited.clone()), set.uid(), vec![]).unwrap();
    let leftover = |pid, uid| js_agent::integrity::Process {
        pid,
        ppid: 100,
        uid,
        start: 900,
        cgroup: "0::/user.slice".to_owned(),
    };
    edited.start_process(leftover(730, 900_005));
    let clean = js_agent::lifecycle::Finished {
        run_id: "r1",
        live: &[],
    };
    assert!(js_agent::lifecycle::CheckHook::run(&mut checks, &clean).is_ok());
    edited.start_process(leftover(731, 231_072 + 5));
    assert!(js_agent::lifecycle::CheckHook::run(&mut checks, &clean).is_err());
}
