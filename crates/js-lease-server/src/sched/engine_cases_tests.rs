//! The placement engine of `--placement js-sched` at its edges: a start it cannot describe
//! held to the memory percent, a rebuild that keeps the load it measured, readings that
//! stop coming, a host that comes back or goes silent past the engine's own grace, hosts
//! the registry forgets, a restart from the lease file, a joined pool of the configuration
//! file, usage off, starts, beats and stops from several threads, and the lease routes.
//! Everything runs in process on a manual clock; no listener is bound.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use js_agent_wire::Hello;
use js_lease::{ManualClock, Timestamp};
use js_provider::{Provider, StartSpec};
use js_registry::{Credential, HostState};
use js_types::{LeaseId, Resources};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::engine_rig::*;
use super::place::Fallbacks;
use super::wiring_tests::{
    JOIN, config_booted, config_result, host_of, scratch, sharing, sharing_spec, write,
};
use crate::api::{Shared, router};

/// What 90 percent of 16 GiB books, rounded down.
const NINETY: u64 = 16 * GIB / 100 * 90 + 16 * GIB % 100 * 90 / 100;

/// `pool` with `share_class = "share"`, so the lease table's own provider serves the class
/// of [`sharing_spec`].
fn shared_pool(machines: usize, cpu_millis: u64, extra: &str) -> String {
    pool(
        machines,
        cpu_millis,
        &format!("share_class = \"share\"\n{extra}"),
    )
}

/// [`share_of`] that also asks for the capabilities `caps`.
fn share_with(sequence: u64, memory_bytes: u64, caps: &[(&str, &str)]) -> StartSpec {
    let mut request = share_of(sequence, 1_000, memory_bytes).request().clone();
    request.capabilities = caps
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

/// A hello of `host` with the capabilities `caps`, claiming 16 cores and 32 GiB.
fn hello_with(host: &str, caps: &[(&str, &str)]) -> Hello {
    let caps: BTreeMap<&str, &str> = caps.iter().copied().collect();
    let body = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 16_000, "memory_bytes": 32 * GIB, "gpus": 0 },
        "capabilities": caps,
        "labels": {},
        "phase": "serving",
    });
    serde_json::from_value(body).unwrap()
}

/// Asserts that a start the engine cannot describe (a capability the pool's hosts carry
/// that is no platform requirement), which falls back, is held to the pool's memory
/// percent though the registry books the whole memory: at 90 such starts are booked up to
/// exactly 90% of 16 GiB, one byte more is booked nowhere, and two of 8 GiB do not both
/// fit. The leases they book are charged in the engine, so a start it can describe finds
/// the share used up. It catches a fallback booked past the percent.
#[test]
fn an_unmappable_start_is_held_to_the_memory_percent() {
    let odd = [("arch", "x86_64"), ("colour", "red")];
    let text = pool(1, 8000, "memory_schedulable_percent = 90").replace(
        "arch = \"x86_64\"\n",
        "arch = \"x86_64\"\ncolour = \"red\"\n",
    );
    let sched = ["--placement", "js-sched"];
    let mut r = rig("unmappable_percent", &text, &[], &sched);
    r.hello(&hello_with("h1", &odd));
    let red = [("colour", "red")];
    assert_eq!(
        r.start(share_with(1, NINETY - GIB, &red)).as_deref(),
        Some("h1")
    );
    assert_eq!(r.start(share_with(2, GIB, &red)).as_deref(), Some("h1"));
    assert_eq!(r.start(share_with(3, 1, &red)), None);
    assert_eq!(r.start(share_of(4, 1_000, 1)), None);
    let counted = Fallbacks {
        unmappable: 2,
        declined: 1,
    };
    assert_eq!(r.counted(), (counted, 0, 1));
    let mut r = rig("unmappable_halves", &text, &[], &sched);
    r.hello(&hello_with("h1", &odd));
    assert_eq!(r.start(share_with(1, 8 * GIB, &red)).as_deref(), Some("h1"));
    assert_eq!(r.start(share_with(2, 8 * GIB, &red)), None);
}

