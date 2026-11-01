//! `--placement` wired into the server, in process: the configuration read from the flags,
//! the agent side assembled from it as the server assembles it, hosts registered with the
//! registry, and leases started on its provider. No listener is bound and no connection is
//! made; the host listing is the body `GET /v1/hosts` serialises.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;
use js_lease::{ManualClock, Timestamp};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{Credential, RegisteredProvider};
use js_testkit::dir::TestDir;
use js_types::{Admission, ClassId, HostId, LeaseId, Request, ResourceClass, Resources};
use serde_json::{Value, json};

use crate::adapt::SharedClock;
use crate::agents::{Agents, assemble};
use crate::config::{Cli, Config};
use crate::hosts::HostList;

/// The join token of the pool.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// A pool of up to two machines with a node report: the scheduler describes a host by
/// its `arch`.
const POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// An empty directory of the test's own under the target directory, removed when the
/// guard is dropped. Its path carries the process id and a counter beside `name`, so two
/// runs of one test at once (two test binaries, or a test run twice) never share it.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/placement_wiring");
    TestDir::under(root, name)
}

fn write(dir: &Path, name: &str, text: &str, mode: u32) -> OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.into_os_string()
}

/// The configuration of a server with agent registration over [`POOL`] and `extra` flags.
fn config(dir: &Path, extra: &[&str]) -> Config {
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("leases.toml").into_os_string()),
        ("--boot", "initialise".into()),
        (
            "--pool-config",
            write(dir, "pool.toml", js_pool_static::EXAMPLE_CONFIG, 0o644),
        ),
        ("--token-file", write(dir, "token", &"t".repeat(40), 0o600)),
        (
            "--agent-token-file",
            write(dir, "agent-tokens", &format!("{JOIN}\n"), 0o600),
        ),
        (
            "--agent-pool-config",
            write(dir, "agent-pool.toml", POOL, 0o644),
        ),
    ];
    let mut args: Vec<OsString> = vec!["js-lease-server".into()];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args.extend(extra.iter().map(OsString::from));
    Config::from_cli(Cli::try_parse_from(args).unwrap()).unwrap()
}

fn hello(host: &str) -> Hello {
    let body = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    });
    serde_json::from_value(body).unwrap()
}

/// The agent side of a server, the provider of its registry, and the directory of their
/// files. The fields drop in order, so the directory goes after what writes into it.
struct Served {
    agents: Arc<Agents>,
    provider: RegisteredProvider,
    _dir: TestDir,
}

/// The agent side of a server over [`POOL`] with `extra` flags, assembled as the server
/// assembles it, with hosts `h1` and `h2` registered, and the provider of its registry
/// serving the class `share`.
fn serving(name: &str, extra: &[&str]) -> Served {
    let dir = scratch(name);
    let config = config(dir.path(), extra);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(1_000_000)));
    let (_source, agents) =
        assemble(config.hosts, config.usage, &clock, &config.state_file).unwrap();
    let agents = agents.unwrap();
    let credential = Credential::of_token(JOIN);
    for host in ["h1", "h2"] {
        agents.registry.hello(&hello(host), &credential).unwrap();
    }
    let provider = agents.registry.provider().with_classes(vec![sharing()]);
    Served {
        agents,
        provider,
        _dir: dir,
    }
}

/// Asserts that two fixtures made under one name at once have two directories, and that
/// dropping one leaves the other's files. It catches a scratch path made of the name alone,
/// which two runs of one test would share and remove under each other.
#[test]
fn fixtures_of_one_name_have_their_own_directories() {
    let first = scratch("same");
    let second = scratch("same");
    assert_ne!(first.path(), second.path());
    let file = write(first.path(), "kept", "x", 0o600);
    drop(second);
    assert!(Path::new(&file).is_file());
    let one = serving("same_served", &[]);
    let two = serving("same_served", &[]);
    assert_ne!(one._dir.path(), two._dir.path());
    let state = one._dir.path().join("leases.toml");
    drop(two);
    assert!(
        one._dir.path().join("agent-tokens").is_file(),
        "{}",
        state.display()
    );
}

