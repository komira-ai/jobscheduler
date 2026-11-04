//! Backfill on a host a whole-node request has set aside, through the server in process
//! (`--placement js-sched`): the configuration read from a pool file with a `share_class`,
//! the agent side and the provider assembled from it as the server assembles them, hosts
//! registered and beating on a manual clock, and a consumer polling its whole-node request.
//! No listener is bound and no connection is made.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_lease::{Clock, ManualClock, Timestamp};
use js_provider::{Pool, RefusalCode, StartAck, StartSpec};
use js_registry::{Credential, Placer};
use js_testkit::dir::TestDir;
use js_types::{HostId, LeaseId, ResourceClass, Resources};

use super::wiring_tests::{JOIN, config_of, hello, host_of, scratch, sharing, sharing_spec, write};
use crate::adapt::SharedClock;
use crate::agents::{Agents, assemble};
use crate::config::{Cli, Config};

/// The agent side of a server, the provider the server leases through (its capacity
/// source's, as configured), the clock it runs on, each registered host with its session,
/// and the whole-node requests a consumer polls (lease sequence and request name).
struct Live {
    agents: Arc<Agents>,
    provider: Box<dyn Pool + Send>,
    /// What the server builds its lease table from: the capacity source, its settings,
    /// its file and how it boots.
    source: Box<dyn crate::CapacitySource>,
    table_of: (js_lease::Settings, std::path::PathBuf, js_lease::Boot),
    clock: ManualClock,
    sessions: Vec<(String, u64, String)>,
    asks: Vec<(u64, String)>,
    _dir: TestDir,
}

/// The time the clock of [`Live`] starts at.
const T0: u64 = 1_000_000;

/// An agent pool of up to two machines whose class is `whole-node` and whose hosts are
/// shared by leases of the class `share`: the pool file a server that backfills runs.
const SHARED_POOL: &str = r#"
name = "burst"
class = "whole-node"
share_class = "share"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// [`Live`] over [`SHARED_POOL`] with `extra` flags and the hosts `hosts` registered at
/// [`T0`].
fn live(name: &str, hosts: &[&str], extra: &[&str]) -> Live {
    live_over(name, SHARED_POOL, hosts, extra)
}

/// [`Live`] over the agent pool `pool`. The provider is the one the server's capacity
/// source gives, with its classes as the pool file sets them.
fn live_over(name: &str, pool: &str, hosts: &[&str], extra: &[&str]) -> Live {
    let dir = scratch(name);
    let config = config_of(dir.path(), pool, extra);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    let (source, agents) =
        assemble(config.hosts, config.usage, &shared, &config.state_file).unwrap();
    let provider = source.provider(&[]).unwrap().provider;
    let table_of = (config.settings, config.state_file.clone(), config.boot);
    let agents = agents.unwrap();
    let credential = Credential::of_token(JOIN);
    let sessions = hosts
        .iter()
        .map(|host| {
            let welcome = agents.registry.hello(&hello(host), &credential).unwrap();
            ((*host).to_owned(), welcome.epoch, welcome.session)
        })
        .collect();
    Live {
        agents,
        provider,
        source,
        table_of,
        clock,
        sessions,
        asks: Vec::new(),
        _dir: dir,
    }
}

impl Live {
    /// The lease table the server runs over this agent side, on the same clock: it ends a
    /// lease at its wall time when it ticks.
    fn table(&self) -> js_lease::LeaseTable<crate::adapt::BoxedPool, SharedClock> {
        let provider = crate::adapt::BoxedPool(self.source.provider(&[]).unwrap().provider);
        let (settings, file, boot) = self.table_of.clone();
        let clock = SharedClock::new(self.clock.clone());
        js_lease::LeaseTable::open(provider, clock, settings, &file, boot).unwrap()
    }