/// Asserts that a rebuild keeps the load the engine measured: the host carrying lease 1
/// reports all its cores busy, the other is idle; a lease booked behind the engine's back
/// on the busy host makes the next start rebuild the engine, and that start still goes to
/// the idle host, where a rebuilt engine that forgot the readings would put it beside the
/// two leases of the busy one. It catches a rebuild that drops measured load.
#[test]
fn a_rebuild_keeps_the_load_it_measured() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "rebuild_load",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let busy = r.start(sharing_spec(1)).unwrap();
    let idle = hosts.iter().find(|h| **h != busy).unwrap().to_string();
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat(&busy, Some(reading(seq, 8_000, GIB, &["1-1"])));
        r.beat(&idle, Some(reading(seq, 0, GIB, &[])));
    }
    let booking = js_lease::Booking {
        lease: LeaseId::new(9, 1),
        host: js_types::HostId::new(&busy).unwrap(),
        state: js_lease::LeaseState::Held,
        shares: Some(Resources::new(1000, 1 << 20, 0, None).unwrap()),
        started_at: Timestamp::from_millis(T0),
        ends_at: Timestamp::from_millis(T0 + 600_000),
    };
    r.provider.seat_booked(&booking, None).unwrap();
    assert_eq!(r.start(sharing_spec(2)).as_deref(), Some(idle.as_str()));
    assert_eq!(r.counted(), (NONE, 1, 2));
}

/// Asserts what beats with no new reading do to a host whose reading was fresh: a beat
/// with no load, one whose load the vet drops and one that repeats the last reading keep
/// the host live in the registry and a worker of the engine, and none refreshes the old
/// reading, which goes stale three windows after it came; the other host, whose readings
/// keep coming, stays fresh. Once stale, the busy host is charged by what it books again.
/// It catches a beat that refreshes an old reading, one that ends the host, and staleness
/// spilling from one host to another.
#[test]
fn a_reading_that_stops_coming_goes_stale_and_the_host_stays() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "stale_reading",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat("h1", Some(reading(seq, 8_000, GIB, &[])));
        r.beat("h2", Some(reading(seq, 0, GIB, &[])));
    }
    let view = |r: &Rig, host: &str| r.placer().load_of(host).unwrap();
    assert!(!view(&r, "h1").fallback);
    assert!(view(&r, "h1").left_cpu_millis >= 8_000);
    let mut bad = reading(3, 8_000, GIB, &[]);
    bad.active_file_bytes = js_agent_wire::limits::MAX_MEMORY_BYTES + 1;
    let beats = [None, Some(bad), Some(reading(2, 8_000, GIB, &[]))];
    for (n, beat) in (3..).zip(beats) {
        r.advance(5_000);
        r.beat("h1", beat);
        r.beat("h2", Some(reading(n, 0, GIB, &[])));
    }
    // 15 s after the last reading of h1 came: not yet past three windows.
    assert!(!view(&r, "h1").fallback);
    r.advance(5_000);
    r.beat("h1", None);
    r.beat("h2", Some(reading(6, 0, GIB, &[])));
    let stale = view(&r, "h1");
    assert!(stale.fallback);
    assert_eq!(stale.left_cpu_millis, 0);
    assert!(!view(&r, "h2").fallback);
    let listed = r.agents.registry.hosts();
    assert!(listed.iter().all(|host| host.state == HostState::Live));
    assert!(r.start(sharing_spec(1)).is_some());
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts that a host that says hello again starts its readings again: its new session's
/// readings, numbered from 1 again, are taken (an idle window where the old session was
/// busy), and its lease, booked before, stays booked and agreed on, with no rebuild. It
/// catches readings of a new session refused as going back, and a hello that drops the
/// engine's picture of the host's leases.
#[test]
fn a_returning_host_starts_its_readings_again() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "returning",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let first = r.start(sharing_spec(1)).unwrap();
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat(&first, Some(reading(seq, 8_000, GIB, &["1-1"])));
    }
    assert!(r.placer().load_of(&first).unwrap().left_cpu_millis >= 8_000);
    // Silent past the registry's grace, the host is lost and may say hello again.
    r.advance(20_000);
    r.hello(&hello_of(&first, 16_000, 32 * GIB));
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat(&first, Some(reading(seq, 0, GIB, &["1-1"])));
    }
    let back = r.placer().load_of(&first).unwrap();
    assert!(!back.fallback);
    assert_eq!(back.measured_cpu_millis, Some(0));
    let listed = r.provider.capacity();
    let on = listed.iter().find(|c| c.host.as_str() == first).unwrap();
    assert_eq!(on.leases, [LeaseId::new(1, 1)]);
    assert!(r.start(sharing_spec(2)).is_some());
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts what happens when the registry's grace (here 120 s) outlasts the engine's own
/// lease grace (60 s): a host that falls silent keeps its lease booked in the registry
/// while the engine gives it up; the engine places it nowhere by itself, and the next
/// start the registry offers that host rebuilds the engine once, after which the host's
/// lease is held again, once. It catches a given-up lease placed again by the engine
/// alone (a lease the registry never booked) and a disagreement left in place.
#[test]
fn a_lease_the_engine_gives_up_while_the_registry_keeps_it_is_rebuilt_once() {
    let text = pool(2, 8000, "grace = \"2m\"");
    let mut r = rig("grace_apart", &text, &[], &["--placement", "js-sched"]);
    r.hello(&hello_of("h1", 16_000, 32 * GIB));
    assert_eq!(r.start(share_of(1, 7_500, GIB)).as_deref(), Some("h1"));
    r.hello(&hello_of("h2", 16_000, 32 * GIB));
    for _ in 0..14 {
        r.advance(5_000);
        r.beat("h2", None);
    }
    // h1 is too full for two cores: the start is offered h2 alone, and the engine, silent
    // h1 past its grace, gives h1's lease up.
    assert_eq!(r.start(share_of(2, 2_000, GIB)).as_deref(), Some("h2"));
    assert_eq!(r.counted(), (NONE, 0, 1));
    let h1 = js_types::HostId::new("h1").unwrap();
    assert!(
        r.agents
            .registry
            .hosts()
            .iter()
            .any(|h| h.id == h1 && h.state == HostState::Live)
    );
    assert_eq!(r.start(share_of(3, 1_000, GIB)).as_deref(), Some("h1"));
    assert_eq!(r.counted(), (NONE, 1, 2));
    let listed = r.provider.capacity();
    let on = |host: &str| listed.iter().find(|c| c.host.as_str() == host).unwrap();
    assert_eq!(on("h1").leases, [LeaseId::new(1, 1), LeaseId::new(1, 3)]);
    assert_eq!(on("h2").leases, [LeaseId::new(1, 2)]);
    assert!(r.start(share_of(4, 500, GIB)).is_some());
    assert_eq!(r.counted(), (NONE, 1, 2));
}

