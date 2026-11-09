//! `--estimate-feed` wired into the server, in process: the configuration read from the
//! flags, the agent side assembled from it as the server assembles it over a usage file that
//! has learned a profile, hosts registered with the registry, and leases started on its
//! provider. No listener is bound and no connection is made. Which host a whole-node request
//! reserves shows what each lease was costed at.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{Hello, Isolation, JobSpec, Limits};
use js_lease::{Booking, LeaseState, LeaseTable, read_bookings};
use js_provider::{Pool, StartSpec};
use js_registry::{Credential, RegisteredProvider};
use js_state::SystemClock;
use js_state::sqlite::{Options, SqliteStore};
use js_testkit::dir::TestDir;
use js_types::{Admission, ClassId, HostId, LeaseId, Request, RequestId, ResourceClass, Resources};
use js_types::{ManualClock, Timestamp};
use js_usage::{NoUsage, ProfileCurves, ProfileKey, ProfileStore, RunMinute, RunSeries, UsageSink};
use serde_json::json;

use crate::adapt::{BoxedPool, SharedClock};
use crate::agents::{Agents, assemble};
use crate::config::{Cli, Config};

/// The join token of the pool.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// When the test's clock starts: day 0 of the usage file, so a profile learned on day 0 is
/// fresh.
const T0: u64 = 1_000_000;

/// A profile the usage file has learned: three runs of one minute, so its P90 is one
/// minute.
const FAST: &str = "est.fast";

/// A profile the usage file has learned: three runs of three minutes, so its P90 is three
/// minutes.
const THREE: &str = "est.three";

/// A pool of up to two machines whose class books a whole host, so a lease put back is a
/// plain booking. The provider of the tests serves the whole-node class beside it, so a
/// whole-node request waits for a host and reserves one.
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
/// guard is dropped.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/estimate_feed");
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
/// Usage is on, in `usage.db` beside the lease file.
fn config(dir: &Path, extra: &[&str]) -> Config {
    config_booted(dir, "initialise", extra)
}

