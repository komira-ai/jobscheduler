//! `server.cold_cpu_millis` and `server.cold_memory_bytes` read from a configuration file and
//! reaching the placement engine: the agent side assembled from the file as the server
//! assembles it, one host of 88 cores and 251 GiB registered, and a pass over a queued job
//! that names no CPU or memory. No listener is bound and no connection is made.

use std::sync::Arc;

use js_jobs::{Booked, JobId, JobSpec, JobState, Needs, NewJob};
use js_registry::Credential;
use js_types::{ManualClock, Resources, Timestamp};
use serde_json::json;

use crate::agent_api::job_rig::{spec, tables};
use crate::agents::assemble_placed;
use crate::clock::SharedClock;
use crate::config::Cli;
use crate::config::Config;
use crate::kinds::{Kinds, OwnerOnlyFiles};
use crate::sched::engine_rig::{GIB, T0};
use crate::sched::wiring_tests::{JOIN, config_of, scratch, write};

/// The host's memory: 251 GiB.
const MEMORY: u64 = 251 * GIB;

/// A configuration file of a joined pool `farm` of hosts of 88 cores and 251 GiB, its
/// `[server]` table ending with `extra`, written into `dir`.
fn file_text(dir: &std::path::Path, extra: &str) -> String {
    let token = write(dir, "client-token", &"t".repeat(40), 0o600);
    let join = write(dir, "join-tokens", &format!("{JOIN}\n"), 0o600);
    let path = |p: &std::ffi::OsString| format!("{:?}", p.to_string_lossy());
    let state = dir.join("leases.toml").into_os_string();
    format!(
        r#"[server]
state_file = {state}
boot = "initialise"
client_token_file = {token}
agent_listen = "127.0.0.2:0"
{extra}

[[pool]]
kind = "joined"
name = "farm"
class = "farm-host"
exec = "none"
max_machines = 1
join_token_file = {join}
shape = {{ cpu_millis = 88000, memory_bytes = {MEMORY} }}
capabilities = {{ arch = "x86_64" }}
"#,
        state = path(&state),
        token = path(&token),
        join = path(&join),
    )
}

/// A job of the class `class` that names no CPU or memory.
fn naming_nothing(class: &str) -> JobSpec {
    let labels = std::collections::BTreeMap::from([("size-class".to_owned(), class.to_owned())]);
    JobSpec {
        needs: Needs::Shape(Resources::new(0, 0, 0, None).unwrap()),
        labels: js_types::Labels::from_caller(&labels).unwrap(),
        ..spec()
    }
}

/// What the server whose file ends its `[server]` table with `extra` books for a queued job
/// that names no CPU or memory and whose class has no runs, on its one registered host.
fn cold_booking(name: &str, extra: &str) -> Booked {
    let dir = scratch(name);
    let text = file_text(dir.path(), extra);
    let path = write(dir.path(), "js.toml", &text, 0o644);
    let shared = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let config = Config::from_config_file(
        std::path::Path::new(&path),
        &Kinds::standard(),
        &OwnerOnlyFiles,
        shared.clone(),
    )
    .unwrap();
    booking_of(config, &shared)
}

/// The agent pool `farm` of `--agent-pool-config`: hosts of 88 cores and 251 GiB.
const FARM_POOL: &str = r#"
name = "farm"
class = "farm-host"
max_machines = 1

[shape]
cpu_millis = 88000
memory_bytes = 269509197824

[capabilities]
arch = "x86_64"
"#;

/// What the server started with the flags of [`config_of`] over [`FARM_POOL`] and `extra`
/// books, as [`cold_booking`].
fn flag_booking(name: &str, extra: &[&str]) -> Booked {
    let dir = scratch(name);
    let config = config_of(dir.path(), FARM_POOL, extra);
    let shared = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    booking_of(config, &shared)
}

/// What the agent side assembled from `config` on `shared` books for a queued job that names
/// no CPU or memory and whose class has no runs, on its one registered host of the pool `farm`.
fn booking_of(config: Config, shared: &SharedClock) -> Booked {
    let (agents, _) = assemble_placed(
        config.agents,
        config.usage,
        shared,
        &config.state_file,
        None,
    )
    .unwrap();
    let (_store, jobs) = tables();
    let agents = Arc::try_unwrap(agents).ok().unwrap();
    let agents = agents.with_jobs(Arc::new(jobs));
    let hello = serde_json::from_value(json!({
        "host_id": "h1",
        "pool": "farm",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 88000, "memory_bytes": MEMORY, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    }))
    .unwrap();
    agents
        .registry
        .hello(&hello, &Credential::of_token(JOIN))
        .unwrap();
    let jobs = Arc::clone(agents.jobs.as_ref().unwrap());
    let id = JobId::parse("job-a").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: naming_nothing("kbf.cargo-test.a"),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    agents.place_jobs();
    let job = jobs.get_job(&id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Placed);
    jobs.get_allocation(&id).unwrap().unwrap().booked
}

