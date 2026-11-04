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
use js_lease::{Booking, ManualClock, Timestamp};
use js_provider::{Pool, RefusalCode, StartAck, StartSpec};
use js_registry::{Credential, RegisteredProvider};
use js_testkit::dir::TestDir;
use js_types::{HostId, LeaseId, ProviderId, Request, ResourceClass, Resources};
use serde_json::{Value, json};

use crate::adapt::SharedClock;
use crate::agents::{Agents, assemble};
use crate::capacity::{CapacitySource, Seated, Unseated};
use crate::config::{Cli, Config};
use crate::hosts::HostList;

/// The join token of the pool.
pub(super) const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

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
pub(super) fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/placement_wiring");
    TestDir::under(root, name)
}

pub(super) fn write(dir: &Path, name: &str, text: &str, mode: u32) -> OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.into_os_string()
}

/// The two fake hosts of the fake pool's example configuration, as the simulated source a
/// server serves beside the agent pool: the bookings of a lease file are seated in a fresh
/// pool at every start, and one the pool refuses comes back unseated.
struct FakeHosts;

impl CapacitySource for FakeHosts {
    fn provider(&self, bookings: &[Booking]) -> Result<Seated, crate::error::StartupError> {
        let id = ProviderId::new("fake-hosts").unwrap();
        let mut pool = js_pool_fake::FakeHostPool::from_toml(id, js_pool_fake::EXAMPLE_CONFIG)
            .map_err(crate::error::StartupError::because("--fake-hosts"))?;
        let unseated = bookings
            .iter()
            .filter_map(|booking| {
                let refused = pool.seat(&booking.host, booking.lease).err()?;
                Some(Unseated {
                    booking: booking.clone(),
                    reason: refused.to_string(),
                })
            })
            .collect();
        Ok(Seated {
            provider: Box::new(pool),
            unseated,
        })
    }
}

/// The configuration of a server with agent registration over the agent pool `pool` and
/// `extra` flags.
pub(super) fn config_of(dir: &Path, pool: &str, extra: &[&str]) -> Config {
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("leases.toml").into_os_string()),
        ("--boot", "initialise".into()),
        ("--token-file", write(dir, "token", &"t".repeat(40), 0o600)),
        (
            "--agent-token-file",
            write(dir, "agent-tokens", &format!("{JOIN}\n"), 0o600),
        ),
        (
            "--agent-pool-config",
            write(dir, "agent-pool.toml", pool, 0o644),
        ),
    ];
    let mut args: Vec<OsString> = vec!["js-lease-server".into()];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args.extend(extra.iter().map(OsString::from));
    Config::simulated(Cli::try_parse_from(args).unwrap(), Box::new(FakeHosts)).unwrap()
}

pub(super) fn hello(host: &str) -> Hello {
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
    serving_of(name, POOL, extra)
}