/// As [`config`], with `--boot boot`.
fn config_booted(dir: &Path, boot: &str, extra: &[&str]) -> Config {
    let flags = [
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", dir.join("leases.toml").into_os_string()),
        ("--boot", boot.into()),
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

/// Writes into the usage file at `db` three clean runs of one minute of [`FAST`] and three
/// of three minutes of [`THREE`], on day 0, as the server's profile store writes them when
/// runs end; the server opening the file then holds their curves, with P90s of one and
/// three minutes.
fn learn(db: &Path) {
    let store = SqliteStore::open(db, Arc::new(SystemClock::default()), Options::default());
    let store = Arc::new(store.unwrap());
    let curves = Arc::new(ProfileCurves::new(Box::new(|_| None)));
    let mut profiles = ProfileStore::open(store, curves.clone(), 1 << 20, 0).unwrap();
    for (name, minutes) in [(FAST, 1), (THREE, 3)] {
        let minute = |minute| RunMinute {
            minute,
            cores_milli: 1000,
            ws_bytes: 1 << 20,
        };
        let series = RunSeries {
            minutes: (0..minutes).map(minute).collect(),
            flags: 0,
        };
        let key = ProfileKey::parse(name).unwrap();
        for _ in 0..3 {
            profiles.run_ended(&key, &series, 0).unwrap();
        }
        assert_eq!(curves.wall_minutes_p90_of(&key), Some(minutes));
    }
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
/// assembles it over a usage file that has learned [`FAST`] and [`THREE`], with the
/// bookings `restored` put back as the server puts back those of its lease file, hosts `a`
/// and `b` registered, and the provider of its registry serving a class that books a whole
/// host and the whole-node class.
fn serving(name: &str, extra: &[&str], restored: &[Booking]) -> Served {
    serving_with(name, extra, Restored::Given(restored), None)
}

/// What a fixture puts back: bookings as given, or the bookings of a lease file it writes
/// with this text.
enum Restored<'a> {
    Given(&'a [Booking]),
    File(&'a str),
}

/// As [`serving`], measuring usage with `sink` when one is given, and putting back what
/// `restored` says.
fn serving_with(
    name: &str,
    extra: &[&str],
    restored: Restored<'_>,
    sink: Option<Arc<dyn UsageSink>>,
) -> Served {
    let dir = scratch(name);
    let config = config(dir.path(), extra);
    let config = match sink {
        Some(sink) => config.with_usage_sink(sink),
        None => config,
    };
    learn(&dir.path().join("usage.db"));
    let restored = match restored {
        Restored::Given(bookings) => bookings.to_vec(),
        Restored::File(text) => {
            std::fs::write(&config.state_file, text).unwrap();
            read_bookings(&config.state_file).unwrap()
        }
    };
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let (source, agents) =
        assemble(config.hosts, config.usage, &clock, &config.state_file, None).unwrap();
    let seated = source.provider(&restored).unwrap();
    assert!(seated.unseated.is_empty(), "{:?}", seated.unseated);
    let agents = agents.unwrap();
    let credential = Credential::of_token(JOIN);
    for host in ["a", "b"] {
        agents.registry.hello(&hello(host), &credential).unwrap();
    }
    let provider = agents
        .registry
        .provider()
        .with_classes(vec![book(), whole()]);
    Served {
        agents,
        provider,
        _dir: dir,
    }
}

fn book() -> ResourceClass {
    ResourceClass::new(ClassId::new("book").unwrap(), Admission::BookAll, 0)
}

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn request(class: &ResourceClass, wall: Duration) -> Request {
    Request {
        class: class.id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: wall,
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Starts lease `sequence` of [`book`], bound to `wall`, and returns the host it took.
fn lease_for(served: &mut Served, sequence: u64, wall: Duration) -> String {
    let spec = StartSpec::new(LeaseId::new(1, sequence), request(&book(), wall), &[book()]);
    match served.provider.start(spec.unwrap()) {
        js_provider::StartAck::Started { host } => host.to_string(),
        other => panic!("lease {sequence} not started: {other:?}"),
    }
}

/// Queues run `run` of lease `sequence` on `on`, named by the profile [`FAST`].
fn keyed_run(served: &Served, on: &str, sequence: u64, run: &str) {
    keyed_run_of(served, on, sequence, run, FAST);
}

/// Queues run `run` of lease `sequence` on `on`, named by the profile `name`.
fn keyed_run_of(served: &Served, on: &str, sequence: u64, run: &str, name: &str) {
    let spec = JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
        network: Default::default(),
        ports: Vec::new(),
        run: None,
        os: None,
        arch: None,
    };
    let profile = Some(ProfileKey::parse(name).unwrap());
    let lease = LeaseId::new(1, sequence);
    let registry = &served.agents.registry;
    registry
        .queue_keyed_run(&host(on), &lease, spec, profile)
        .unwrap();
}

/// Asks for a whole node as request `q` under lease `sequence` and returns the host
/// reserved for it.
fn reserved_for_whole_node(served: &mut Served, sequence: u64) -> Option<String> {
    reserved_with_tail(served, sequence).0
}

/// As [`reserved_for_whole_node`], with the tail the host was reserved with.
fn reserved_with_tail(served: &mut Served, sequence: u64) -> (Option<String>, Option<u64>) {
    let id = RequestId::new("q").unwrap();
    let wanted = request(&whole(), Duration::from_secs(600));
    let spec = StartSpec::new(LeaseId::new(1, sequence), wanted, &[whole()]);
    served
        .provider
        .start(spec.unwrap().with_request_id(id.clone()));
    let reservations = served.agents.registry.reservations();
    let mark = reservations.into_iter().find(|mark| mark.request == id);
    let host = mark.as_ref().map(|mark| mark.host.to_string());
    (host, mark.and_then(|mark| mark.tail_end))
}

/// Asserts the A/B case of the feed. Host `a` carries a lease of the longer max wall (an
/// hour) whose keyed run has a learned P90 of one minute, so its estimated tail is the
/// shorter; host `b` carries a lease of ten minutes with no keyed run. With
/// `--estimate-feed` the whole-node request reserves `a`; without it no estimate is passed,
/// every lease is costed at its bound, and the request reserves `b`, as before the flag
/// existed. It catches a feed that is on without the flag, one the flag does not turn on,
/// and a flag that does not reach the registry the server assembles.
#[test]
fn with_the_flag_the_shorter_estimated_tail_is_reserved_and_without_it_the_shorter_bound() {
    for (name, extra, expected) in [
        ("feed_on", vec!["--estimate-feed"], "a"),
        ("feed_off", vec![], "b"),
    ] {
        let mut served = serving(name, &extra, &[]);
        assert_eq!(lease_for(&mut served, 1, Duration::from_secs(3600)), "a");
        assert_eq!(lease_for(&mut served, 2, Duration::from_secs(600)), "b");
        keyed_run(&served, "a", 1, "r1");
        let reserved = reserved_for_whole_node(&mut served, 3);
        assert_eq!(reserved.as_deref(), Some(expected), "{name}");
    }
}

/// Asserts the A/B case of the feed under `--placement js-sched`, where the placement engine,
/// not the registry's own pass, marks the host a whole-node request drains: the registry
/// hands the engine its expected lives of the leases, so with `--estimate-feed` the request
/// marks `a`, whose lease's keyed run has a learned P90 of one minute, and without it the
/// request marks `b`, whose lease is bound to end first. It catches an engine that drains by
/// the bounds alone (it would mark `b` with the feed on) and a feed that reaches the engine
/// without the flag.
#[test]
fn under_js_sched_the_engine_drains_by_the_registrys_lives() {
    for (name, extra, expected) in [
        (
            "engine_feed_on",
            vec!["--placement", "js-sched", "--estimate-feed"],
            "a",
        ),
        ("engine_feed_off", vec!["--placement", "js-sched"], "b"),
    ] {
        let mut served = serving(name, &extra, &[]);
        assert_eq!(lease_for(&mut served, 1, Duration::from_secs(3600)), "a");
        assert_eq!(lease_for(&mut served, 2, Duration::from_secs(600)), "b");
        keyed_run(&served, "a", 1, "r1");
        let reserved = reserved_for_whole_node(&mut served, 3);
        assert_eq!(reserved.as_deref(), Some(expected), "{name}");
    }
}

/// Asserts the unknown case through the server, with the flag on: `a`'s lease may run for
/// ever (its end saturates, which with the feed on is no bound) and has no keyed run, so its
/// tail is unknown and ranks after `b`'s, which is bound to end in ten hours: `b` is reserved
/// with that tail. Once a keyed run with a learned P90 of one minute is queued on `a`'s
/// lease, `a` is known, ends a minute after its start and is reserved with that tail. It
/// catches an unknown tail ranked by what is known of its host, or first. (That the tail is
/// unknown, not a far bound, is asserted by the registry's tests, where one host is alone.)
#[test]
fn a_lease_with_no_keyed_run_and_no_bound_is_unknown_and_ranks_after_a_known_tail() {
    let mut served = serving("feed_unknown", &["--estimate-feed"], &[]);
    let forever = Duration::from_millis(u64::MAX);
    assert_eq!(lease_for(&mut served, 1, forever), "a");
    assert_eq!(lease_for(&mut served, 2, Duration::from_secs(36_000)), "b");
    let reserved = reserved_with_tail(&mut served, 3);
    assert_eq!(reserved, (Some("b".to_string()), Some(T0 + 36_000_000)));
    let q = RequestId::new("q").unwrap();
    assert!(served.agents.registry.cancel_request(&q));
    keyed_run(&served, "a", 1, "r1");
    let reserved = reserved_with_tail(&mut served, 4);
    assert_eq!(reserved, (Some("a".to_string()), Some(T0 + 60_000)));
}

/// A lease file that holds lease 1, held on `a`, acquired two minutes before [`T0`] and
/// bound to end 15 minutes after that.
const FILE: &str = "version = 1\nterm = 1\nnext_sequence = 2\nlease_count = 1\n\n\
    [[lease]]\nterm = 1\nsequence = 1\nstate = \"held\"\nhost = \"a\"\n\
    acquired_at_ms = 880000\nexpires_at_ms = 1780000\nends_at_ms = 1780000\n";

/// The flags of `--placement js-sched`, with `--estimate-feed` when `feed` says so.
fn js_sched(feed: bool) -> Vec<&'static str> {
    let mut flags = vec!["--placement", "js-sched"];
    flags.extend(feed.then_some("--estimate-feed"));
    flags
}

/// Lease 1, held on `a`, started at [`T0`] and bound to end an hour later.
fn restored_for_an_hour() -> Booking {
    Booking {
        lease: LeaseId::new(1, 1),
        host: host("a"),
        state: LeaseState::Held,
        shares: None,
        started_at: Timestamp::from_millis(T0),
        ends_at: Timestamp::from_millis(T0 + 3_600_000),
    }
}

/// Asserts, under `--placement js-sched`, that a lease put back from the lease file keeps
/// its recorded bound in the engine's drain cost, with the flag and without it (the engine
/// drains by bounds either way). Lease 1 on `a` is restored bound to end in an hour; lease 2
/// on `b` starts here with ten minutes: `b` is reserved, with its bound as the tail. It
/// catches a restored lease the engine does not hold (`a` would read empty and be reserved
/// with no wait), and one it holds with no bound.
#[test]
fn under_js_sched_a_restored_lease_keeps_its_recorded_bound() {
    let restored = restored_for_an_hour();
    for (name, feed) in [("engine_restored_on", true), ("engine_restored_off", false)] {
        let mut served = serving(name, &js_sched(feed), std::slice::from_ref(&restored));
        assert_eq!(lease_for(&mut served, 2, Duration::from_secs(600)), "b");
        let reserved = reserved_with_tail(&mut served, 3);
        assert_eq!(
            reserved,
            (Some("b".to_string()), Some(T0 + 600_000)),
            "{name}"
        );
    }
}

/// Asserts, with a usage sink that keeps no curves, under `--placement js-sched`: the engine is handed no lives, the restored lease keeps its bound
/// of an hour, and `b` (ten minutes) is reserved with its own bound as the tail. It catches
/// an engine that loses the restored bound when the sink keeps no curves.
#[test]
fn under_js_sched_with_a_sink_with_no_curves_a_restored_lease_keeps_its_bound() {
    let restored = restored_for_an_hour();
    let given = Restored::Given(std::slice::from_ref(&restored));
    let sink: Arc<dyn UsageSink> = Arc::new(NoUsage);
    let flags = js_sched(true);
    let mut served = serving_with("engine_no_curves", &flags, given, Some(sink));
    assert_eq!(lease_for(&mut served, 2, Duration::from_secs(600)), "b");
    keyed_run(&served, "a", 1, "r1");
    let reserved = reserved_with_tail(&mut served, 3);
    assert_eq!(reserved, (Some("b".to_string()), Some(T0 + 600_000)));
}

/// Asserts that a start read from an actual lease file reaches the drain cost through the
/// server's startup path, under `--placement js-sched`: with the flag, lease 1 on `a`, started two minutes before the restart with a
/// keyed run of three, ends a minute after now, before lease 2 on `b` (two minutes), and
/// `a` is reserved with that tail. Without the flag the engine costs `a` at its recorded
/// bound (13 minutes from now) and reserves `b`, with its bound of two minutes as the tail.
/// It catches the recorded start replaced by the restart (`a` would end three minutes from
/// now and `b` be reserved with the flag), and the recorded bound dropped (`a` would read
/// as ending at the latest time there is, or empty).
#[test]
fn under_js_sched_a_start_read_from_the_lease_file_reaches_the_drain_cost() {
    for (name, feed, expected) in [
        ("engine_file_on", true, ("a", T0 + 60_000)),
        ("engine_file_off", false, ("b", T0 + 120_000)),
    ] {
        let mut served = serving_with(name, &js_sched(feed), Restored::File(FILE), None);
        assert_eq!(lease_for(&mut served, 2, Duration::from_secs(120)), "b");
        keyed_run_of(&served, "a", 1, "r1", THREE);
        let reserved = reserved_with_tail(&mut served, 3);
        let (on, tail) = expected;
        assert_eq!(reserved, (Some(on.to_string()), Some(tail)), "{name}");
    }
}

/// The lease table of a server over the files in `dir`, started as the server starts it,
/// with no listener: the configuration read from `--boot boot` and `extra` flags (measuring
/// usage with `sink` when one is given), the bookings of the lease file read, the agent side
/// assembled, the bookings put back on its provider and the table opened over that
/// provider. Returns the table and the agent side.
fn started(
    dir: &Path,
    boot: &str,
    extra: &[&str],
    sink: Option<Arc<dyn UsageSink>>,
    clock: &SharedClock,
) -> (LeaseTable<BoxedPool, SharedClock>, Arc<Agents>) {
    let config = config_booted(dir, boot, extra);
    let config = match sink {
        Some(sink) => config.with_usage_sink(sink),
        None => config,
    };
    let bookings = read_bookings(&config.state_file).unwrap();
    let (source, agents) =
        assemble(config.hosts, config.usage, clock, &config.state_file, None).unwrap();
    let seated = source.provider(&bookings).unwrap();
    assert!(seated.unseated.is_empty(), "{:?}", seated.unseated);
    let provider = BoxedPool::new(seated.provider);
    let table = LeaseTable::open(
        provider,
        clock.clone(),
        config.settings,
        &config.state_file,
        config.boot,
    );
    (table.unwrap(), agents.unwrap())
}

/// Asserts the start-up path of a server with the flag, in process: a lease granted on a
/// registered host is written to the lease file, and a restart with the flag (`--boot
/// resume`) reads it back, puts it back held on that host, and keeps it held once the host
/// says hello again and the table ticks; over the server's own usage file and over a usage
/// sink that keeps no curves. It catches a start that fails, or drops or moves a restored
/// lease, when the feed is on.
#[test]
fn with_the_flag_a_restart_puts_back_a_held_lease_on_its_host() {
    let sinks: [(&str, Option<Arc<dyn UsageSink>>); 2] = [
        ("restart_curves", None),
        ("restart_no_curves", Some(Arc::new(NoUsage))),
    ];
    for (name, sink) in sinks {
        let dir = scratch(name);
        learn(&dir.path().join("usage.db"));
        let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
        let flag = ["--estimate-feed"];
        let (mut table, agents) = started(dir.path(), "initialise", &flag, sink.clone(), &clock);
        let credential = Credential::of_token(JOIN);
        agents.registry.hello(&hello("a"), &credential).unwrap();
        let class = ResourceClass::new(ClassId::new("burst-host").unwrap(), Admission::BookAll, 0);
        let lease = table.acquire(request(&class, Duration::from_secs(600)));
        let lease = lease.unwrap();
        assert_eq!(lease.host, host("a"), "{name}");
        drop(table);
        drop(agents);

        let (mut table, agents) = started(dir.path(), "resume", &flag, sink, &clock);
        let listed = table.list();
        assert_eq!(listed.len(), 1, "{name}");
        assert_eq!(listed[0].id, lease.id, "{name}");
        assert_eq!(listed[0].state, LeaseState::Held, "{name}");
        assert_eq!(listed[0].host, Some(host("a")), "{name}");
        agents.registry.hello(&hello("a"), &credential).unwrap();
        table.tick();
        let listed = table.list();
        assert_eq!(listed.len(), 1, "{name}");
        assert_eq!(listed[0].host, Some(host("a")), "{name}");
        assert_eq!(listed[0].state, LeaseState::Held, "{name}");
    }
}

/// The refusal `assemble` gives for the agent side over `dir` with `extra` flags, after
/// `spoil` has been done to the directory.
fn refused(name: &str, extra: &[&str], spoil: impl Fn(&Path)) -> String {
    let dir = scratch(name);
    let config = config(dir.path(), extra);
    spoil(dir.path());
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let assembled = assemble(config.hosts, config.usage, &clock, &config.state_file, None);
    match assembled {
        Err(err) => err.to_string(),
        Ok(_) => panic!("{name}: assembled"),
    }
}

/// Asserts that with the flag a start whose usage file will not open (a directory in its
/// place) is refused naming `--usage-db`, and one whose file of join tokens cannot be read
/// (a directory in its place) is refused, before any source of lives is set. It catches a
/// feed that turns a broken usage file into a start with no estimates.
#[test]
fn with_the_flag_a_start_over_files_that_will_not_open_is_refused() {
    let usage = refused("refused_usage", &["--estimate-feed"], |dir| {
        std::fs::create_dir(dir.join("usage.db")).unwrap();
    });
    assert!(usage.starts_with("--usage-db: "), "{usage}");
    let joins = refused("refused_joins", &["--estimate-feed"], |dir| {
        std::fs::create_dir(dir.join("leases.toml.joins")).unwrap();
    });
    assert!(!joins.is_empty());
    assert!(!joins.starts_with("--usage-db"), "{joins}");
}