/// A share booking of `cpu_millis` and `memory_bytes`.
fn share(cpu_millis: u64, memory_bytes: u64) -> Booked {
    Booked::Shape {
        cpu_millis,
        memory_bytes,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// Asserts that a server whose file sets `cold_cpu_millis = 2000` and
/// `cold_memory_bytes = 8589934592` books a job that names nothing, of a class with no runs,
/// two cores and 8 GiB on a host of 88 cores and 251 GiB; that one whose file sets neither
/// books a quarter of that host (22 cores and 62.75 GiB, as before the keys); and that one
/// setting only the memory keeps the quarter of the cores. Catches keys read and then dropped
/// on the way from the file to the engine.
#[test]
fn the_file_s_cold_size_is_what_the_server_books_a_class_with_no_runs() {
    let farm = "cold_cpu_millis = 2000\ncold_memory_bytes = 8589934592";
    assert_eq!(cold_booking("cold_farm", farm), share(2000, 8 * GIB));
    assert_eq!(cold_booking("cold_unset", ""), share(22_000, MEMORY / 4));
    let memory = "cold_memory_bytes = 8589934592";
    assert_eq!(cold_booking("cold_memory", memory), share(22_000, 8 * GIB));
}

/// Asserts that `--cold-cpu-millis 2000 --cold-memory-bytes 8589934592` make the server book a
/// job that names nothing, of a class with no runs, two cores and 8 GiB on a host of 88 cores
/// and 251 GiB, and that `--cold-cpu-millis` alone keeps a quarter of the host's memory.
/// Catches flags read and then dropped (a server started from its flags, as a farm's is,
/// would book a quarter of the host).
#[test]
fn the_flags_cold_size_is_what_the_server_books_a_class_with_no_runs() {
    let both = [
        "--cold-cpu-millis",
        "2000",
        "--cold-memory-bytes",
        "8589934592",
    ];
    assert_eq!(flag_booking("cold_flags", &both), share(2000, 8 * GIB));
    let cpu = ["--cold-cpu-millis", "2000"];
    assert_eq!(flag_booking("cold_flag_cpu", &cpu), share(2000, MEMORY / 4));
}

/// Asserts that a flag below its least (249 millicores, a byte under 1 GiB), negative or not a
/// number is refused naming the flag, and that either flag without `--agent-pool-config` is
/// refused naming it. Catches a cold size of no CPU, one under the estimator's least memory,
/// and a flag accepted where nothing reads it.
#[test]
fn the_cold_flags_are_refused_out_of_range_and_without_a_pool() {
    for (flag, bad) in [
        ("--cold-cpu-millis", "249"),
        ("--cold-cpu-millis", "4294967296"),
        ("--cold-cpu-millis", "-1"),
        ("--cold-cpu-millis", "2k"),
        ("--cold-memory-bytes", "1073741823"),
        ("--cold-memory-bytes", "8GiB"),
    ] {
        let given = format!("{flag}={bad}");
        let said = Cli::try_parse_from(["js-lease-server", &given])
            .err()
            .unwrap()
            .to_string();
        assert!(said.contains(&format!("'{flag} <")), "{given}: {said}");
    }
    for (flag, value) in [
        ("--cold-cpu-millis", "2000"),
        ("--cold-memory-bytes", "8589934592"),
    ] {
        let dir = scratch("cold_flag_no_pool");
        let token = write(dir.path(), "token", &"t".repeat(40), 0o600);
        let state = dir.path().join("leases.toml").into_os_string();
        let args: Vec<std::ffi::OsString> = vec![
            "js-lease-server".into(),
            "--state-file".into(),
            state,
            "--boot".into(),
            "initialise".into(),
            "--token-file".into(),
            token,
            flag.into(),
            value.into(),
        ];
        let said = Config::from_cli(Cli::try_parse_from(args).unwrap())
            .err()
            .unwrap()
            .to_string();
        assert_eq!(said, format!("{flag}: requires --agent-pool-config"));
    }
}