/// [`serving`] over the agent pool `pool`.
fn serving_of(name: &str, pool: &str, extra: &[&str]) -> Served {
    let dir = scratch(name);
    let config = config_of(dir.path(), pool, extra);
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

/// The class `share`, which books part of a host, as a pool's `share_class` names it.
pub(super) fn sharing() -> ResourceClass {
    crate::agent_pool::share_class("share").unwrap()
}

/// A lease of one core of class `share`, numbered `sequence`.
pub(super) fn sharing_spec(sequence: u64) -> StartSpec {
    let request = Request {
        class: sharing().id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

pub(super) fn host_of(ack: &StartAck) -> Option<&str> {
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

/// A lease of one core of class `share`, numbered `sequence`, a probe pinned to `host`.
fn probe_spec(sequence: u64, host: &str) -> StartSpec {
    probe_of(sequence, host, |_| {})
}

/// [`probe_spec`] with `change` made to its request.
fn probe_of(sequence: u64, host: &str, change: impl FnOnce(&mut Request)) -> StartSpec {
    let mut request = sharing_spec(sequence).request().clone();
    request.probe_host = Some(HostId::new(host).unwrap());
    change(&mut request);
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

fn cordon(served: &Served, host: &str) {
    let host = HostId::new(host).unwrap();
    served
        .agents
        .registry
        .cordon(&host, Duration::from_secs(600))
        .unwrap();
}

/// Asserts that with `--placement js-sched` a probe lands on its cordoned host while the
/// ordinary leases of its shape started beside it go to the open host, never the cordoned
/// one; the cordon is the same after. It catches the cordon read for a probe (it would
/// never run), the probe placed elsewhere, and the cordon opened to ordinary work.
#[test]
fn with_the_flag_a_probe_lands_on_its_cordoned_host_and_ordinary_leases_do_not() {
    let mut served = serving("probe_lands", &["--placement", "js-sched"]);
    cordon(&served, "h2");
    let before = served.agents.registry.hosts();
    let probed = served.provider.start(probe_spec(1, "h2"));
    assert_eq!(host_of(&probed), Some("h2"));
    let ordinary: Vec<Option<String>> = (2..=4)
        .map(|n| host_of(&served.provider.start(sharing_spec(n))).map(str::to_owned))
        .collect();
    assert_eq!(ordinary, vec![Some("h1".to_owned()); 3]);
    let cordon_of = |views: &[js_registry::HostView]| {
        let h2 = views.iter().find(|v| v.id.as_str() == "h2").unwrap();
        h2.cordon_until
    };
    let after = served.agents.registry.hosts();
    assert!(cordon_of(&after).is_some());
    assert_eq!(cordon_of(&after), cordon_of(&before));
}

/// Asserts that with `--placement js-sched` a probe the rule refuses is answered its
/// code and books nothing: an unknown host, an open one, a host that is not serving, one
/// smaller than the probe, and one that does not satisfy its platform. It catches a
/// refusal answered `no-capacity` (a client would wait for ever), and a probe placed on
/// another host in its stead.
#[test]
fn with_the_flag_a_probe_the_rule_refuses_is_answered_by_name() {
    let pool = POOL.replace("max_machines = 2", "max_machines = 3");
    let mut served = serving_of("probe_refused", &pool, &["--placement", "js-sched"]);
    let refused = |served: &mut Served, spec| match served.provider.start(spec) {
        StartAck::Refused(code) => code,
        ack => panic!("expected a refusal, got {ack:?}"),
    };
    assert_eq!(
        refused(&mut served, probe_spec(1, "h9")),
        RefusalCode::ProbeHostUnknown
    );
    assert_eq!(
        refused(&mut served, probe_spec(2, "h1")),
        RefusalCode::ProbeHostNotCordoned
    );
    cordon(&served, "h2");
    let big = probe_of(3, "h2", |r| {
        r.resources = Resources::new(16_000, 1 << 20, 0, None).unwrap();
    });
    assert_eq!(refused(&mut served, big), RefusalCode::ProbeHostTooSmall);
    let mac = probe_of(4, "h2", |r| {
        r.capabilities.insert("os".to_owned(), "macos".to_owned());
    });
    assert_eq!(refused(&mut served, mac), RefusalCode::ProbeHostMismatch);
    let mut draining = hello("h3");
    draining.phase = js_agent_wire::Phase::Draining;
    let credential = Credential::of_token(JOIN);
    served
        .agents
        .registry
        .hello(&draining, &credential)
        .unwrap();
    cordon(&served, "h3");
    assert_eq!(
        refused(&mut served, probe_spec(5, "h3")),
        RefusalCode::ProbeHostNotServing
    );
    assert!(served.provider.booked().is_empty());
}

/// Asserts that with `--placement js-sched` a probe whose cordoned host is full is
/// `no-capacity`, not placed on the free host beside it, and placed on its host once a
/// lease there ends. It catches a probe that oversubscribes its host, spills to another,
/// or never gets its host back.
#[test]
fn with_the_flag_a_probe_on_its_full_host_waits_for_room_there() {
    let mut served = serving("probe_full", &["--placement", "js-sched"]);
    // Eight one-core leases fill `h2`'s eight cores (see the test above).
    for n in 1..=8 {
        assert_eq!(host_of(&served.provider.start(sharing_spec(n))), Some("h2"));
    }
    cordon(&served, "h2");
    let full = served.provider.start(probe_spec(9, "h2"));
    assert_eq!(full, StartAck::Refused(RefusalCode::NoCapacity));
    let h1 = HostId::new("h1").unwrap();
    let on = |served: &Served, host: &HostId| {
        let listed = served.provider.capacity();
        listed.into_iter().find(|c| c.host == *host).unwrap().leases
    };
    assert!(on(&served, &h1).is_empty());
    served.provider.stop(&LeaseId::new(1, 1));
    let placed = served.provider.start(probe_spec(10, "h2"));
    assert_eq!(host_of(&placed), Some("h2"));
    assert!(on(&served, &h1).is_empty());
}

/// Asserts that a probe is judged against the memory the pool books, after
/// `memory_schedulable_percent`, not the host's whole memory: with half of 16 GiB
/// schedulable a 12 GiB probe is too small for its host, and a 6 GiB one is placed. It
/// catches a probe that skips the percent and books memory the pool keeps back.
#[test]
fn with_the_flag_a_probe_is_held_to_the_memory_percent() {
    let pool = format!("memory_schedulable_percent = 50\n{POOL}");
    let mut served = serving_of("probe_percent", &pool, &["--placement", "js-sched"]);
    cordon(&served, "h1");
    let sized = |sequence, gib: u64| {
        probe_of(sequence, "h1", |r| {
            r.resources = Resources::new(1000, gib << 30, 0, None).unwrap();
        })
    };
    let refused = served.provider.start(sized(1, 12));
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeHostTooSmall));
    assert_eq!(host_of(&served.provider.start(sized(2, 6))), Some("h1"));
}

/// Asserts that with no `--placement` flag a probe is refused `probe-unsupported` for a
/// class the registry places alone, though its host is cordoned and free: the registry has
/// no probe rule. It catches a probe placed by the registry's first-host rule.
#[test]
fn by_default_a_probe_is_refused_as_unsupported() {
    let served = serving("probe_default", &[]);
    cordon(&served, "h1");
    let mut provider = served.agents.registry.provider();
    let class = provider.classes()[0].clone();
    let mut request = probe_spec(1, "h1").request().clone();
    request.class = class.id().clone();
    let spec = StartSpec::new(LeaseId::new(1, 1), request, &[class]).unwrap();
    let ack = provider.start(spec);
    assert_eq!(ack, StartAck::Refused(RefusalCode::ProbeUnsupported));
    assert!(provider.booked().is_empty());
}
