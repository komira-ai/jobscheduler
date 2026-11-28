//! `server.cold_cpu_millis` and `server.cold_memory_bytes` read from a configuration file and
//! reaching the placement engine: the agent side assembled from the file as the server
//! assembles it, one host of 88 cores and 251 GiB registered, and a pass over a queued job
//! that names no CPU or memory. And `--cold-memory-bytes-on` and `--cold-cpu-millis-on`, the cold
//! memory and CPU of the jobs of an operating system, over one Mac of 28 cores and 96 GiB. No
//! listener is bound and no connection is made.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_jobs::{Booked, JobId, JobSpec, JobState, Needs, NewJob};
use js_registry::Credential;
use js_sched_placer::sizes::{ColdCpuOn, ColdMemoryOn};
use js_types::{ManualClock, Resources, Timestamp};
use serde_json::json;

use crate::agent_api::job_rig::{spec, tables};
use crate::agents::assemble_placed;
use crate::clock::SharedClock;
use crate::config::Cli;
use crate::config::{ColdOn, Config};
use crate::kinds::{Kinds, OwnerOnlyFiles};
use crate::sched::engine_rig::{GIB, T0};
use crate::sched::wiring_tests::{JOIN, config_of, config_result, scratch, write};

/// The host's memory: 251 GiB.
const MEMORY: u64 = 251 * GIB;

/// A configuration file of a joined pool `farm` of hosts of 88 cores and 251 GiB, its
/// `[server]` table ending with `extra`, written into `dir`.
fn file_text(dir: &std::path::Path, extra: &str) -> String {
    let token = write(dir, "client-token", &"t".repeat(40), 0o600);
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
shape = {{ cpu_millis = 88000, memory_bytes = {MEMORY} }}
capabilities = {{ arch = "x86_64" }}
"#,
        state = path(&state),
        token = path(&token),
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
    let (config, shared) = file_config(dir.path(), extra);
    booking_of(config, &shared, &FARM)
}

/// The configuration of the file written into `dir` whose `[server]` table ends with `extra`,
/// read on a clock at [`T0`], and that clock.
fn file_config(dir: &std::path::Path, extra: &str) -> (Config, SharedClock) {
    let text = file_text(dir, extra);
    let path = write(dir, "js.toml", &text, 0o644);
    let shared = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let config = Config::from_config_file(
        std::path::Path::new(&path),
        &Kinds::standard(),
        &OwnerOnlyFiles,
        shared.clone(),
    )
    .unwrap();
    (config, shared)
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
    booking_of(config, &shared, &FARM)
}

/// The agent pool `macs` of `--agent-pool-config`: Macs of 28 cores and 96 GiB.
const MAC_POOL: &str = r#"
name = "macs"
class = "farm-host"
max_machines = 1

[shape]
cpu_millis = 28000
memory_bytes = 103079215104

[capabilities]
arch = "arm64"
os = "macos"
"#;

/// The host a test registers and what its job names: its pool, its CPU and memory, its
/// capabilities, and the `os` the job's constraints give (none for `None`).
struct Platform {
    pool: &'static str,
    cpu_millis: u64,
    memory_bytes: u64,
    caps: &'static [(&'static str, &'static str)],
    os: Option<&'static str>,
}

/// A farm host of the pool `farm`, whose job names no `os`.
const FARM: Platform = Platform {
    pool: "farm",
    cpu_millis: 88_000,
    memory_bytes: MEMORY,
    caps: &[("arch", "x86_64")],
    os: None,
};

/// A Mac of the pool `macs`, whose job names `os` `macos`.
const MAC: Platform = Platform {
    pool: "macs",
    cpu_millis: 28_000,
    memory_bytes: 96 * GIB,
    caps: &[("arch", "arm64"), ("os", "macos")],
    os: Some("macos"),
};

/// What the server started with the flags of [`config_of`] over [`MAC_POOL`] and `extra`
/// books for a `macos` job, as [`flag_booking`].
fn mac_flag_booking(name: &str, extra: &[&str]) -> Booked {
    let dir = scratch(name);
    let config = config_of(dir.path(), MAC_POOL, extra);
    let shared = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    booking_of(config, &shared, &MAC)
}