fn sharing() -> ResourceClass {
    let id = ClassId::new("share").unwrap();
    ResourceClass::new(id, Admission::Book(Default::default()), 0)
}

/// A lease of one core of class `share`, numbered `sequence`.
fn sharing_spec(sequence: u64) -> StartSpec {
    let request = Request {
        class: sharing().id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

fn host_of(ack: &StartAck) -> Option<&str> {
    match ack {
        StartAck::Started { host } => Some(host.as_str()),
        _ => None,
    }
}

/// Asserts the default: with no flag, and with `--placement registry`, a lease of a class
/// that books part of a host is refused `UnsupportedClass`, as it always was, and nothing
/// is booked. It catches a default that turns the scheduler on.
#[test]
fn by_default_the_registry_places_and_a_sharing_class_is_refused() {
    for (name, extra) in [
        ("default", vec![]),
        ("registry", vec!["--placement", "registry"]),
    ] {
        let mut served = serving(name, &extra);
        let ack = served.provider.start(sharing_spec(1));
        assert_eq!(ack, StartAck::Refused(RefusalCode::UnsupportedClass));
        assert!(served.provider.booked().is_empty());
    }
}

/// Asserts that with `--placement js-sched` a lease is placed by the scheduler and not by
/// the registry's first-host-by-id: the scheduler breaks the tie between the two empty
/// hosts by a hash of the lease and the host, which gives lease 1 to `h2`, where the
/// registry would have booked `h1`. The second lease is placed beside the first on `h2`,
/// which has room and which the lease fits more tightly than the empty `h1`. The capacity
/// listing, the registry's view and the body of `GET /v1/hosts` read both back in booking
/// order; a guard, it fails if a listing shows only the first lease.
#[test]
fn with_the_flag_the_scheduler_places_the_leases() {
    let Served {
        agents,
        mut provider,
        _dir,
    } = serving("sched", &["--placement", "js-sched"]);
    let first = provider.start(sharing_spec(1));
    let second = provider.start(sharing_spec(2));
    assert_eq!(
        (host_of(&first), host_of(&second)),
        (Some("h2"), Some("h2"))
    );
    let listed: BTreeMap<HostId, Vec<LeaseId>> = provider
        .capacity()
        .into_iter()
        .map(|host| (host.host, host.leases))
        .collect();
    let held = vec![LeaseId::new(1, 1), LeaseId::new(1, 2)];
    assert_eq!(listed[&HostId::new("h2").unwrap()], held);
    assert!(listed[&HostId::new("h1").unwrap()].is_empty());

    let views = agents.registry.hosts();
    let of = |name: &str| views.iter().find(|v| v.id.as_str() == name).unwrap();
    assert_eq!(of("h2").leases, held);
    assert_eq!(of("h2").lease, Some(held[0]));
    assert!(of("h1").leases.is_empty());
    let reserved = agents.registry.reserved();
    let list = HostList::of(provider.capacity(), views.clone(), Some(&reserved));
    let body: Value = serde_json::to_value(&list).unwrap();
    let hosts = body["hosts"].as_array().unwrap();
    let entry = |name: &str| hosts.iter().find(|h| h["id"] == name).unwrap().clone();
    assert_eq!(entry("h2")["leases"], json!(["1-1", "1-2"]));
    assert_eq!(entry("h2")["lease"], "1-1");
    assert!(entry("h1").get("leases").is_none());
}

/// Asserts that a host with room takes a second lease and a full one takes none: eight
/// leases of one core land on `h2` (the lease fits the loaded host more tightly than the
/// empty one), and the ninth, with `h2`'s eight cores booked, goes to `h1`.
#[test]
fn with_the_flag_a_host_with_room_takes_a_second_lease_and_a_full_one_none() {
    let mut served = serving("full", &["--placement", "js-sched"]);
    let hosts: Vec<Option<String>> = (1..=9)
        .map(|n| host_of(&served.provider.start(sharing_spec(n))).map(str::to_owned))
        .collect();
    let named = |host: &str| Some(host.to_owned());
    let mut expected = vec![named("h2"); 8];
    expected.push(named("h1"));
    assert_eq!(hosts, expected);
}