    /// Moves the clock to `ms` (from [`T0`]) five seconds at a time, every host beating and
    /// every whole-node request not yet booked presented again at each step, as an agent
    /// and a polling consumer do, so no host is lost and no request dropped.
    fn at(&mut self, ms: u64) {
        let credential = Credential::of_token(JOIN);
        while self.clock.now().millis() < T0 + ms {
            let left = T0 + ms - self.clock.now().millis();
            self.clock.advance(Duration::from_millis(left.min(5_000)));
            for (host, epoch, session) in &self.sessions {
                let beat = js_agent_wire::Heartbeat {
                    host_id: host.clone(),
                    epoch: *epoch,
                    session: session.clone(),
                    phase: js_agent_wire::Phase::Serving,
                    held: Vec::new(),
                    results: Vec::new(),
                    taint: None,
                    exclusive_ready: None,
                    exclusive_refused: None,
                    load: None,
                    disk_gib: None,
                    jobs: Vec::new(),
                    retired_jobs: Vec::new(),
                };
                self.agents.registry.heartbeat(&beat, &credential).unwrap();
            }
            for (sequence, name) in self.asks.clone() {
                let lease = LeaseId::new(1, sequence);
                let booked = self
                    .provider
                    .capacity()
                    .iter()
                    .any(|c| c.leases.contains(&lease));
                if !booked {
                    self.whole(sequence, &name);
                }
            }
        }
    }

    /// Starts lease `sequence` of class `share` that may run `wall_s`: the host it is
    /// placed on, if any, else the answer.
    fn share(&mut self, sequence: u64, wall_s: u64) -> Result<String, StartAck> {
        let ack = self
            .provider
            .start(share_for(sequence, Duration::from_secs(wall_s)));
        host_of(&ack).map(str::to_owned).ok_or(ack)
    }

    /// Starts whole-node request `name` as lease `sequence`, which may run an hour: its
    /// mark is not given up (the registry's drain deadline) before then.
    fn whole(&mut self, sequence: u64, name: &str) -> StartAck {
        let class = ResourceClass::whole_node();
        let mut request = sharing_spec(sequence).request().clone();
        request.class = class.id().clone();
        request.max_wall = Duration::from_secs(3_600);
        let spec = StartSpec::new(LeaseId::new(1, sequence), request, &[class]).unwrap();
        let id = js_types::RequestId::new(name).unwrap();
        let ask = (sequence, name.to_owned());
        if !self.asks.contains(&ask) {
            self.asks.push(ask);
        }
        self.provider.start(spec.with_request_id(id))
    }

    /// The leases on `host`.
    fn on(&self, host: &str) -> Vec<LeaseId> {
        let listed = self.provider.capacity().into_iter();
        let found = listed.into_iter().find(|c| c.host.as_str() == host);
        found.map(|c| c.leases).unwrap_or_default()
    }

    /// The host request `name` holds and the end of its tail as the mark took it.
    fn mark(&self, name: &str) -> Option<(String, Option<u64>)> {
        let marks = self.agents.registry.reservations();
        let mark = marks.into_iter().find(|r| r.request.as_str() == name);
        mark.map(|r| (r.host.to_string(), r.tail_end))
    }
}

/// A lease of one core of class `share` that may run `wall`, numbered `sequence`.
fn share_for(sequence: u64, wall: Duration) -> StartSpec {
    let mut request = sharing_spec(sequence).request().clone();
    request.max_wall = wall;
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

/// [`live`] with `--placement js-sched` over `h1` alone: lease 1 of 600 s on `h1` from
/// [`T0`], and whole-node request `q` (lease 2) waiting with `h1` set aside, its tail ending
/// at `T0 + 600 s`; the clock at 10 s.
fn marked(name: &str) -> Live {
    let mut l = live(name, &["h1"], &["--placement", "js-sched"]);
    assert_eq!(l.share(1, 600).as_deref(), Ok("h1"));
    assert!(host_of(&l.whole(2, "q")).is_none());
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    l.at(10_000);
    l
}

/// Asserts, through the server, that a host a whole-node request has set aside takes a
/// lease that ends, with a minute to spare, by the end of its tail, and not one that ends
/// after: at 10 s a lease of 300 s (ending at 310 s) lands on `h1` beside the lease the
/// tail is made of, and counts as backfill; one of 600 s (610 s) has no host and waits;
/// the mark is unchanged. It catches the marked host closed to every other start (the
/// registry's rule without a placer), and the gate not applied to it.
#[test]
fn with_the_flag_a_lease_ending_inside_the_tail_backfills_a_marked_host() {
    let mut l = marked("backfill_fits");
    assert_eq!(l.share(3, 300).as_deref(), Ok("h1"));
    assert_eq!(l.agents.registry.tail_counters().backfilled, 1);
    let late = l.share(4, 600);
    assert_eq!(late, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1), LeaseId::new(1, 3)]);
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
}

