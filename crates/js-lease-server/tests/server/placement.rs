//! `--placement`: who picks the host of a lease on a host that registered itself. The
//! default is the registry, which is the server as it was; `js-sched` has the scheduler
//! choose among the hosts with room and lets leases of a class that books part of a host
//! share one.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::Duration;

use js_lease_server::{Cli, Config, Placement};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, ClassId, HostId, LeaseId, Request, ResourceClass, Resources};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, post, session};
use crate::config_support::{PoolText, file, load, quote, quoted, refusal, server};
use crate::support::{Harness, Scratch, base_args, config};

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

fn hello(host: &str) -> Value {
    json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    })
}

/// The flags of a server with agent registration over [`POOL`], and `extra` after them.
fn args(scratch: &Scratch, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let pool = scratch.write("agent-pool.toml", POOL, 0o644);
    let flags = [
        ("--agent-token-file", tokens.into_os_string()),
        ("--agent-pool-config", pool.into_os_string()),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args.extend(extra.iter().map(OsString::from));
    args
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

/// A server over [`POOL`] with `extra` flags and hosts `h1` and `h2` joined, and the
/// provider of its registry serving the class `share`.
async fn serving(name: &str, extra: &[&str]) -> (Harness, js_registry::RegisteredProvider) {
    let scratch = Scratch::new(name);
    let flags = args(&scratch, extra);
    let h = Harness::with(scratch, flags, None).await;
    for host in ["h1", "h2"] {
        let reply = post(&h, "/v1/agent/hello", Some(JOIN), &hello(host)).await;
        session(&reply, host);
    }
    let registry = h.running.registry().unwrap();
    let provider = registry.provider().with_classes(vec![sharing()]);
    (h, provider)
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
#[tokio::test]
async fn by_default_the_registry_places_and_a_sharing_class_is_refused() {
    for (name, extra) in [
        ("placement_default", vec![]),
        ("placement_registry", vec!["--placement", "registry"]),
    ] {
        let (_h, mut provider) = serving(name, &extra).await;
        let ack = provider.start(sharing_spec(1));
        assert_eq!(ack, StartAck::Refused(RefusalCode::UnsupportedClass));
        assert!(provider.booked().is_empty());
    }
}

/// Asserts that with `--placement js-sched` a run is placed by the scheduler and not by the
/// registry's first-host-by-id: the scheduler breaks the tie between the two empty hosts by
/// a hash of the lease and the host, which gives lease 1 to `h2`, where the registry would
/// have booked `h1`. The second lease is placed beside the first on `h2`, which has room
/// and which the lease fits more tightly than the empty `h1`. The capacity listing reads
/// both back.
#[tokio::test]
async fn with_the_flag_the_scheduler_places_the_leases() {
    let (_h, mut provider) = serving("placement_sched", &["--placement", "js-sched"]).await;
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

    // The registered listing (the registry's view and `GET /v1/hosts`) shows both leases
    // of `h2` in booking order and none for `h1`. A guard: it fails if the listing shows
    // only the first lease.
    let views = _h.running.registry().unwrap().hosts();
    let of = |name: &str| views.iter().find(|v| v.id.as_str() == name).unwrap();
    assert_eq!(of("h2").leases, held);
    assert_eq!(of("h2").lease, Some(held[0]));
    assert!(of("h1").leases.is_empty());
    let body = _h.call("GET", "/v1/hosts", None).await.json();
    let hosts = body["hosts"].as_array().unwrap();
    let entry = |name: &str| hosts.iter().find(|h| h["id"] == name).unwrap().clone();
    assert_eq!(entry("h2")["leases"], serde_json::json!(["1-1", "1-2"]));
    assert_eq!(entry("h2")["lease"], "1-1");
    assert!(entry("h1").get("leases").is_none());
}

/// Asserts that a host with room takes a second lease and a full one takes none: eight
/// leases of one core land on `h2` (the lease fits the loaded host more tightly than the
/// empty one), and the ninth, with `h2`'s eight cores booked, goes to `h1`.
#[tokio::test]
async fn with_the_flag_a_host_with_room_takes_a_second_lease_and_a_full_one_none() {
    let (_h, mut provider) = serving("placement_full", &["--placement", "js-sched"]).await;
    let hosts: Vec<Option<String>> = (1..=9)
        .map(|n| host_of(&provider.start(sharing_spec(n))).map(str::to_owned))
        .collect();
    let named = |host: &str| Some(host.to_owned());
    let mut expected = vec![named("h2"); 8];
    expected.push(named("h1"));
    assert_eq!(hosts, expected);
}

fn parsed(args: Vec<OsString>) -> Result<Config, String> {
    let cli = Cli::try_parse_from(args).map_err(|err| err.to_string())?;
    Config::from_cli(cli).map_err(|err| err.to_string())
}

/// Asserts the flag's parse: absent it is the registry; `registry` and `js-sched` are
/// read; any other value is a usage error; and without `--agent-pool-config` it is refused
/// naming the flag, as the other flags of agent registration are.
#[test]
fn the_flag_is_read_and_needs_an_agent_pool() {
    let scratch = Scratch::new("placement_flag");
    let none = config(args(&scratch, &[])).unwrap();
    assert_eq!(none.placement(), Placement::Registry);
    let sched = config(args(&scratch, &["--placement", "js-sched"])).unwrap();
    assert_eq!(sched.placement(), Placement::JsSched);
    let registry = config(args(&scratch, &["--placement", "registry"])).unwrap();
    assert_eq!(registry.placement(), Placement::Registry);
    let said = parsed(args(&scratch, &["--placement", "elsewhere"]))
        .err()
        .unwrap();
    assert!(
        said.contains("invalid value 'elsewhere' for '--placement"),
        "{said}"
    );
    let mut bare = base_args(&scratch);
    bare.extend(["--placement", "js-sched"].map(OsString::from));
    let said = parsed(bare).err().unwrap();
    assert_eq!(said, "--placement: requires --agent-pool-config");
    let flagless = config(base_args(&scratch)).unwrap();
    assert_eq!(flagless.placement(), Placement::Registry);
}

/// An elastic pool whose hosts register agents.
fn pool(scratch: &Scratch) -> PoolText {
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    PoolText::elastic(scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("exec", &quote("none"))
        .set("join_token_file", &quoted(&joins))
}

/// Asserts the file's key: `server.placement` is the registry when left out or named, and a
/// bad value is refused naming the key (the values of the key are tested in `config_one_file`).
#[test]
fn the_key_is_read_from_the_file() {
    for (name, extra) in [
        ("placement_key_off", ""),
        ("placement_key_reg", "placement = \"registry\""),
    ] {
        let scratch = Scratch::new(name);
        let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
        let table = server(&scratch, &format!("{listeners}\n{extra}"));
        let text = file(&table, &[&pool(&scratch)]);
        let config = load(&scratch, &text).unwrap();
        assert_eq!(config.placement(), Placement::Registry);
    }
}

/// Asserts that `js-sched` is refused at start for a pool whose shape the scheduler cannot
/// take as a node (no `arch`), by the flag and by the key, naming the flag or key and the
/// missing `arch`; and that the same pool is accepted by the registry. Without the refusal
/// every start would fall back to the first host and the flag would do nothing.
#[test]
fn js_sched_is_refused_for_a_pool_with_no_arch() {
    let scratch = Scratch::new("placement_no_arch");
    let mut flags = args(&scratch, &["--placement", "js-sched"]);
    let at = flags
        .iter()
        .position(|arg| arg == "--agent-pool-config")
        .unwrap();
    let bare = POOL.replace("arch = \"x86_64\"", "isa = \"x86-64-v3\"");
    flags[at + 1] = scratch
        .write("bare-pool.toml", &bare, 0o644)
        .into_os_string();
    let said = parsed(flags.clone()).err().unwrap();
    assert!(said.starts_with("--placement: "), "{said}");
    assert!(said.contains("arch"), "{said}");
    let at = flags.iter().position(|arg| arg == "--placement").unwrap();
    flags[at + 1] = "registry".into();
    assert!(parsed(flags).is_ok());

    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let table = server(&scratch, &format!("{listeners}\nplacement = \"js-sched\""));
    let text = file(&table, &[&pool(&scratch)]);
    let said = refusal(&scratch, &text);
    assert!(said.starts_with("server.placement: "), "{said}");
    assert!(said.contains("arch"), "{said}");
}
