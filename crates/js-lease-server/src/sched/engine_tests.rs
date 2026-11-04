//! The one placement engine of `--placement js-sched`, in process: built once and kept
//! in step with the registry's books, fed the heartbeats' load readings, held to the CPU
//! slack setting and to the pool's memory percent; and the default placement, which none
//! of it reaches. The agent side is assembled from the flags as the server assembles it,
//! on a manual clock; no listener is bound.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_agent_wire::{Heartbeat, Hello, LoadDrops, NodeLoad as Wire, Phase};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{Credential, HostState, RegisteredProvider};
use js_sched::LoadConfig;
use js_testkit::dir::TestDir;
use js_types::{LeaseId, Request, Resources};
use serde_json::json;

use super::place::{Fallbacks, SchedPlacer};
use super::wiring_tests::{JOIN, config_of, host_of, scratch, sharing, sharing_spec, write};
use crate::adapt::SharedClock;
use crate::agents::{Agents, assemble};
use crate::config::{Cli, Config, CpuSlack};

/// The time the clock starts at.
const T0: u64 = 1_000_000;

/// One GiB.
const GIB: u64 = 1 << 30;

/// An agent pool of up to `machines` hosts of `cpu_millis` and 16 GiB, with an `arch`,
/// and `extra` keys first.
fn pool(machines: usize, cpu_millis: u64, extra: &str) -> String {
    format!(
        r#"{extra}
name = "burst"
class = "burst-host"
max_machines = {machines}

[shape]
cpu_millis = {cpu_millis}
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#
    )
}

/// A hello of `host` claiming `cpu_millis` and `memory_bytes`.
fn hello_of(host: &str, cpu_millis: u64, memory_bytes: u64) -> Hello {
    let body = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": cpu_millis, "memory_bytes": memory_bytes, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    });
    serde_json::from_value(body).unwrap()
}

/// The agent side of a server, the provider of its registry serving the class `share`,
/// its clock and each registered host's session.
struct Rig {
    agents: Arc<Agents>,
    provider: RegisteredProvider,
    clock: ManualClock,
    sessions: Vec<(String, u64, String)>,
    _dir: TestDir,
}

/// [`Rig`] over the agent pool `pool` with `extra` flags, the hosts `hosts` registered at
/// [`T0`] with hellos claiming 16 cores and 32 GiB.
fn rig(name: &str, pool: &str, hosts: &[&str], extra: &[&str]) -> Rig {
    let dir = scratch(name);
    let config = config_of(dir.path(), pool, extra);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    let (_source, agents) =
        assemble(config.hosts, config.usage, &shared, &config.state_file).unwrap();
    let agents = agents.unwrap();
    let provider = agents.registry.provider().with_classes(vec![sharing()]);
    let mut rig = Rig {
        agents,
        provider,
        clock,
        sessions: Vec::new(),
        _dir: dir,
    };
    for host in hosts {
        rig.hello(&hello_of(host, 16_000, 32 * GIB));
    }
    rig
}

impl Rig {
    /// The placer the registry was given.
    fn placer(&self) -> &SchedPlacer {
        self.agents.placer.as_deref().unwrap()
    }

    fn hello(&mut self, hello: &Hello) {
        let credential = Credential::of_token(JOIN);
        let welcome = self.agents.registry.hello(hello, &credential).unwrap();
        let entry = (hello.host_id.clone(), welcome.epoch, welcome.session);
        self.sessions.push(entry);
    }

    /// A heartbeat of `host` with `load`, which the registry accepts.
    fn beat(&self, host: &str, load: Option<Wire>) {
        let (_, epoch, session) = self.sessions.iter().find(|(h, ..)| h == host).unwrap();
        let beat = Heartbeat {
            host_id: host.to_owned(),
            epoch: *epoch,
            session: session.clone(),
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load,
            disk_gib: None,
        };
        let credential = Credential::of_token(JOIN);
        self.agents.registry.heartbeat(&beat, &credential).unwrap();
    }