/// Asserts, through the server, that backfill does not delay the whole-node request past
/// the tail it was set aside by: the lease of 300 s placed at 10 s ends at its wall
/// (310 s); at 599 s the request still has no host, and at 600 s, when the lease the tail
/// is made of ends, it is committed on `h1`, its host's only lease. It catches backfill
/// left running past the tail, and a request that takes the host before it is quiet.
#[test]
fn with_the_flag_the_whole_node_request_starts_on_its_host_when_the_tail_ends() {
    let mut l = marked("backfill_start");
    assert_eq!(l.share(3, 300).as_deref(), Ok("h1"));
    l.at(310_000);
    l.provider.stop(&LeaseId::new(1, 3));
    l.at(599_000);
    assert!(host_of(&l.whole(2, "q")).is_none());
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
    l.at(600_000);
    l.provider.stop(&LeaseId::new(1, 1));
    assert_eq!(l.whole(2, "q"), StartAck::Unknown);
    assert_eq!(l.on("h1"), [LeaseId::new(1, 2)]);
    assert_eq!(l.mark("q"), None);
}

/// Asserts, through the server, that a lease the tail of a marked host refuses goes to a
/// host with room that is not marked: `h1` carries a lease of 600 s and `h2` one of 900 s
/// (from 2 s), so `q` sets `h1` aside, and a lease of 600 s at 10 s, too long for `h1`,
/// lands on `h2`. It catches the gate refusing the start outright, or the placer's
/// fallback booking the marked host.
#[test]
fn with_the_flag_a_lease_too_long_for_the_tail_goes_to_an_unmarked_host() {
    let flags = ["--placement", "js-sched"];
    let mut l = live("backfill_elsewhere", &["h1", "h2"], &flags);
    let second = Duration::from_secs(1);
    // The scheduler packs a lease onto the busier host, so each host is given its lease
    // while the other is cordoned for a second.
    let cordon = |l: &Live, host: &str| {
        let host = HostId::new(host).unwrap();
        l.agents.registry.cordon(&host, second).unwrap();
    };
    cordon(&l, "h2");
    assert_eq!(l.share(1, 600).as_deref(), Ok("h1"));
    l.at(2_000);
    cordon(&l, "h1");
    assert_eq!(l.share(5, 900).as_deref(), Ok("h2"));
    l.at(4_000);
    assert!(host_of(&l.whole(2, "q")).is_none());
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    l.at(10_000);
    assert_eq!(l.share(3, 600).as_deref(), Ok("h2"));
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
}

/// Asserts, through the server, that an overdue lease is handled as the design states: at
/// 650 s the lease of 600 s on `h1` has not ended (nothing has stopped it yet), the mark
/// stands (the request may run an hour, so its drain is not given up), the tail it took
/// is past, and not even a lease of a second is placed there. It catches a past tail read
/// as a free host.
#[test]
fn with_the_flag_a_marked_host_whose_tail_is_past_takes_no_backfill() {
    let mut l = marked("backfill_overdue");
    l.at(650_000);
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    let refused = l.share(3, 1);
    assert_eq!(refused, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
}

/// Asserts, through the server, that a host whose lease has an unknown end takes no
/// backfill: with `--estimate-feed`, a lease whose max wall time saturates the clock is no
/// bound, and with no keyed run it has no estimate, so the mark's tail is unknown and a
/// lease of a second is not placed on `h1`. It catches an unknown tail read as far away.
#[test]
fn with_the_flag_a_host_with_a_lease_of_unknown_end_takes_no_backfill() {
    let flags = ["--placement", "js-sched", "--estimate-feed"];
    let mut l = live("backfill_unknown", &["h1"], &flags);
    let endless = l.provider.start(share_for(1, Duration::MAX));
    assert_eq!(host_of(&endless), Some("h1"));
    assert!(host_of(&l.whole(2, "q")).is_none());
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), None)));
    l.at(10_000);
    let refused = l.share(3, 1);
    assert_eq!(refused, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
}

