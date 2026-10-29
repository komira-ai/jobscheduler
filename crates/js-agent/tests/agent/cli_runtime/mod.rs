//! The container runtime over the `docker` and `podman` command line: the arguments it
//! builds, the flags that configure it, its answers to every call and its failures, the
//! lifecycle driven over it, and one run against a stub program the test writes. Every
//! test but the stub's answers through [`scripted::Scripted`], which starts no process;
//! docker and podman themselves are never run.

mod args;
mod lifecycle;
mod logs;
mod runtime;
mod scripted;
mod settings;
mod stub;
mod subid;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use js_agent::Platform;
use js_agent::cli_runtime::{CliRuntime, PROFILE_FILE, Settings};
use js_agent::lifecycle::{ContainerSpec, ResolvedMount, label};
use js_agent_wire::Isolation;

use scripted::{DOCKER, Scripted};

use crate::support::Scratch;

/// The host id the runtimes of these tests register under.
pub const HOST: &str = "h-0123456789abcdef";

/// The `--mount` value of the scratch directory `/work/scratch/r1` on Docker: a volume
/// of the local driver that binds the directory `nosuid,nodev` (Docker's `--mount
/// type=bind` takes neither), with no copy of the image's `/scratch` into it. The option
/// list holds commas, so that field is quoted, as the engine reads the value as CSV.
pub const DOCKER_SCRATCH: &str = r#"type=volume,dst=/scratch,volume-driver=local,volume-nocopy,volume-opt=type=none,volume-opt=device=/work/scratch/r1,"volume-opt=o=bind,nosuid,nodev""#;

/// The wall clock of a runtime that made its containers.
pub fn made_at() -> u64 {
    1_000_000
}

/// The wall clock of a runtime opened 30 seconds later.
pub fn thirty_seconds_on() -> u64 {
    1_030_000
}

/// The name of the container the agent `HOST` gives run `r1`: `js-`, the 16 digits of the
/// FNV-1a hash of the host id, `-`, the run id. Pinned, so that a change of the hash (it
/// would orphan nothing, but names are documented) shows.
pub const NAME_R1: &str = "js-368fcd37c22ac926-r1";

/// A program that exists and can be executed, for settings that are never run.
pub const BIN: &str = "/bin/sh";

pub fn settings() -> Settings {
    Settings::new(Path::new(BIN), None, None).unwrap()
}

pub fn platform(os: &str) -> Platform {
    Platform {
        os: os.to_owned(),
        arch: "x86_64".to_owned(),
    }
}

/// A runtime over `world` on `os`, with `now` as its wall clock.
pub fn open_at(world: &Scripted, os: &str, now: fn() -> u64) -> CliRuntime {
    open_as(world, os, settings(), now)
}

/// A runtime over `world` on `os` with `set` as its settings.
pub fn open_with(world: &Scripted, os: &str, set: Settings) -> CliRuntime {
    open_as(world, os, set, made_at)
}

fn open_as(world: &Scripted, os: &str, set: Settings, now: fn() -> u64) -> CliRuntime {
    CliRuntime::open(set, HOST, &platform(os), now, Box::new(world.clone()))
        .unwrap()
        .with_profile_dir(profile_dir())
        .with_pid_seen(sees_the_engines_pid)
        .with_proc_files(no_file, in_another_pid_namespace)
}

/// The directory the scripted runtimes keep the seccomp profile in: their jobs name
/// scratch directories that do not exist, so the directory above is no place for it.
/// One for the whole test process; it is removed with the target directory.
pub fn profile_dir() -> &'static Path {
    static DIR: OnceLock<(Scratch, PathBuf)> = OnceLock::new();
    let (_, dir) = DIR.get_or_init(|| {
        let owner = Scratch::new("profile");
        let dir = owner.path("profiles");
        fs::create_dir(&dir).unwrap();
        (owner, dir)
    });
    dir
}

/// The `--security-opt` value those runtimes give: `seccomp=` and the file.
pub fn profile_option() -> String {
    format!("seccomp={}", profile_dir().join(PROFILE_FILE).display())
}

/// The process id the scripted engine gives a running container.
pub const ENGINE_PID: u32 = 4000;

/// A `/proc` with no readable file.
pub fn no_file(_path: &str) -> Option<String> {
    None
}

/// Links that show the engine's process in a pid namespace other than the agent's.
pub fn in_another_pid_namespace(path: &str) -> Option<String> {
    match path {
        "/proc/self/ns/pid" => Some("pid:[1]".to_owned()),
        _ => Some("pid:[2]".to_owned()),
    }
}

/// Links that show every process in the agent's own pid namespace.
pub fn in_the_agents_pid_namespace(_path: &str) -> Option<String> {
    Some("pid:[1]".to_owned())
}

/// An agent whose `/proc` shows the process the scripted engine reports.
pub fn sees_the_engines_pid(pid: u32) -> bool {
    pid == ENGINE_PID
}

/// A Docker runtime over `world` on Linux.
pub fn open(world: &Scripted) -> CliRuntime {
    open_at(world, "linux", made_at)
}

/// A new world that prints Docker's version.
pub fn docker() -> Scripted {
    Scripted::new(DOCKER)
}

/// A contained job of run `run_id`: 1.5 CPUs, 1 GiB, 100 processes, no GPU, no mount.
pub fn spec(run_id: &str) -> ContainerSpec {
    let labels: BTreeMap<String, String> = [
        (label::RUN, run_id),
        (label::LEASE, "lease-1"),
        ("team", "ml"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    ContainerSpec {
        run_id: run_id.to_owned(),
        image: "registry.example/job:1".to_owned(),
        args: vec!["--go".to_owned(), "x y".to_owned()],
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        pids: Some(100),
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch: PathBuf::from("/work/scratch").join(run_id),
        slot: 0,
        isolation: Isolation::Contained,
        labels,
    }
}

pub fn mount(host: &str, target: &str, ro: bool) -> ResolvedMount {
    ResolvedMount {
        host: PathBuf::from(host),
        target: target.to_owned(),
        ro,
    }
}

/// The arguments of the `create` that made the container of `run_id`.
pub fn created(world: &Scripted, run_id: &str) -> Vec<String> {
    world.create_of(&world.id_of(run_id).unwrap())
}