/// Asserts that hosts the registry forgets (lost past the pool's retention with nothing
/// booked) are forgotten by the engine, so that hosts coming and going over and over keep
/// no more state than the hosts there are, and starts on the new ones agree with no
/// rebuild. It catches an engine that keeps every host it ever heard of.
#[test]
fn hosts_the_registry_forgets_leave_the_engine() {
    let text = pool(2, 8000, "lost_retention = \"1m\"");
    let mut r = rig("churn", &text, &[], &["--placement", "js-sched"]);
    for round in 0..10u64 {
        let name = format!("h{round}");
        r.hello(&hello_of(&name, 16_000, 32 * GIB));
        assert_eq!(
            r.start(sharing_spec(round + 1)).as_deref(),
            Some(name.as_str())
        );
        r.provider.stop(&LeaseId::new(1, round + 1));
        assert!(r.placer().hosts_kept() <= 2, "round {round}");
        r.advance(15_000 + 60_000 + 1_000);
        let _ = r.agents.registry.hosts();
        assert!(r.placer().load_of(&name).is_none(), "round {round}");
    }
    assert_eq!(r.placer().hosts_kept(), 0);
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts that the news of a host gone, while the engine still holds a lease there (the
/// registry forgets only a host it books nothing on, so this is the engine and the
/// registry apart), leaves the host and its lease in the engine, where the next start
/// still agrees; once the lease ends, the same news forgets the host. It catches an engine
/// that drops a host it holds a lease on, which would orphan the lease.
#[test]
fn a_host_gone_while_the_engine_holds_a_lease_there_is_kept() {
    use js_registry::Placer;
    let mut r = rig(
        "gone_held",
        &pool(1, 8000, ""),
        &["h1"],
        &["--placement", "js-sched"],
    );
    assert_eq!(r.start(sharing_spec(1)).as_deref(), Some("h1"));
    let h1 = js_types::HostId::new("h1").unwrap();
    r.placer().host_gone(&h1);
    assert!(r.placer().load_of("h1").is_some());
    assert_eq!(r.placer().hosts_kept(), 1);
    assert_eq!(r.start(sharing_spec(2)).as_deref(), Some("h1"));
    assert_eq!(r.counted(), (NONE, 0, 1));
    for sequence in 1..=2 {
        r.provider.stop(&LeaseId::new(1, sequence));
    }
    r.placer().host_gone(&h1);
    assert!(r.placer().load_of("h1").is_none());
    assert_eq!(r.placer().hosts_kept(), 0);
}

/// Asserts a restart from the lease file under nondefault settings (`--cpu-slack 1.5`, a
/// memory percent of 90): the shared and whole-host leases the file records are put back
/// before any host says hello, the first start meets them and rebuilds the engine once,
/// and from then on starts and ends agree; the slack and the percent hold after the
/// restart as before it. Restarted with `--placement registry`, the pool that shares hosts
/// is refused at start, as it always is. It catches restored leases the engine never
/// learns (it would place past them), settings lost on restart, and a rebuild at every
/// start after one.
#[test]
fn a_restart_puts_the_leases_back_and_the_engine_learns_them_once() {
    let text = shared_pool(2, 8000, "memory_schedulable_percent = 90");
    let flags = ["--placement", "js-sched", "--cpu-slack", "1.5"];
    let r = rig("restart", &text, &["h1", "h2"], &flags);
    let mut table = r.table();
    let ttl = Some(Duration::from_secs(3_600));
    let class = r.agents.registry.provider().classes()[0].clone();
    let mut whole = sharing_spec(1).request().clone();
    whole.class = class.id().clone();
    let held = table.acquire(whole, ttl).unwrap();
    let shared: Vec<_> = (0..3)
        .map(|_| {
            table
                .acquire(share_of(1, 1_000, GIB).request().clone(), ttl)
                .unwrap()
        })
        .collect();
    assert!(shared.iter().all(|lease| lease.host != held.host));
    let busy = shared[0].host.to_string();
    drop(table);
    let config = config_booted(r.dir(), &text, &flags, "resume");
    let mut r = r.restarted(config);
    let mut table = r.table();
    for host in ["h1", "h2"] {
        r.hello(&hello_of(host, 16_000, 32 * GIB));
    }
    let listed = r.agents.registry.provider().capacity();
    let leases: usize = listed.iter().map(|c| c.leases.len()).sum();
    assert_eq!(leases, 4);
    // 1.5x of 8 cores, 3 taken: 9 more one-core leases fit, a tenth does not.
    for n in 0..9 {
        let lease = table.acquire(share_of(1, 1_000, GIB).request().clone(), ttl);
        assert_eq!(lease.unwrap().host.as_str(), busy, "lease {n}");
    }
    assert!(
        table
            .acquire(share_of(1, 1_000, GIB).request().clone(), ttl)
            .is_err()
    );
    assert_eq!(r.counted(), (r.placer().fallbacks(), 1, 2));
    table.release(&shared[0].id).unwrap();
    let again = table.acquire(share_of(1, 1_000, GIB).request().clone(), ttl);
    assert_eq!(again.unwrap().host.as_str(), busy);
    let above = share_of(1, 1_000, NINETY + 1).request().clone();
    assert!(table.acquire(above, ttl).is_err());
    assert_eq!(r.placer().resynced(), 1);
    let refused = config_result(r.dir(), &text, &[], "resume").err().unwrap();
    assert!(
        refused.to_string().contains("share_class needs js-sched"),
        "{refused}"
    );
}

/// Asserts the settings through a configuration file whose joined pool sets
/// `memory_schedulable_percent = 90` and whose `[server]` sets `placement = "js-sched"` and
/// `cpu_slack = 1.5`, assembled as the server assembles it: a 10-core host takes fifteen
/// one-core leases and not a sixteenth, a lease above 90% of its memory fits no host, and
/// the registry's book keeps the whole memory. It catches a file's settings read but not
/// given to the engine.
#[test]
fn a_joined_pool_of_the_file_holds_the_engine_to_its_settings() {
    let dir = scratch("joined_settings");
    let token = write(dir.path(), "client-token", &"t".repeat(40), 0o600);
    let joins = write(dir.path(), "joins", &format!("{JOIN}\n"), 0o600);
    let quoted = |path: &std::ffi::OsString| format!("{:?}", path.to_string_lossy());
    let text = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\
         state_file = {}\nboot = \"initialise\"\nclient_token_file = {}\n\
         placement = \"js-sched\"\ncpu_slack = 1.5\n\n\
         [[pool]]\nkind = \"joined\"\nname = \"burst\"\nclass = \"burst-host\"\n\
         exec = \"none\"\nmax_machines = 2\nmemory_schedulable_percent = 90\n\
         join_token_file = {}\n\
         shape = {{ cpu_millis = 10000, memory_bytes = 17179869184 }}\n\
         capabilities = {{ arch = \"x86_64\" }}\n",
        quoted(&dir.path().join("leases.toml").into_os_string()),
        quoted(&token),
        quoted(&joins),
    );
    let path = write(dir.path(), "js.toml", &text, 0o644);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let config = crate::config::Config::from_config_file(
        std::path::Path::new(&path),
        &crate::kinds::Kinds::standard(),
        &crate::kinds::OwnerOnlyFiles,
        crate::adapt::SharedClock::new(clock.clone()),
    )
    .unwrap();
    let mut r = rig_of(config, dir, clock);
    r.hello(&hello_of("h1", 16_000, 32 * GIB));
    for sequence in 1..=15 {
        assert_eq!(r.start(share_of(sequence, 1_000, 1)).as_deref(), Some("h1"));
    }
    assert_eq!(r.start(share_of(16, 1_000, 1)), None);
    let above = r.provider.start(share_of(17, 1, NINETY + 1));
    assert_eq!(
        above,
        js_provider::StartAck::Refused(js_provider::RefusalCode::NoFit)
    );
    let listed = r.provider.capacity();
    assert_eq!(listed[0].shape.resources().memory_bytes(), 16 * GIB);
    let shown = r.agents.registry.hosts();
    assert_eq!(shown[0].shape.resources().memory_bytes(), NINETY);
}

/// Asserts that with `--usage-off` the heartbeats' readings still reach the engine and
/// choose the host: the host carrying lease 1 reports all its cores busy, the other is
/// idle, and lease 2 goes to the idle one. It catches readings fed to the engine only
/// through the usage side.
#[test]
fn with_usage_off_readings_still_choose_the_host() {
    let hosts = ["h1", "h2"];
    let flags = ["--placement", "js-sched", "--usage-off"];
    let mut r = rig("usage_off_load", &pool(2, 8000, ""), &hosts, &flags);
    assert!(r.agents.usage.is_none());
    let busy = r.start(sharing_spec(1)).unwrap();
    let idle = hosts.iter().find(|h| **h != busy).unwrap().to_string();
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat(&busy, Some(reading(seq, 8_000, 15 * GIB, &["1-1"])));
        r.beat(&idle, Some(reading(seq, 0, GIB, &[])));
    }
    assert_eq!(r.start(sharing_spec(2)).as_deref(), Some(idle.as_str()));
}