/// Asserts that without the flag nothing shares a host, so no host a whole-node request
/// sets aside is ever backfilled: a pool file with a `share_class` is refused at start,
/// naming the flag, and a whole-node pool without one serves no class that shares a host,
/// so a lease of class `share` books nothing and counts no backfill. With the flag the same
/// file starts. It catches a share class served by the registry's own placement.
#[test]
fn without_the_flag_no_lease_shares_a_host() {
    let dir = scratch("backfill_off");
    let flags: Vec<OsString> = [
        "js-lease-server",
        "--listen",
        "127.0.0.1:0",
        "--boot",
        "initialise",
    ]
    .map(OsString::from)
    .into_iter()
    .chain([
        "--state-file".into(),
        dir.path().join("leases.toml").into_os_string(),
        "--token-file".into(),
        write(dir.path(), "token", &"t".repeat(40), 0o600),
        "--agent-token-file".into(),
        write(dir.path(), "agent-tokens", &format!("{JOIN}\n"), 0o600),
        "--agent-pool-config".into(),
        write(dir.path(), "agent-pool.toml", SHARED_POOL, 0o644),
    ])
    .collect();
    let refused = Config::from_cli(Cli::try_parse_from(flags.clone()).unwrap())
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        refused,
        "--placement: the pool's share_class needs js-sched: the registry places no lease that shares a host"
    );
    let mut sched = flags;
    sched.extend(["--placement", "js-sched"].map(OsString::from));
    assert!(Config::from_cli(Cli::try_parse_from(sched).unwrap()).is_ok());

    let whole_pool = SHARED_POOL.replace("share_class = \"share\"\n", "");
    let mut l = live_over("backfill_off_whole", &whole_pool, &["h1"], &[]);
    let classes: Vec<_> = l
        .provider
        .classes()
        .into_iter()
        .map(|c| c.id().to_string())
        .collect();
    assert_eq!(classes, ["gpu-host", "whole-node"]);
    let ack = l.provider.start(share_for(1, Duration::from_secs(1)));
    assert!(matches!(ack, StartAck::Refused(_)), "{ack:?}");
    assert!(l.on("h1").is_empty());
    assert_eq!(l.agents.registry.tail_counters().backfilled, 0);
}

/// Asserts that the provider the server leases through, built from a pool file with a
/// `share_class`, serves the pool's class and the share class beside the static pool's
/// own (`gpu-host`, from the fixture's `--pool-config`), and no other. It catches the
/// key read but not served.
#[test]
fn a_pool_files_share_class_is_served_beside_its_class() {
    let l = live("share_served", &["h1"], &["--placement", "js-sched"]);
    let classes: Vec<_> = l
        .provider
        .classes()
        .into_iter()
        .map(|c| c.id().to_string())
        .collect();
    assert_eq!(classes, ["gpu-host", "whole-node", "share"]);
}

/// Asserts, through the server, that a probe never backfills a marked host: `h1` is set
/// aside until 600 s and then cordoned, and a probe pinned to it that runs a second (which
/// the tail would fit) books nothing and counts no backfill. It catches a probe shown the
/// marked host as a backfill candidate.
#[test]
fn with_the_flag_a_probe_on_a_marked_host_is_not_backfilled() {
    let mut l = marked("backfill_probe");
    let h1 = HostId::new("h1").unwrap();
    l.agents
        .registry
        .cordon(&h1, Duration::from_secs(600))
        .unwrap();
    let mut request = share_for(3, Duration::from_secs(1)).request().clone();
    request.probe_host = Some(h1.clone());
    let probe = StartSpec::new(LeaseId::new(1, 3), request, &[sharing()]).unwrap();
    assert!(host_of(&l.provider.start(probe)).is_none());
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
    assert_eq!(l.agents.registry.tail_counters().backfilled, 0);
}