    /// Moves the clock on by `ms`.
    fn advance(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    /// Starts `spec`; the host it is booked on.
    fn start(&mut self, spec: StartSpec) -> Option<String> {
        host_of(&self.provider.start(spec)).map(str::to_owned)
    }

    /// The counters of the placer, as one value: fallbacks, resyncs and engines built.
    fn counted(&self) -> (Fallbacks, u64, u64) {
        let placer = self.placer();
        (placer.fallbacks(), placer.resynced(), placer.built())
    }
}

/// No fallback.
const NONE: Fallbacks = Fallbacks {
    unmappable: 0,
    declined: 0,
};

/// A lease of class `share` numbered `sequence`, booking `cpu_millis` and `memory_bytes`.
fn share_of(sequence: u64, cpu_millis: u64, memory_bytes: u64) -> StartSpec {
    let spec = sharing_spec(sequence);
    let request = Request {
        resources: Resources::new(cpu_millis, memory_bytes, 0, None).unwrap(),
        ..spec.request().clone()
    };
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

/// The hosts `h01` to `h<count>`.
fn names(count: usize) -> Vec<String> {
    (1..=count).map(|n| format!("h{n:02}")).collect()
}

/// Asserts that 200 starts and 200 ends over 20 hosts build the engine once: the placer
/// counts one construction, no resync and no fallback, and every start is placed. It
/// catches an engine built per start (the design's row 21) and any path that rebuilds it
/// on a normal start or end.
#[test]
fn the_engine_is_built_once_across_many_starts() {
    let hosts = names(20);
    let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let mut r = rig(
        "built_once",
        &pool(20, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    assert_eq!(r.counted(), (NONE, 0, 1));
    for sequence in 1..=200 {
        assert!(
            r.start(sharing_spec(sequence)).is_some(),
            "start {sequence}"
        );
        if sequence > 30 {
            r.provider.stop(&LeaseId::new(1, sequence - 30));
        }
        if sequence % 10 == 0 {
            r.advance(5_000);
            hosts.iter().for_each(|host| r.beat(host, None));
        }
    }
    for sequence in 171..=200 {
        r.provider.stop(&LeaseId::new(1, sequence));
    }
    assert!(r.provider.booked().is_empty());
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts that a lease the registry books without the engine (put back as a restart
/// does) makes the next start that meets its host rebuild the engine once, counted as
/// `resynced`, and that the next 50 starts and ends add no rebuild. It catches a
/// disagreement left in place (the engine placing past a booking it does not know) and a
/// rebuild that does not take the registry's books, which would rebuild again at every
/// start.
#[test]
fn a_disagreement_rebuilds_once_and_is_counted() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "resync_once",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    assert!(r.start(sharing_spec(1)).is_some());
    assert_eq!(r.counted(), (NONE, 0, 1));
    let behind = js_types::HostId::new("h2").unwrap();
    let held = Some(Resources::new(1000, 1 << 20, 0, None).unwrap());
    let booking = js_lease::Booking {
        lease: LeaseId::new(9, 1),
        host: behind.clone(),
        state: js_lease::LeaseState::Held,
        shares: held,
        started_at: Timestamp::from_millis(T0),
        ends_at: Timestamp::from_millis(T0 + 600_000),
    };
    r.provider.seat_booked(&booking, None).unwrap();
    assert!(r.start(sharing_spec(2)).is_some());
    assert_eq!(r.counted(), (NONE, 1, 2));
    for sequence in 3..=52 {
        assert!(
            r.start(sharing_spec(sequence)).is_some(),
            "start {sequence}"
        );
        r.provider.stop(&LeaseId::new(1, sequence));
    }
    assert_eq!(r.counted(), (NONE, 1, 2));
}

/// A load reading numbered `seq`, `seq` windows of 5 s into the agent's clock, with the
/// host's leases busy at `cores` thousandths of a core on average since the start, a
/// working set of `working_set` bytes, and `covered` the leases it covers.
fn reading(seq: u64, cores: u64, working_set: u64, covered: &[&str]) -> Wire {
    Wire {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 5 * cores * 1_000,
        working_set_bytes: working_set,
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: covered.iter().map(|lease| (*lease).to_owned()).collect(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        dropped: LoadDrops::default(),
    }
}

/// Over two equal hosts with room, the host lease 1 lands on, and the host a second lease
/// lands on after `beats` (each host's two readings, or none) are fed.
fn second_after(name: &str, beats: impl Fn(&str) -> [Option<Wire>; 2]) -> (String, String) {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        name,
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let first = r.start(sharing_spec(1)).unwrap();
    for at in 0..2 {
        r.advance(5_000);
        for host in hosts {
            let reading = beats(&first)[at].clone().filter(|_| host == first);
            let other = beats("")[at].clone().filter(|_| host != first);
            r.beat(host, reading.or(other));
        }
    }
    let second = r.start(sharing_spec(2)).unwrap();
    assert_eq!(r.counted(), (NONE, 0, 1), "{name}");
    (first, second)
}

/// Asserts that the heartbeats' load readings choose among the hosts that fit: the host
/// that carries lease 1 reports all 8 of its cores busy and a 15 GiB working set, the
/// other an idle one, and lease 2 goes to the idle host; with the same hosts and no load
/// in the heartbeats it goes where booking alone sends it, beside lease 1 (the host the
/// lease fits most tightly). It catches readings not fed to the engine, and readings fed
/// to the wrong host (which would send it to the busy one).
#[test]
fn load_readings_choose_among_hosts_that_fit() {
    let busy = |first: &str| {
        if first.is_empty() {
            [Some(reading(1, 0, GIB, &[])), Some(reading(2, 0, GIB, &[]))]
        } else {
            let cover = ["1-1"];
            let ws = 15 * GIB;
            [
                Some(reading(1, 8_000, ws, &cover)),
                Some(reading(2, 8_000, ws, &cover)),
            ]
        }
    };
    let (first, second) = second_after("load_chooses", busy);
    assert_ne!(second, first);
    let (first, second) = second_after("load_absent", |_| [None, None]);
    assert_eq!(second, first);
}

/// Asserts that a heartbeat whose load the vet drops (an active file figure past the most
/// the wire takes) still counts as a beat and feeds no reading: the registry keeps every
/// host live with no reading, and lease 2 goes where booking alone sends it. The dropped
/// readings say all 8 cores of each host are busy with a working set the engine would
/// take, so had they been fed, the engine would place lease 2 nowhere. It catches a beat
/// whose bad load reaches the engine, and a bad load that loses the beat.
#[test]
fn a_heartbeat_with_a_bad_load_still_counts_as_a_beat() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "bad_load",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let first = r.start(sharing_spec(1)).unwrap();
    for seq in 1..=4 {
        r.advance(5_000);
        for host in hosts {
            let mut bad = reading(seq, 8_000, GIB, &["1-1"]);
            bad.active_file_bytes = MAX_MEMORY_BYTES + 1;
            r.beat(host, Some(bad));
        }
    }
    let listed = r.agents.registry.hosts();
    assert!(listed.iter().all(|host| host.state == HostState::Live));
    assert!(listed.iter().all(|host| host.load.is_none()));
    assert_eq!(r.start(sharing_spec(2)).unwrap(), first);
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts the CPU slack through the server's configuration: with no `--cpu-slack` (the
/// engine's own default, read from js-sched and not restated) a 10-core host whose leases
/// book 10 cores refuses a start of 1.2 cores and takes one of 1 core, and a start that
/// would pass the host's memory is refused whatever the slack; with `--cpu-slack 1.0` the
/// 1-core start is refused; with `--cpu-slack 1.5` five more cores are taken and a sixth
/// is not, which the engine's own rule decides as much as the registry's book. It catches
/// the setting dropped on either side, a default restated that drifts, and slack applied
/// to memory.
#[test]
fn slack_1_1_lets_cpu_book_ten_percent_over() {
    let sched = ["--placement", "js-sched"];
    let full = |name: &str, extra: &[&str]| {
        let mut r = rig(name, &pool(1, 10_000, ""), &["h1"], extra);
        for sequence in 1..=10 {
            assert_eq!(
                r.start(share_of(sequence, 1_000, GIB)).as_deref(),
                Some("h1")
            );
        }
        r
    };
    let mut r = full("slack_default", &sched);
    assert_eq!(r.start(share_of(11, 1_200, 1)), None);
    assert_eq!(r.start(share_of(12, 1, 7 * GIB)), None);
    assert_eq!(r.start(share_of(13, 1_000, 1)).as_deref(), Some("h1"));
    assert_eq!(r.start(share_of(14, 1, 1)), None);
    let default = LoadConfig::default().s_cpu_permille;
    assert_eq!(r.agents.registry.provider().capacity()[0].leases.len(), 11);
    assert_eq!(CpuSlack::default().permille(), default);
    let mut extra = sched.to_vec();
    extra.extend(["--cpu-slack", "1.0"]);
    let mut r = full("slack_one", &extra);
    assert_eq!(r.start(share_of(11, 1_000, 1)), None);
    let refused = r.provider.start(share_of(12, 1_000, 1));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    let mut extra = sched.to_vec();
    extra.extend(["--cpu-slack", "1.5"]);
    let mut r = full("slack_one_half", &extra);
    for sequence in 11..=15 {
        assert_eq!(r.start(share_of(sequence, 1_000, 1)).as_deref(), Some("h1"));
    }
    assert_eq!(r.start(share_of(16, 1_000, 1)), None);
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// What 90 percent of 16 GiB books, rounded down: 15461882265.6 bytes.
const NINETY: u64 = 16 * GIB / 100 * 90 + 16 * GIB % 100 * 90 / 100;

/// Asserts that under `--placement js-sched` the pool's `memory_schedulable_percent` is
/// the engine's rule: at 90 a host of 16 GiB takes starts totalling 90% of it, and one
/// byte more is placed nowhere (the decline stands and is counted), while the registry
/// books the host's whole 16 GiB and still reports the percent. It catches the registry still cutting the shape (a
/// double cut to 81%, which refuses the third start) and the engine ignoring the key
/// (which places the fourth).
#[test]
fn memory_percent_cuts_bookable_memory_in_the_engine() {
    let percent = "memory_schedulable_percent = 90";
    let sched = ["--placement", "js-sched"];
    let mut r = rig("percent_engine", &pool(2, 8000, percent), &["h1"], &sched);
    assert_eq!(NINETY, 15_461_882_265);
    let third = NINETY - 10 * GIB;
    for (sequence, bytes) in [(1, 5 * GIB), (2, 5 * GIB), (3, third)] {
        assert_eq!(
            r.start(share_of(sequence, 1_000, bytes)).as_deref(),
            Some("h1")
        );
    }
    assert_eq!(r.start(share_of(4, 1_000, 1)), None);
    let declined = Fallbacks {
        unmappable: 0,
        declined: 1,
    };
    assert_eq!(r.counted(), (declined, 0, 1));
    let listed = r.agents.registry.provider().capacity();
    assert_eq!(listed[0].shape.resources().memory_bytes(), 16 * GIB);
    let hosts = r.agents.registry.hosts();
    assert_eq!(hosts[0].memory.percent().get(), 90);
    assert_eq!(hosts[0].memory.bookable_bytes(), NINETY);
}

/// Asserts that a host whose hello claims far more memory than the pool lists (512 GiB)
/// is still held to the pool's percent: a request one byte above 90% of the pool's 16 GiB
/// fits no host (`no-fit`, not a wait), and the engine books what 90% allows on it and not
/// a byte more. It catches a host booked by its claim, and a request above the share left
/// to wait for ever.
#[test]
fn an_agent_claim_never_raises_the_percent() {
    let percent = "memory_schedulable_percent = 90";
    let sched = ["--placement", "js-sched"];
    let mut r = rig("percent_claim", &pool(2, 8000, percent), &[], &sched);
    r.hello(&hello_of("big", 64_000, 512 * GIB));
    let above = r.provider.start(share_of(9, 1_000, NINETY + 1));
    assert_eq!(above, StartAck::Refused(RefusalCode::NoFit));
    assert_eq!(r.start(share_of(1, 1_000, NINETY)).as_deref(), Some("big"));
    assert_eq!(r.start(share_of(2, 1_000, 1)), None);
    assert_eq!(r.placer().fallbacks().declined, 1);
}

/// Asserts that a `memory_schedulable_percent` outside 1 to 100, or not a number, is
/// refused at start under `--placement js-sched` with the pool's text, so no such value
/// reaches the engine. It catches a value let through to the engine unchecked.
#[test]
fn memory_percent_values_outside_1_to_100_are_refused() {
    for value in ["0", "101", "-1", "\"ninety\""] {
        let dir = scratch("percent_refused");
        let text = pool(2, 8000, &format!("memory_schedulable_percent = {value}"));
        let flags = [
            ("--listen", "127.0.0.1:0".into()),
            (
                "--state-file",
                dir.path().join("leases.toml").into_os_string(),
            ),
            ("--boot", "initialise".into()),
            (
                "--token-file",
                write(dir.path(), "token", &"t".repeat(40), 0o600),
            ),
            (
                "--agent-pool-config",
                write(dir.path(), "agent-pool.toml", &text, 0o644),
            ),
            ("--placement", "js-sched".into()),
        ];
        let mut args: Vec<std::ffi::OsString> = vec!["js-lease-server".into()];
        for (flag, value) in flags {
            args.push(flag.into());
            args.push(value);
        }
        let refused = Config::from_cli(Cli::try_parse_from(args).unwrap())
            .err()
            .unwrap();
        let said = refused.to_string();
        assert!(
            said.contains("memory_schedulable_percent"),
            "{value}: {said}"
        );
        if ["0", "101"].contains(&value) {
            let text = "memory_schedulable_percent must be from 1 to 100";
            assert!(said.contains(text), "{value}: {said}");
        }
    }
}

/// Asserts that the default placement gained nothing: with no `--placement` the registry
/// is given no placer, books whole hosts first free by id through a fixed script of
/// starts and ends (the hosts chosen are the ones a registry with no placer always
/// chose), and still cuts the shape it books to the pool's memory percent. It catches the
/// registry path changed by this placement's settings, such as the percent no longer
/// applied when the placer is absent.
#[test]
fn registry_placement_goldens_unchanged() {
    let percent = "memory_schedulable_percent = 90";
    let hosts = ["h1", "h2", "h3"];
    let r = rig("registry_script", &pool(3, 8000, percent), &hosts, &[]);
    assert!(r.agents.placer.is_none());
    let mut provider = r.agents.registry.provider();
    let class = provider.classes()[0].clone();
    let spec = |sequence: u64| {
        let mut request = sharing_spec(sequence).request().clone();
        request.class = class.id().clone();
        StartSpec::new(
            LeaseId::new(1, sequence),
            request,
            std::slice::from_ref(&class),
        )
        .unwrap()
    };
    let mut chosen = Vec::new();
    let script: [(u64, Option<u64>); 8] = [
        (1, None),
        (2, None),
        (3, None),
        (4, None),
        (5, Some(2)),
        (6, Some(1)),
        (7, Some(3)),
        (8, None),
    ];
    for (sequence, stop) in script {
        if let Some(stop) = stop {
            provider.stop(&LeaseId::new(1, stop));
        }
        let ack = provider.start(spec(sequence));
        chosen.push(host_of(&ack).unwrap_or("-").to_owned());
    }
    let expected = ["h1", "h2", "h3", "-", "h2", "h1", "h3", "-"];
    assert_eq!(chosen, expected);
    let listed = provider.capacity();
    assert_eq!(listed[0].shape.resources().memory_bytes(), NINETY);
    assert_eq!(listed[0].memory.percent().get(), 90);
    let shapes = r.agents.registry.hosts();
    let booked = |host: &js_registry::HostView| host.shape.resources().memory_bytes();
    assert!(shapes.iter().all(|host| booked(host) == NINETY));
}