/// What the agent side assembled from `config` on `shared` books for a queued job that names
/// no CPU or memory (and the `os` of `on`) and whose class has no runs, on its one registered
/// host, of `on`.
fn booking_of(config: Config, shared: &SharedClock, on: &Platform) -> Booked {
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
    let caps: serde_json::Map<String, serde_json::Value> = on
        .caps
        .iter()
        .map(|(key, value)| ((*key).to_owned(), json!(value)))
        .collect();
    let hello = serde_json::from_value(json!({
        "host_id": "h1",
        "pool": on.pool,
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": on.cpu_millis, "memory_bytes": on.memory_bytes, "gpus": 0 },
        "capabilities": caps,
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
    let mut spec = naming_nothing("kbf.cargo-test.a");
    spec.constraints.os = on.os.map(str::to_owned);
    let new = NewJob {
        id: Some(id.clone()),
        spec,
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
/// books the default, 22 cores (a quarter of that host's cores) and 4 GiB (not a quarter of its
/// memory, 62.75 GiB); and that one setting only the memory keeps the default cores. Catches keys read and then
/// dropped on the way from the file to the engine, and a default taken from the host's size.
#[test]
fn the_file_s_cold_size_is_what_the_server_books_a_class_with_no_runs() {
    let farm = "cold_cpu_millis = 2000\ncold_memory_bytes = 8589934592";
    assert_eq!(cold_booking("cold_farm", farm), share(2000, 8 * GIB));
    assert_eq!(cold_booking("cold_unset", ""), share(22_000, 4 * GIB));
    let memory = "cold_memory_bytes = 8589934592";
    assert_eq!(cold_booking("cold_memory", memory), share(22_000, 8 * GIB));
}

/// Asserts that `--cold-cpu-millis 2000 --cold-memory-bytes 8589934592` make the server book a
/// job that names nothing, of a class with no runs, two cores and 8 GiB on a host of 88 cores
/// and 251 GiB; that `--cold-cpu-millis` alone keeps the default memory (4 GiB); and that a
/// server started from its flags with neither books the default, 22 cores and 4 GiB. Catches
/// flags read and then dropped, and a flag default other than the file's (a server started
/// from its flags, as a farm's is, would book a quarter of the host).
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
    assert_eq!(flag_booking("cold_flag_cpu", &cpu), share(2000, 4 * GIB));
    assert_eq!(flag_booking("cold_flag_none", &[]), share(22_000, 4 * GIB));
}

/// Asserts that a flag below its least (249 millicores, a byte under 1 GiB), negative or not a
/// number is refused naming the flag, and that either flag without `--agent-pool-config` is
/// refused naming it, its default value (22000, 4294967296) too. Catches a cold size of no
/// CPU, one under the estimator's least memory, and a flag accepted where nothing reads it
/// (a flag given at its default is still given).
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
        ("--cold-cpu-millis", "22000"),
        ("--cold-memory-bytes", "4294967296"),
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

/// Asserts that `--cold-memory-bytes-on macos=4294967296` makes the server book a `macos` job
/// that names nothing, of a class with no runs, 4 GiB on a Mac, beside a server-wide
/// `--cold-memory-bytes` of 16 GiB, with the cold CPU of `--cold-cpu-millis`; that without it
/// the same job books the 16 GiB; and that an entry for another operating system changes
/// nothing for it. Catches the flag read and
/// then dropped on the way to the engine, and an entry applied to the jobs of every system.
#[test]
fn the_cold_memory_of_an_os_is_what_the_server_books_its_class_with_no_runs() {
    let shared = [
        "--cold-cpu-millis",
        "4000",
        "--cold-memory-bytes",
        "17179869184",
    ];
    let on_macs = [&shared[..], &["--cold-memory-bytes-on", "macos=4294967296"]].concat();
    assert_eq!(
        mac_flag_booking("cold_on_macos", &on_macs),
        share(4000, 4 * GIB)
    );
    assert_eq!(
        mac_flag_booking("cold_on_none", &shared),
        share(4000, 16 * GIB)
    );
    let on_linux = [&shared[..], &["--cold-memory-bytes-on", "linux=4294967296"]].concat();
    assert_eq!(
        mac_flag_booking("cold_on_linux", &on_linux),
        share(4000, 16 * GIB)
    );
}

/// Asserts that a `--cold-memory-bytes-on` value that is not `OS=BYTES`, names no system, or
/// gives bytes under 1 GiB or that are not a whole number is refused naming the flag and what
/// is wrong; that a system given twice is refused at start; and that the flag without
/// `--agent-pool-config` is refused naming it. Catches a cold memory under the estimator's
/// least, one entry silently taking the place of another, and a flag accepted where nothing
/// reads it.
#[test]
fn the_cold_memory_of_an_os_is_refused_out_of_range_twice_and_without_a_pool() {
    for (bad, why) in [
        ("macos", "must be OS=BYTES, such as macos=4294967296"),
        ("=4294967296", "the operating system is empty"),
        (
            "macos=1073741823",
            "macos: 1073741823 is below the least, 1073741824 (1 GiB)",
        ),
        ("macos=4GiB", "\"4GiB\" is not a whole number of bytes"),
    ] {
        let given = format!("--cold-memory-bytes-on={bad}");
        let said = Cli::try_parse_from(["js-lease-server", &given])
            .err()
            .unwrap()
            .to_string();
        assert!(
            said.contains("'--cold-memory-bytes-on <OS=BYTES>'"),
            "{bad}: {said}"
        );
        assert!(said.contains(why), "{bad}: {said}");
    }
    let dir = scratch("cold_on_twice");
    let twice = [
        "--cold-memory-bytes-on",
        "macos=4294967296",
        "--cold-memory-bytes-on",
        "macos=8589934592",
    ];
    let said = config_result(dir.path(), MAC_POOL, &twice, "initialise")
        .err()
        .unwrap()
        .to_string();
    assert_eq!(said, "--cold-memory-bytes-on: macos is given twice");

    let dir = scratch("cold_on_no_pool");
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
        "--cold-memory-bytes-on".into(),
        "macos=4294967296".into(),
    ];
    let said = Config::from_cli(Cli::try_parse_from(args).unwrap())
        .err()
        .unwrap()
        .to_string();
    assert_eq!(said, "--cold-memory-bytes-on: requires --agent-pool-config");
}

/// The Mac's cold size this server sets beside a server-wide 4 cores and 16 GiB: one core and
/// 4 GiB for `macos`.
const MAC_COLD: [&str; 8] = [
    "--cold-cpu-millis",
    "4000",
    "--cold-memory-bytes",
    "17179869184",
    "--cold-memory-bytes-on",
    "macos=4294967296",
    "--cold-cpu-millis-on",
    "macos=1000",
];

/// Asserts that `--cold-cpu-millis-on macos=1000` makes the server book a `macos` job that
/// names nothing, of a class with no runs, one core on a Mac in place of `--cold-cpu-millis`'s
/// four, with the cold memory of `--cold-memory-bytes-on` beside it or the server-wide memory
/// without it; that an entry for another operating system changes nothing for it; and that on a
/// Linux farm host a job that names no `os` books the server-wide 4 cores and 16 GiB whether
/// the Mac's entries are given or not. Catches the flag read and then dropped on the way to the
/// engine, an entry applied to the jobs of every system, and Mac entries that change what the
/// Linux hosts book.
#[test]
fn the_cold_cpu_of_an_os_is_what_the_server_books_its_class_with_no_runs() {
    assert_eq!(
        mac_flag_booking("cold_cpu_on_macos", &MAC_COLD),
        share(1000, 4 * GIB)
    );
    let cpu_only = [&MAC_COLD[..4], &MAC_COLD[6..]].concat();
    assert_eq!(
        mac_flag_booking("cold_cpu_on_cpu_only", &cpu_only),
        share(1000, 16 * GIB)
    );
    let on_linux = [&MAC_COLD[..4], &["--cold-cpu-millis-on", "linux=1000"]].concat();
    assert_eq!(
        mac_flag_booking("cold_cpu_on_linux", &on_linux),
        share(4000, 16 * GIB)
    );
    for (name, extra) in [
        ("cold_cpu_on_farm_none", &MAC_COLD[..4]),
        ("cold_cpu_on_farm_macs", &MAC_COLD[..]),
    ] {
        assert_eq!(
            flag_booking(name, extra),
            share(4000, 16 * GIB),
            "{extra:?}"
        );
    }
}

/// Asserts that a `--cold-cpu-millis-on` value that is not `OS=MILLIS`, names no system, or
/// gives millicores under 250, over 4294967295 or that are not a whole number is refused naming
/// the flag and what is wrong; that a system given twice is refused at start; and that the flag
/// without `--agent-pool-config` is refused naming it. Catches a cold CPU under the
/// estimator's least, one entry silently taking the place of another, and a flag accepted
/// where nothing reads it.
#[test]
fn the_cold_cpu_of_an_os_is_refused_out_of_range_twice_and_without_a_pool() {
    let range = "is not from 250 (a quarter core) to 4294967295";
    for (bad, why) in [
        ("macos", "must be OS=MILLIS, such as macos=1000".to_owned()),
        ("=1000", "the operating system is empty".to_owned()),
        ("macos=249", format!("macos: 249 {range}")),
        ("macos=4294967296", format!("macos: 4294967296 {range}")),
        (
            "macos=1000m",
            "\"1000m\" is not a whole number of millicores".to_owned(),
        ),
    ] {
        let given = format!("--cold-cpu-millis-on={bad}");
        let said = Cli::try_parse_from(["js-lease-server", &given])
            .err()
            .unwrap()
            .to_string();
        assert!(
            said.contains("'--cold-cpu-millis-on <OS=MILLIS>'"),
            "{bad}: {said}"
        );
        assert!(said.contains(&why), "{bad}: {said}");
    }
    let dir = scratch("cold_cpu_on_twice");
    let twice = [
        "--cold-cpu-millis-on",
        "macos=1000",
        "--cold-cpu-millis-on",
        "macos=2000",
    ];
    let said = config_result(dir.path(), MAC_POOL, &twice, "initialise")
        .err()
        .unwrap()
        .to_string();
    assert_eq!(said, "--cold-cpu-millis-on: macos is given twice");

    let dir = scratch("cold_cpu_on_no_pool");
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
        "--cold-cpu-millis-on".into(),
        "macos=1000".into(),
    ];
    let said = Config::from_cli(Cli::try_parse_from(args).unwrap())
        .err()
        .unwrap()
        .to_string();
    assert_eq!(said, "--cold-cpu-millis-on: requires --agent-pool-config");
}

/// Asserts that a configuration file's `cold_cpu_millis_on` and `cold_memory_bytes_on` reach
/// the agent side the server assembles, each as the file gives it, and that a file that gives
/// neither leaves both empty. Catches the file's per-system CPU read and then dropped.
#[test]
fn the_file_s_cold_cpu_and_memory_of_an_os_reach_the_agent_side() {
    let extra = "cold_cpu_millis_on = { macos = 1000 }\n\
                 cold_memory_bytes_on = { macos = 4294967296 }";
    let dir = scratch("cold_on_file");
    let (config, _) = file_config(dir.path(), extra);
    let cpu = BTreeMap::from([("macos".to_owned(), 1000)]);
    let memory = BTreeMap::from([("macos".to_owned(), 4 * GIB)]);
    let both = ColdOn {
        memory: ColdMemoryOn::new(memory),
        cpu: ColdCpuOn::new(cpu),
    };
    assert_eq!(config.agents.cold_on, both);
    let dir = scratch("cold_on_file_none");
    let (config, _) = file_config(dir.path(), "");
    assert_eq!(config.agents.cold_on, ColdOn::default());
}