/// Asserts that the placer of `--placement js-sched` trusts a lease's wall time up to the
/// server's `max_lease_wall` and no further: with a ceiling of 300 s a lease of 300 s
/// backfills a host set aside until 600 s, and one of 301 s, which would end in time, is
/// not placed there. With no ceiling the 301 s lease is. It catches the ceiling not
/// passed to the placer, or applied with `<` for `<=`.
#[test]
fn the_placer_trusts_a_wall_time_up_to_max_lease_wall() {
    use js_provider::Candidate;
    let open = [Candidate {
        host: HostId::new("h1").unwrap(),
        shape: js_types::Shape::new(
            Resources::new(8000, 16 << 30, 0, None).unwrap(),
            BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]),
        ),
        leases: Vec::new(),
    }];
    let marked =
        js_registry::MarkedTails::from([(open[0].host.clone(), Some(Duration::from_secs(600)))]);
    let placer = |wall: Option<Duration>, placement: crate::config::Placement| {
        let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
        let slack = crate::config::CpuSlack::default();
        placement.placer(wall, slack, js_sched::MemoryPercent::FULL, &clock)
    };
    let js_sched = crate::config::Placement::JsSched;
    let five = placer(Some(Duration::from_secs(300)), js_sched).unwrap();
    let wall = |s: u64| share_for(1, Duration::from_secs(s));
    assert_eq!(five.pick(&wall(300), &open, &marked), Some(0));
    assert_eq!(five.pick(&wall(301), &open, &marked), None);
    let none = placer(None, js_sched).unwrap();
    assert_eq!(none.pick(&wall(301), &open, &marked), Some(0));
    assert!(placer(None, crate::config::Placement::Registry).is_none());
}

/// Asserts, through the server's lease table, that backfill ends at its enforced wall and
/// does not delay the whole-node request past the tail its host was set aside by. The table
/// grants lease A of 600 s at 0 on `h1`; whole-node request `q` sets `h1` aside with its
/// tail at 600 s; at 10 s the table grants lease B of 300 s, which backfills `h1`. Nothing
/// is stopped by hand: the table, ticked every five seconds, ends B at 310 s and A at
/// 600 s, at their walls, and `q` is committed on `h1` at 600 s, the end the mark took. It
/// catches backfill that outlives its bound, and a request that starts before or after
/// the tail it was given.
#[test]
fn with_the_flag_the_table_ends_backfill_at_its_wall_and_the_request_starts_at_the_tail() {
    let mut l = live("backfill_table", &["h1"], &["--placement", "js-sched"]);
    let mut table = l.table();
    let ttl = Some(Duration::from_secs(3_600));
    let wall = |s: u64| share_for(1, Duration::from_secs(s)).request().clone();
    let a = table.acquire(wall(600), ttl).unwrap();
    assert_eq!(a.host.as_str(), "h1");
    // The table numbers its leases from 1; the request's lease is far past them.
    assert!(host_of(&l.whole(1_000, "q")).is_none());
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    l.at(10_000);
    let b = table.acquire(wall(300), ttl).unwrap();
    assert_eq!(b.host.as_str(), "h1");
    assert_eq!(l.agents.registry.tail_counters().backfilled, 1);
    let mut ended_b = None;
    for ms in (15_000..=600_000).step_by(5_000) {
        l.at(ms);
        let report = table.tick();
        if report.freed.contains(&b.id) {
            ended_b = Some(ms);
        }
        assert!(
            !report.freed.contains(&a.id) || ms == 600_000,
            "A ended at {ms}"
        );
        if ms == 595_000 {
            assert_eq!(l.on("h1"), [a.id]);
            assert!(host_of(&l.whole(1_000, "q")).is_none());
        }
    }
    assert_eq!(ended_b, Some(310_000));
    assert!(l.on("h1").is_empty());
    assert_eq!(l.whole(1_000, "q"), StartAck::Unknown);
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1_000)]);
    assert_eq!(l.mark("q"), None);
}