/// Asserts that starts, beats and stops from four threads at once, released together by
/// a barrier, keep the engine and the registry agreed: every start is booked on a host
/// with room, no lease is booked twice, every stop releases, nothing falls back and the
/// engine is never rebuilt. It catches state lost between the registry's lock and the
/// placer's, and a lock order that deadlocks (the test would not end).
#[test]
fn starts_beats_and_stops_from_threads_keep_the_engine_agreed() {
    let hosts = ["h1", "h2", "h3", "h4"];
    let r = rig(
        "threads",
        &pool(4, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let barrier = Barrier::new(hosts.len());
    let booked = std::sync::Mutex::new(Vec::new());
    let credential = Credential::of_token(JOIN);
    std::thread::scope(|scope| {
        for (n, host) in (0u64..).zip(hosts) {
            let mut provider = r.provider.clone();
            let agents = r.agents.clone();
            let beat = r.heartbeat(host, None);
            let (barrier, booked, credential) = (&barrier, &booked, &credential);
            scope.spawn(move || {
                barrier.wait();
                for i in 0..10 {
                    let sequence = n * 100 + i + 1;
                    let ack = provider.start(sharing_spec(sequence));
                    let on = host_of(&ack).map(str::to_owned);
                    booked.lock().unwrap().push((sequence, on.unwrap()));
                    agents.registry.heartbeat(&beat, credential).unwrap();
                    if i % 2 == 1 {
                        provider.stop(&LeaseId::new(1, sequence - 1));
                    }
                }
            });
        }
    });
    let booked = booked.into_inner().unwrap();
    assert_eq!(booked.len(), 40);
    let mut leases: Vec<u64> = booked.iter().map(|(sequence, _)| *sequence).collect();
    leases.sort_unstable();
    leases.dedup();
    assert_eq!(leases.len(), 40);
    let left: usize = r.provider.capacity().iter().map(|c| c.leases.len()).sum();
    assert_eq!(left, 20);
    assert!(r.provider.capacity().iter().all(|c| c.leases.len() <= 8));
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// `method path` with `body` and the client token of [`super::wiring_tests::config_of`],
/// through the server's router over `table` and the rig's agent side: the status and the
/// JSON body.
async fn call(
    shared: &Arc<Shared>,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let token = "t".repeat(40);
    let request = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let reply = router(shared.clone(), true)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = reply.status();
    let bytes = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Asserts, through the lease routes of a server under `--placement js-sched`, what only
/// the engine does: leases of the class that shares a host are granted (201) beside each
/// other, and the hosts' load readings choose where the next one goes (away from the host
/// whose cores are all busy). Without the engine installed in the registry such a class
/// is refused, and the registry places by no reading. It catches the server's placer not
/// given to its registry.
#[tokio::test]
async fn the_routes_place_shared_leases_by_load() {
    let hosts = ["h1", "h2"];
    let r = rig(
        "routes",
        &shared_pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let token = crate::config::parse_token("t".repeat(40).as_bytes()).unwrap();
    let shared = Arc::new(Shared::new(r.table(), token, Some(r.agents.clone())));
    let body = json!({
        "class": "share",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1 << 20 },
        "max_wall_seconds": 600,
    });
    let (status, first) = call(&shared, "POST", "/v1/leases", Some(&body)).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let busy = first["host"].as_str().unwrap().to_owned();
    let (status, second) = call(&shared, "POST", "/v1/leases", Some(&body)).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["host"], busy.as_str());
    let idle = hosts.iter().find(|h| **h != busy).unwrap().to_string();
    let ids: Vec<String> = [&first, &second]
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_owned())
        .collect();
    let covered: Vec<&str> = ids.iter().map(String::as_str).collect();
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat(&busy, Some(reading(seq, 8_000, GIB, &covered)));
        r.beat(&idle, Some(reading(seq, 0, GIB, &[])));
    }
    let (status, third) = call(&shared, "POST", "/v1/leases", Some(&body)).await;
    assert_eq!(status, StatusCode::CREATED, "{third}");
    assert_eq!(third["host"], idle.as_str());
    let (status, listed) = call(&shared, "GET", "/v1/hosts", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let entry = |host: &str| {
        let hosts = listed["hosts"].as_array().unwrap();
        hosts.iter().find(|h| h["id"] == host).unwrap().clone()
    };
    let on_busy: Vec<Value> = entry(&busy)["leases"].as_array().unwrap().clone();
    assert_eq!(on_busy, [first["id"].clone(), second["id"].clone()]);
    assert_eq!(entry(&idle)["leases"], json!([third["id"].clone()]));
    assert_eq!(entry(&idle)["shape"]["memory_bytes"], json!(16 * GIB));
}
