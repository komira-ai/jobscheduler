//! The container runtime over the `docker` and `podman` command line: the arguments it
//! builds, the flags that configure it, its answers to every call and its failures, the
//! lifecycle driven over it, and one run against a stub program the test writes. Every
//! test but the stub's answers through [`scripted::Scripted`], which starts no process;
//! docker and podman themselves are never run.

mod args;
mod lifecycle;
mod runtime;
mod scripted;
mod settings;
mod stub;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use js_agent::Platform;
use js_agent::cli_runtime::{CliRuntime, Settings};
use js_agent::lifecycle::{ContainerSpec, ResolvedMount, label};
use js_agent_wire::Isolation;

use scripted::{DOCKER, Scripted};

/// The host id the runtimes of these tests register under.
pub const HOST: &str = "h-0123456789abcdef";

/// The wall clock of a runtime that made its containers.
pub fn made_at() -> u64 {
    1_000_000
}

/// The wall clock of a runtime opened 30 seconds later.
pub fn thirty_seconds_on() -> u64 {
    1_030_000
}

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
    CliRuntime::open(
        settings(),
        HOST,
        &platform(os),
        now,
        Box::new(world.clone()),
    )
    .unwrap()
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