/// The agent side and the provider of a server started from a configuration file with a
/// joined pool of `whole-node` hosts shared by the class `share`, `server.placement =
/// "js-sched"`, and `max_lease_wall` set to `ceiling` when given; host `h1` registered.
fn from_file(name: &str, ceiling: Option<&str>) -> Live {
    let dir = scratch(name);
    let token = write(dir.path(), "client-token", &"t".repeat(40), 0o600);
    let joins = write(dir.path(), "joins", &format!("{JOIN}\n"), 0o600);
    let quoted = |path: &OsString| format!("{:?}", path.to_str().unwrap());
    let ceiling = ceiling.map_or(String::new(), |c| format!("max_lease_wall = \"{c}\"\n"));
    let text = format!(
        "[server]\nstate_file = {}\nboot = \"initialise\"\nclient_token_file = {}\n\
         listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\nplacement = \"js-sched\"\n{ceiling}\n\
         [[pool]]\nkind = \"joined\"\nname = \"burst\"\nclass = \"whole-node\"\n\
         share_class = \"share\"\nmax_machines = 2\njoin_token_file = {}\n\
         shape = {{ cpu_millis = 8000, memory_bytes = 17179869184 }}\n\
         capabilities = {{ arch = \"x86_64\" }}\n",
        quoted(&dir.path().join("leases.toml").into_os_string()),
        quoted(&token),
        quoted(&joins),
    );
    let path = write(dir.path(), "js.toml", &text, 0o644);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    let kinds = crate::Kinds::standard();
    let config = Config::from_config_file(
        Path::new(&path),
        &kinds,
        &crate::OwnerOnlyFiles,
        shared.clone(),
    )
    .unwrap();
    let table_of = (config.settings, config.state_file.clone(), config.boot);
    let (source, agents) =
        assemble(config.hosts, config.usage, &shared, &config.state_file).unwrap();
    let provider = source.provider(&[]).unwrap().provider;
    let agents = agents.unwrap();
    let welcome = agents
        .registry
        .hello(&hello("h1"), &Credential::of_token(JOIN))
        .unwrap();
    Live {
        agents,
        provider,
        source,
        table_of,
        clock,
        sessions: vec![("h1".to_owned(), welcome.epoch, welcome.session)],
        asks: Vec::new(),
        _dir: dir,
    }
}

/// Asserts that `server.max_lease_wall` read from a configuration file reaches the placer
/// the server assembles: with a ceiling of 5 minutes a lease of 300 s backfills a host set
/// aside until 600 s, and one of 301 s, which would end in time, is not placed there;
/// with the key left out the 301 s lease is. It catches the ceiling dropped between the
/// file and the placer, or the default taken as a ceiling.
#[test]
fn a_files_max_lease_wall_is_the_ceiling_of_backfill() {
    for (name, ceiling, longer) in [
        ("ceiling_set", Some("5m"), None),
        ("ceiling_unset", None, Some("h1")),
    ] {
        let mut l = from_file(name, ceiling);
        assert_eq!(l.share(1, 600).as_deref(), Ok("h1"), "{name}");
        assert!(host_of(&l.whole(2, "q")).is_none(), "{name}");
        assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
        l.at(10_000);
        assert_eq!(l.share(3, 301).ok().as_deref(), longer, "{name}");
        assert_eq!(l.share(4, 300).as_deref(), Ok("h1"), "{name}");
    }
}

/// Asserts, through the server, the margin at the scheduler's clock: the mark's tail
/// (600 s on the registry's clock, which is not at zero) is handed to the scheduler as the
/// time left. At 10 s a lease of 530 s fits exactly (ending a minute before the tail) and
/// one of 530.001 s does not; at 100 s, with the tail the mark took unchanged, 440 s fits
/// and 440.001 s does not. It catches a tail passed as an absolute time, or taken again.
#[test]
fn with_the_flag_the_margin_holds_at_the_millisecond_through_the_server() {
    let mut l = marked("backfill_margin");
    let start = |l: &mut Live, n: u64, ms: u64| {
        let ack = l.provider.start(share_for(n, Duration::from_millis(ms)));
        host_of(&ack).map(str::to_owned)
    };
    assert_eq!(start(&mut l, 3, 530_001), None);
    assert_eq!(start(&mut l, 4, 530_000).as_deref(), Some("h1"));
    l.at(100_000);
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    assert_eq!(start(&mut l, 5, 440_001), None);
    assert_eq!(start(&mut l, 6, 440_000).as_deref(), Some("h1"));
}

/// Asserts, through the server, that a backfilled lease started again is the same booking
/// (one lease, one backfill counted, the mark's tail as it was), and that backfill stops
/// at the host's room: `h1` (8 cores) holds the 1-core lease the tail is made of, so
/// seven 1-core leases of 300 s backfill it and the eighth, which fits the tail, has no
/// room. It catches a retried start booked or counted twice, and a gate that overbooks.
#[test]
fn with_the_flag_a_repeated_backfill_is_one_booking_and_backfill_stops_at_room() {
    let mut l = marked("backfill_repeat");
    assert_eq!(l.share(3, 300).as_deref(), Ok("h1"));
    assert_eq!(l.share(3, 300).as_deref(), Ok("h1"));
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1), LeaseId::new(1, 3)]);
    assert_eq!(l.agents.registry.tail_counters().backfilled, 1);
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    for n in 4..=9 {
        assert_eq!(l.share(n, 300).as_deref(), Ok("h1"), "lease {n}");
    }
    let full = l.share(10, 300);
    assert_eq!(full, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.on("h1").len(), 8);
    assert_eq!(l.agents.registry.tail_counters().backfilled, 7);
}

