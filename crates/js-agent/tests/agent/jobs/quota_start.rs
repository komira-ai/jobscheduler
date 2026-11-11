//! `--scratch-quota` at the start of the whole agent: the probe that must pass, the flags
//! that are checked, the capabilities the hello carries, and the agreement of the agent's
//! booking with the server's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::Capacity;
use js_agent::quota::FakeQuota;
use js_agent_wire::Hello;
use js_jobs::{
    Constraints, JobFiles, JobId, JobSpec, JobStore, Needs, NetworkPolicy, NewJob, RandomIds,
};
use js_registry::{Credential, PoolConfig, Registry, SequentialSessions};
use js_sched_placer::engine::jobs::{JobHosts, NoStarts, OpenHost, PassMetrics};
use js_state::fake::{Fake, Flaw};
use js_types::{
    Admission, ClassId, HostId, ImageDigestRef, Labels, ProviderId, ResourceClass, Resources, Shape,
};
use js_types::{ManualClock, Timestamp};
use serde_json::{Value, json};

use super::drain::container_flags;
use super::fake::FakeMachine;
use crate::lifecycle::quota::local_with;
use crate::lifecycle::{Harness, job, names};
use crate::support::{
    CountingSleeper, Jobs, Lowest, Ran, Scratch, Script, ScriptConnector, code, flags, linux_host,
    reply, run_on,
};

const GIB: u64 = 1 << 30;

const DF_HEAD: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n";

/// Runs the whole agent over containers on `machine`, on a host whose `df` says `free_gib`
/// GiB are free on the scratch root, with `extra` flags beside the container ones, until the
/// server refuses its hello; returns what it printed and the hello it sent (`null` when it
/// sent none).
fn start(name: &str, machine: FakeMachine, free_gib: u64, extra: &[&str]) -> (Ran, Value) {
    let dir = Scratch::new(name);
    let mut host = linux_host();
    let line = format!("/bin/df -P -k {}", dir.path("run-scratch").display());
    let row = format!("{DF_HEAD}/dev/sda1 99999999 100 {} 11% /\n", free_gib << 20);
    host.commands = host.commands.with(&line, Ok(&row));
    let mut owned = container_flags(&dir);
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let refusal = reply(403, json!({ "error": "host-contract", "field": "runtime" }));
    let (script, sent) = Script::new(vec![refusal]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (Arc::new(machine), None);
    let ran = run_on(
        &flags(&all),
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// Asserts that a probe that is not `Project` ends the agent only when `--scratch-quota`
/// asked for enforcement: with the flag the agent exits 2 with the probe's own reason and
/// sends nothing; without it the same machine starts, says hello with no quota capability,
/// and logs the probe's answer as before. It catches an agent that starts without the
/// enforcement the operator asked for (its jobs would be unbounded and the server told they
/// are bounded), and one that refuses a host that never asked for a quota.
#[test]
fn a_probe_that_is_not_project_refuses_the_start_only_with_scratch_quota() {
    let reason = "/dev/sda1 is mounted without prjquota";
    let broken = || FakeMachine::new().with_quota(FakeQuota::unsupported(reason));

    let (ran, hello) = start("quota-probe-flag", broken(), 10, &["--scratch-quota"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said =
        format!("--scratch-quota: the scratch root cannot enforce a project quota: {reason}");
    assert!(ran.err.contains(&said), "{}", ran.err);
    assert_eq!(hello, Value::Null, "nothing was sent");

    let (ran, hello) = start("quota-probe-no-flag", broken(), 10, &[]);
    assert!(
        ran.err.contains(&format!("scratch quota: none ({reason})")),
        "{}",
        ran.err
    );
    assert_ne!(hello, Value::Null, "the agent started");
    for key in ["scratch-quota", "scratch-default-gib"] {
        assert!(hello["capabilities"].get(key).is_none(), "{key}: {hello}");
    }
}

/// Asserts the capabilities of the hello: with `--scratch-quota` and a passing probe,
/// `scratch-quota=project` and `scratch-default-gib` (2 without the flag, the flag's value
/// with it); without `--scratch-quota`, neither, whatever the probe says. They are added by
/// the agent with the host contract, so `--capability` cannot give them (pinned in
/// `flag_cannot_claim_scratch_quota`). It catches a quota advertised on a host that does
/// not enforce one, a default that differs from what the agent books, and one never sent.
#[test]
fn a_passing_probe_advertises_the_quota_and_its_default() {
    let cases: [(&[&str], Option<&str>); 3] = [
        (&["--scratch-quota"], Some("2")),
        (
            &["--scratch-quota", "--scratch-default-gib", "5"],
            Some("5"),
        ),
        (&[], None),
    ];
    for (n, (extra, default)) in cases.into_iter().enumerate() {
        let (_, hello) = start(&format!("quota-adv-{n}"), FakeMachine::new(), 10, extra);
        let caps = &hello["capabilities"];
        match default {
            Some(gib) => {
                assert_eq!(caps["scratch-quota"], "project", "{extra:?}");
                assert_eq!(caps["scratch-default-gib"], gib, "{extra:?}");
            }
            None => {
                assert!(caps.get("scratch-quota").is_none(), "{extra:?}: {hello}");
                assert!(caps.get("scratch-default-gib").is_none(), "{extra:?}");
            }
        }
        assert_eq!(caps["runtime"], "container");
    }
}

/// Asserts the checks of the new flags: all need `--scratch-quota`; the default is from 1
/// to 65535 GiB (its bytes must fit the wire's disk limit) and the inode limit from 1 to
/// 4294967295, the failures from 1 to 1000000, the usage wait from 1 to 86400 seconds; the edges are accepted. Each refusal ends the agent with status 2, names the
/// flag, and sends nothing. It catches a flag that does nothing without `--scratch-quota`,
/// a default of 0 (which would book no disk), and one whose bytes overflow the wire.
#[test]
fn the_quota_flags_are_checked() {
    let refused: &[(&[&str], &str)] = &[
        (
            &["--scratch-default-gib", "2"],
            "--scratch-default-gib: needs --scratch-quota",
        ),
        (
            &["--scratch-max-inodes", "5"],
            "--scratch-max-inodes: needs --scratch-quota",
        ),
        (
            &["--scratch-quota", "--scratch-default-gib", "0"],
            "--scratch-default-gib: 0 is not from 1 to 65535",
        ),
        (
            &["--scratch-quota", "--scratch-default-gib", "65536"],
            "--scratch-default-gib: 65536 is not from 1 to 65535",
        ),
        (
            &["--scratch-quota", "--scratch-max-inodes", "0"],
            "--scratch-max-inodes: 0 is not from 1 to 4294967295",
        ),
        (
            &["--scratch-quota", "--scratch-max-inodes", "4294967296"],
            "--scratch-max-inodes: 4294967296 is not from 1 to 4294967295",
        ),
        (
            &["--scratch-quota-failures", "3"],
            "--scratch-quota-failures: needs --scratch-quota",
        ),
        (
            &["--scratch-quota", "--scratch-quota-failures", "0"],
            "--scratch-quota-failures: 0 is not from 1 to 1000000",
        ),
        (
            &["--scratch-quota", "--scratch-quota-failures", "1000001"],
            "--scratch-quota-failures: 1000001 is not from 1 to 1000000",
        ),
        (
            &["--scratch-usage-wait-secs", "30"],
            "--scratch-usage-wait-secs: needs --scratch-quota",
        ),
        (
            &["--scratch-quota", "--scratch-usage-wait-secs", "0"],
            "--scratch-usage-wait-secs: 0 is not from 1 to 86400",
        ),
        (
            &["--scratch-quota", "--scratch-usage-wait-secs", "86401"],
            "--scratch-usage-wait-secs: 86401 is not from 1 to 86400",
        ),
        (
            &["--scratch-abandon-limit", "8"],
            "--scratch-abandon-limit: needs --scratch-quota",
        ),
        (
            &["--scratch-quota", "--scratch-abandon-limit", "0"],
            "--scratch-abandon-limit: 0 is not from 1 to 1000000",
        ),
        (
            &["--scratch-quota", "--scratch-abandon-limit", "1000001"],
            "--scratch-abandon-limit: 1000001 is not from 1 to 1000000",
        ),
    ];
    for (n, (extra, said)) in refused.iter().copied().enumerate() {
        let (ran, hello) = start(&format!("quota-flag-{n}"), FakeMachine::new(), 10, extra);
        assert_eq!(ran.status(), code(2), "{extra:?}: {}", ran.err);
        assert!(ran.err.contains(said), "{extra:?}: {}", ran.err);
        assert_eq!(hello, Value::Null, "{extra:?}");
    }
    let edges = [
        "--scratch-quota",
        "--scratch-default-gib",
        "65535",
        "--scratch-max-inodes",
        "4294967295",
        "--scratch-quota-failures",
        "1000000",
        "--scratch-usage-wait-secs",
        "86400",
        "--scratch-abandon-limit",
        "1000000",
    ];
    let (ran, hello) = start("quota-flag-edges", FakeMachine::new(), 10, &edges);
    assert_ne!(hello, Value::Null, "{}", ran.err);
    assert_eq!(hello["capabilities"]["scratch-default-gib"], "65535");
}

/// Asserts that an agent whose offered disk is below its default says so at start (a warning
/// naming the flag and the bytes offered) and still starts, and that one whose disk holds the
/// default says nothing: every run without `disk_bytes` would be refused as larger than the
/// machine. It catches a host that silently takes no work.
#[test]
fn a_default_above_the_offered_disk_is_warned_about() {
    let (ran, hello) = start("quota-warn", FakeMachine::new(), 1, &["--scratch-quota"]);
    assert_ne!(hello, Value::Null, "{}", ran.err);
    let said =
        "--scratch-default-gib: 2 GiB is more than the 1073741824 bytes of disk this host offers";
    assert!(ran.err.contains(said), "{}", ran.err);
    let (ran, _) = start("quota-nowarn", FakeMachine::new(), 10, &["--scratch-quota"]);
    assert!(!ran.err.contains("--scratch-default-gib"), "{}", ran.err);
}

/// The production placer, as the server gives it to a registry.
fn placer(clock: &ManualClock) -> std::sync::Arc<js_sched_placer::place::SchedPlacer> {
    let settings = js_sched_placer::engine::Settings::default();
    std::sync::Arc::new(js_sched_placer::place::SchedPlacer::new(
        settings,
        clock.clone(),
    ))
}

/// The architecture the agent of `hello` reports: the pool names it because the production
/// placer describes only hosts that have one.
fn arch_of(hello: &Hello) -> &str {
    &hello.capabilities["arch"]
}

/// A pass's metrics, which the quota tests do not read.
struct Unread;

impl PassMetrics for Unread {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {}
}

/// The job `id`, asking for a CPU, a MiB and `disk_bytes` of disk, with a wall
/// limit of ten minutes.
fn queued_job(id: &str, disk_bytes: u64) -> NewJob {
    let resources = Resources::new(1000, 1 << 20, 0, None)
        .unwrap()
        .with_disk_bytes(disk_bytes);
    let image = format!("team/app@sha256:{}", "a".repeat(64));
    let spec = JobSpec {
        image: ImageDigestRef::parse(&image).ok(),
        command: vec!["run".to_owned()],
        args: Vec::new(),
        env: BTreeMap::new(),
        needs: Needs::Shape(resources),
        constraints: Constraints::default(),
        wall_limit_secs: 600,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: JobFiles::default(),
        labels: Labels::default(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
    };
    NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority: 0,
        client: None,
    }
}

/// How many jobs the server places onto the host that said `hello`, each declaring the disk
/// the hello's `scratch-default-gib` names (what the agent books for a run that declares
/// none), in a pool that books the hello's disk and names `scratch-quota`: the hello goes to
/// the registry, twenty jobs of one CPU each are queued, and one pass of the production
/// placer over the job tables places what the host's disk holds (the pool's eight cores
/// hold more than any case here offers disk for).
fn server_admits(hello: &Hello) -> usize {
    let resources = Resources::new(8000, 16 << 30, 0, None)
        .unwrap()
        .with_disk_bytes(hello.shape.disk_bytes);
    let caps: BTreeMap<String, String> = [
        ("os", "linux"),
        ("arch", arch_of(hello)),
        ("scratch-quota", "project"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let class = ResourceClass::new(ClassId::new("box").unwrap(), Admission::BookAll, 0);
    let name = ProviderId::new(&hello.pool).unwrap();
    let config = PoolConfig::new(name, class, Shape::new(resources, caps), 2);
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let placer = placer(&clock);
    let shared: Arc<dyn js_registry::Placer> = placer.clone();
    let registry = Registry::with_sessions(config, clock, SequentialSessions::new(), shared);
    registry.hello(hello, &Credential::of_token("t")).unwrap();

    let default_gib: u64 = hello.capabilities["scratch-default-gib"].parse().unwrap();
    let store_clock = Arc::new(js_state::ManualClock::default());
    let store = Arc::new(Fake::with_flaw(store_clock, Flaw::None));
    let jobs = JobStore::new(store, RandomIds).unwrap();
    for n in 0..20 {
        let job = queued_job(&format!("job-{n}"), default_gib * GIB);
        jobs.insert_queued(job).unwrap();
    }
    let host = HostId::new(&hello.host_id).unwrap();
    let open = BTreeMap::from([(host.clone(), OpenHost::default())]);
    let said_hello = BTreeSet::from([host]);
    let hosts = JobHosts {
        open: &open,
        hello: &said_hello,
        whole: None,
    };
    let placed = placer.place_jobs_with(&jobs, &hosts, (&NoStarts, &Unread));
    placed.unwrap().len()
}

/// Asserts that a pool that books no disk and names no quota refuses the hello of an agent
/// that enforces one (`shape-not-accepted`): the pool would book 0 for runs the agent books
/// at its default, so the server would admit what the agent refuses.
fn plain_pool_refuses(hello: &Hello) {
    let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
    let caps: BTreeMap<String, String> = [
        ("os".to_owned(), "linux".to_owned()),
        ("arch".to_owned(), arch_of(hello).to_owned()),
    ]
    .into();
    let class = ResourceClass::new(ClassId::new("box").unwrap(), Admission::BookAll, 0);
    let name = ProviderId::new(&hello.pool).unwrap();
    let config = PoolConfig::new(name, class, Shape::new(resources, caps), 2);
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let placer = placer(&clock);
    let registry = Registry::with_sessions(config, clock, SequentialSessions::new(), placer);
    let refused = registry
        .hello(hello, &Credential::of_token("t"))
        .unwrap_err();
    assert_eq!(
        refused,
        js_registry::Refusal::ShapeNotAccepted(js_registry::ShapeFault::QuotaWithoutPoolDisk)
    );
    assert_eq!(refused.reason(), Some("quota-without-pool-disk"));
}

/// A run of a tenth of a CPU and a MiB, with no `disk_bytes`.
fn small_job(run: &str) -> js_agent_wire::JobSpec {
    let mut spec = job(run);
    spec.limits.cpu_millis = 100;
    spec.limits.memory_bytes = 1 << 20;
    spec
}

/// How many runs without `disk_bytes` the agent starts, on a lifecycle with the capacity of
/// `hello` and the quota `extra` flags ask for.
fn agent_starts(hello: &Hello, extra: &[&str]) -> usize {
    // `local_with` gives `--scratch-quota` itself.
    let rest: Vec<&str> = extra
        .iter()
        .copied()
        .filter(|flag| *flag != "--scratch-quota")
        .collect();
    let local = local_with("quota-agrees-local", &rest);
    let mut h = Harness::new("quota-agrees");
    h.config.capacity = Capacity::of(hello);
    h.config.quota = Some(local.run_quota(Arc::new(FakeQuota::new())));
    h.restart().unwrap();
    (0..20)
        .take_while(|n| {
            let run = format!("r{n}");
            names(&h.start(small_job(&run))) == [format!("started {run}")]
        })
        .count()
}

/// Asserts that the server never admits what the agent refuses: for the hello a real agent
/// sends (its capabilities and shape, read from the hello itself), the number of jobs
/// declaring the default disk that the server's placer places onto the host equals the
/// number of runs without `disk_bytes` the agent's lifecycle starts, for three offers and
/// two defaults, and both are the offer divided by the default, and a pool that books no
/// disk and names no quota refuses that hello instead of booking 0 for it. A server that
/// books another number for a job's disk (say, 0, or the default in other units) would send
/// a job the agent then refuses; an agent that booked less would run what the server
/// believes does not fit.
#[test]
fn server_never_admits_what_agent_refuses() {
    let cases: [(u64, &[&str], usize); 3] = [
        (7, &["--scratch-quota"], 3),
        (7, &["--scratch-quota", "--scratch-default-gib", "3"], 2),
        (5, &["--scratch-quota", "--scratch-default-gib", "1"], 5),
    ];
    for (n, (free_gib, extra, expected)) in cases.into_iter().enumerate() {
        let (ran, sent) = start(
            &format!("quota-agrees-{n}"),
            FakeMachine::new(),
            free_gib,
            extra,
        );
        assert_ne!(sent, Value::Null, "{}", ran.err);
        let hello: Hello = serde_json::from_value(sent).unwrap();
        assert_eq!(hello.shape.disk_bytes, free_gib * GIB);
        plain_pool_refuses(&hello);
        let (server, agent) = (server_admits(&hello), agent_starts(&hello, extra));
        assert_eq!(
            (server, agent),
            (expected, expected),
            "{free_gib} GiB, {extra:?}"
        );
    }
}