/// Asserts, through the server, that a quiet host assigned to a waiting whole-node request
/// takes nothing before the request comes back for it: `h1` carries a lease of 600 s and
/// `h2` one of 900 s, so `q` sets `h1` aside; when `h2`'s lease ends, `h2` is quiet and is
/// assigned to `q` (its tail unknown: it drains nothing). A lease of a second then goes to
/// `h1`, no longer marked, and another whole-node request gets no host; `q`'s next start
/// is committed on `h2`. It catches an assigned host read as free or as backfillable.
#[test]
fn with_the_flag_a_quiet_host_assigned_to_a_request_takes_nothing_else() {
    let flags = ["--placement", "js-sched"];
    let mut l = live("backfill_assigned", &["h1", "h2"], &flags);
    let second = Duration::from_secs(1);
    let cordon = |l: &Live, host: &str| {
        let host = HostId::new(host).unwrap();
        l.agents.registry.cordon(&host, second).unwrap();
    };
    cordon(&l, "h2");
    assert_eq!(l.share(1, 600).as_deref(), Ok("h1"));
    l.at(2_000);
    cordon(&l, "h1");
    assert_eq!(l.share(5, 900).as_deref(), Ok("h2"));
    l.at(4_000);
    assert!(host_of(&l.whole(2, "q")).is_none());
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
    l.provider.stop(&LeaseId::new(1, 5));
    assert_eq!(l.share(3, 1).as_deref(), Ok("h1"));
    assert_eq!(l.mark("q"), Some(("h2".to_owned(), None)));
    assert!(l.on("h2").is_empty());
    let other = l.whole(7, "r");
    assert!(host_of(&other).is_none(), "{other:?}");
    assert!(l.on("h2").is_empty());
    assert_eq!(l.whole(2, "q"), StartAck::Unknown);
    assert_eq!(l.on("h2"), [LeaseId::new(1, 2)]);
}

/// Asserts, through the server, that a marked host whose tail a lease would fit takes it
/// only while the host is bookable: once `h1` is cordoned, a lease of a second (which fits
/// the tail) has no host; and on a second server, once `h1` has been silent past its
/// grace and is lost, the same lease has no host either. It catches the gate admitting
/// work to a host that placement may not use.
#[test]
fn with_the_flag_a_marked_host_that_is_not_bookable_takes_no_backfill() {
    let mut l = marked("backfill_cordoned");
    let h1 = HostId::new("h1").unwrap();
    l.agents
        .registry
        .cordon(&h1, Duration::from_secs(600))
        .unwrap();
    let refused = l.share(3, 1);
    assert_eq!(refused, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.agents.registry.tail_counters().backfilled, 0);

    let mut l = marked("backfill_lost");
    l.clock
        .advance(Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS + 1));
    let refused = l.share(3, 1);
    assert!(matches!(refused, Err(StartAck::Refused(_))), "{refused:?}");
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
    assert_eq!(l.agents.registry.tail_counters().backfilled, 0);
}

/// [`live`] with `--placement js-sched` over `h1` alone, carrying lease 1 of 600 s from
/// [`T0`], with request `q`'s reservation of `h1` put back from a ledger whose tail is
/// `tail_end`, as a restart puts it back; the clock at 10 s.
fn restored(name: &str, tail_end: Option<u64>) -> Live {
    let mut l = live(name, &["h1"], &["--placement", "js-sched"]);
    assert_eq!(l.share(1, 600).as_deref(), Ok("h1"));
    let record = js_lease::ReservationRecord {
        request_id: "q".to_owned(),
        held: Some(js_lease::Held {
            host: HostId::new("h1").unwrap(),
            reserved_at: Timestamp::from_millis(T0),
            deadline: Timestamp::from_millis(T0 + 3_600_000),
            assigned: false,
            tail_end,
        }),
        queued: Some(js_lease::Queued {
            place: 0,
            since: Timestamp::from_millis(T0),
        }),
        give_ups: 0,
        prepare_failures: 0,
        exclusions: Vec::new(),
        claim_by: None,
    };
    let back = l.agents.registry.restore_reservations(&[record]);
    assert_eq!(back.resumed, 1);
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), tail_end)));
    l.at(10_000);
    l
}

/// Asserts, through the server's placer, backfill against a reservation put back from the
/// ledger: with its tail at 600 s, a lease of 300 s is booked on `h1` and counted as
/// backfill, and a lease of 600 s is refused; the reservation keeps its tail. It catches a
/// restored mark that takes no backfill, or any.
#[test]
fn with_the_flag_a_restored_reservation_takes_backfill_by_its_persisted_tail() {
    let mut l = restored("backfill_restored", Some(T0 + 600_000));
    assert_eq!(l.share(3, 300).as_deref(), Ok("h1"));
    assert_eq!(l.agents.registry.tail_counters().backfilled, 1);
    let late = l.share(4, 600);
    assert_eq!(late, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.mark("q"), Some(("h1".to_owned(), Some(T0 + 600_000))));
}

/// Asserts, through the server's placer, that a reservation put back from a ledger written
/// without a tail takes no backfill: even a lease of a second is refused. It catches a
/// missing tail read as a far one.
#[test]
fn with_the_flag_a_restored_reservation_without_a_tail_takes_no_backfill() {
    let mut l = restored("backfill_restored_legacy", None);
    let refused = l.share(3, 1);
    assert_eq!(refused, Err(StartAck::Refused(RefusalCode::NoCapacity)));
    assert_eq!(l.on("h1"), [LeaseId::new(1, 1)]);
    assert_eq!(l.agents.registry.tail_counters().backfilled, 0);
}

/// A lease of `cpu_millis` of class `share` that may run `wall`, numbered `sequence`.
fn share_sized(sequence: u64, cpu_millis: u64, wall: Duration) -> StartSpec {
    let mut request = share_for(sequence, wall).request().clone();
    request.resources = Resources::new(cpu_millis, 1 << 20, 0, None).unwrap();
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

/// Asserts, with one engine throughout, that a mark that goes frees its host and that the
/// mark left still holds: two hosts each carry a five-core lease of 600 s and are set aside
/// for two whole-node requests; once one request is cancelled, a lease of 600 s, which no
/// marked host takes, is booked on the host it held, and on the host still marked a
/// three-core lease of 600 s is refused while one of 60 s is booked as backfill. It catches
/// a hold the engine keeps after its mark is gone (the freed host would refuse long work)
/// and one dropped with another mark's.
#[test]
fn with_the_flag_a_cancelled_mark_frees_its_host_and_the_other_still_holds() {
    // Two hosts may be drained at once, so each request takes one.
    let pool = format!("max_draining = 2\n{SHARED_POOL}");
    let hosts = ["h1", "h2"];
    let mut l = live_over(
        "backfill_cancelled",
        &pool,
        &hosts,
        &["--placement", "js-sched"],
    );
    let five = |sequence| share_sized(sequence, 5_000, Duration::from_secs(600));
    let first = host_of(&l.provider.start(five(1))).unwrap().to_owned();
    let second = host_of(&l.provider.start(five(2))).unwrap().to_owned();
    assert_ne!(first, second);
    assert!(host_of(&l.whole(3, "q1")).is_none());
    assert!(host_of(&l.whole(4, "q2")).is_none());
    let freed = l.mark("q1").unwrap().0;
    let held = l.mark("q2").unwrap().0;
    assert_ne!(freed, held);
    l.at(10_000);
    l.asks.retain(|(_, name)| name != "q1");
    l.provider.stop(&LeaseId::new(1, 3));
    assert_eq!(l.mark("q1"), None);
    let long = share_sized(5, 1_000, Duration::from_secs(600));
    assert_eq!(host_of(&l.provider.start(long)), Some(freed.as_str()));
    let three_long = share_sized(6, 3_000, Duration::from_secs(600));
    let refused = l.provider.start(three_long);
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    let three_short = share_sized(7, 3_000, Duration::from_secs(60));
    assert_eq!(host_of(&l.provider.start(three_short)), Some(held.as_str()));
    assert_eq!(l.mark("q2").map(|mark| mark.0), Some(held));
}
