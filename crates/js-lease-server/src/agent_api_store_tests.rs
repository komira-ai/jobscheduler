//! The hello, the heartbeat, the cordon and the tick over a server whose bindings are kept in
//! the state store: what each commits, in how many transactions, before
//! what it changes in memory, and what a failed commit leaves. The store is the in-memory
//! fake behind a wrapper that fails the commit or the read it is told to.

use std::collections::BTreeMap;
use std::path::Path as FilePath;
use std::time::Duration;

use js_agent_wire::Taint;
use js_jobs::interfere::{INJECTED, Interfere};
use js_registry::{Binding, PoolConfig, Registry, SequentialSessions};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, Store, Txn};
use js_testkit::dir::TestDir;
use js_types::cordon::Asker;
use js_types::{ManualClock, Timestamp};
use js_types::{ProviderId, ResourceClass, Resources, Shape};
use serde_json::{Value, json};

use super::log_tests::capture;
use super::*;
use crate::agent_state::BindingStore;
use crate::agents::CordonFault;
use crate::api::Shared;
use crate::api::rig::Lines;
use crate::clock::SharedClock;
use crate::config::parse_token;

#[path = "agent_api_store_clear_tests.rs"]
mod clear;

#[path = "agent_api_store_retire_tests.rs"]
mod retire;

#[path = "agent_api_host_cert_tests.rs"]
mod host_cert;

#[path = "agent_api_judged_tests.rs"]
mod judged;

#[path = "agent_api_overlap_tests.rs"]
mod overlap;

#[path = "agent_api_taint_log_tests.rs"]
mod taint_log;

#[path = "agent_api_stale_taint_tests.rs"]
mod stale_taint;

#[path = "agent_api_resync_tests.rs"]
mod resync;

/// When a rig's clock starts, in milliseconds.
const START_MS: u64 = 5_000_000;

/// The body of a hello of `host` in the pool `pool`, naming the instance `i-1`.
fn body(host: &str, pool: &str) -> Bytes {
    let hello = json!({
        "host_id": host,
        "pool": pool,
        "agent_version": "0.1.0",
        "instance": { "provider": "lambdalabs", "instance_id": "i-1" },
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "capabilities": { "arch": "x86_64" },
        "phase": "serving",
    });
    Bytes::from(hello.to_string())
}

/// A fresh fake store behind the wrapper that fails what it is told to.
fn fake() -> Arc<Interfere<Fake>> {
    let clock = Arc::new(js_state::ManualClock::default());
    Arc::new(Interfere::new(Fake::with_flaw(clock, Flaw::None)))
}

/// The agent side of a server over the pool `burst`, its bindings kept in `store`.
struct Rig {
    agents: Arc<Agents>,
    store: Arc<Interfere<Fake>>,
    clock: ManualClock,
    dir: TestDir,
}

/// The credential the registry knows the host `host` by: the one its certificate gives.
fn who_of(host: &str) -> Credential {
    Credential::of_host_certificate(&HostId::new(host).unwrap())
}

/// The credential a binding written by a release that admitted hosts by token holds: the
/// digest of a token, which no certificate gives.
fn legacy() -> Credential {
    Credential::of_token(&"j".repeat(40))
}

/// A process-wide subscriber that takes every event and writes it nowhere, so the code of
/// each log line on a failure path runs, on whichever thread reaches it. The interest of
/// every log line is then worked out again, at each call: a line another test reached while
/// that subscriber was being installed, beside the thread-scoped subscribers of other tests,
/// can have kept an interest of never, and would then be skipped here.
fn quiet_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .try_init();
    });
    tracing::callsite::rebuild_interest_cache();
}

/// Asserts that `lines` holds exactly one line that says `message`, at `level`, with the
/// injected failure.
fn logged(lines: &Lines, level: &str, message: &str) {
    let text = lines.take();
    let said: Vec<&str> = text.lines().filter(|line| line.contains(message)).collect();
    assert_eq!(said.len(), 1, "{text}");
    for part in [level, INJECTED] {
        assert!(said[0].contains(part), "{text}");
    }
}

fn rig(name: &str) -> Rig {
    quiet_logs();
    let root = FilePath::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_store");
    let clock = ManualClock::new(Timestamp::from_millis(START_MS));
    over(TestDir::under(root, name), fake(), clock)
}

/// A rig that reads `store` as it is, as a server does at start.
fn over(dir: TestDir, store: Arc<Interfere<Fake>>, clock: ManualClock) -> Rig {
    let shared = SharedClock::new(clock.clone());
    let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".to_string())).unwrap();
    let shape = Shape::new(
        resources,
        BTreeMap::from([("arch".to_string(), "x86_64".to_string())]),
    );
    let name = ProviderId::new("burst").unwrap();
    let pool = PoolConfig::new(name, ResourceClass::gpu_host(), shape, 4);
    let registry = Registry::with_sessions(
        pool,
        clock.clone(),
        SequentialSessions::new(),
        Arc::new(js_sched_placer::place::SchedPlacer::default()),
    );
    let kept: Arc<dyn Store> = store.clone();
    let agents = Agents::over(registry, &shared, dir.path()).kept_in(kept);
    Rig {
        agents: Arc::new(agents),
        store,
        clock,
        dir,
    }
}

impl Rig {
    /// The hello of `host` in `pool` with the certificate of `host`.
    async fn hello(&self, host: &str, pool: &str) -> Result<Welcome, Refused> {
        let body = Ok(body(host, pool));
        let welcome = serve_hello(&self.agents, host, body, &|| ());
        welcome.await.map(|Json(welcome)| welcome)
    }

    /// Writes the binding of `host` as a release that admitted hosts by token wrote it: bound
    /// to the credential [`legacy`], with no row for the token.
    fn bind_by_token(&self, host: &str) {
        let binding = json!({ "version": 1, "credential": legacy().to_hex() });
        let put = Txn::new().put(
            format!("hosts/binding/{host}"),
            binding.to_string(),
            Cond::Any,
        );
        self.store.inner().txn(put).unwrap().unwrap();
    }

    /// The heartbeat of `host` in the session `welcome` opened, carrying `taint` (`null` for
    /// none), with the certificate of `host`.
    async fn beat(
        &self,
        host: &str,
        welcome: &Welcome,
        taint: Value,
    ) -> Result<HeartbeatReply, Refused> {
        let beat = json!({
            "host_id": host,
            "epoch": welcome.epoch,
            "session": welcome.session,
            "phase": "serving",
            "taint": taint,
        });
        beat_as(&self.agents, host, Bytes::from(beat.to_string())).await
    }

    /// The heartbeat of `host` that reports `held` as cleared and `taint` as new, as `beat`
    /// reports a taint.
    async fn beat_clearing(
        &self,
        host: &str,
        welcome: &Welcome,
        (held, taint): (Value, Value),
    ) -> Result<HeartbeatReply, Refused> {
        let beat = json!({
            "host_id": host,
            "epoch": welcome.epoch,
            "session": welcome.session,
            "phase": "serving",
            "taint": taint,
            "taint_cleared": held,
        });
        beat_as(&self.agents, host, Bytes::from(beat.to_string())).await
    }

    /// A second server over the store of `first`, started as a server starts, reading what
    /// the store holds.
    fn second(first: &Rig, name: &str) -> Rig {
        let root = FilePath::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_api_store");
        over(
            TestDir::under(root, name),
            Arc::clone(&first.store),
            first.clock.clone(),
        )
    }

    /// The value the store holds at `key` as JSON, read past the wrapper; `null` when the key
    /// is absent.
    fn stored(&self, key: &str) -> Value {
        let row = self.store.inner().get(key).unwrap();
        row.map_or(Value::Null, |row| {
            serde_json::from_slice(&row.value).unwrap()
        })
    }

    /// The stored binding of `host`.
    fn binding(&self, host: &str) -> Value {
        self.stored(&format!("hosts/binding/{host}"))
    }

    /// The keys the store holds under `prefix`.
    fn keys(&self, prefix: &str) -> Vec<String> {
        let rows = self.store.inner().scan_prefix(prefix).unwrap();
        rows.into_iter().map(|(key, _)| key).collect()
    }

    /// How many transactions the store has committed.
    fn commits(&self) -> u64 {
        self.store.commit_stats().txns
    }

    /// The binding the registry holds for `host`.
    fn held(&self, host: &str) -> Option<Binding> {
        let host = HostId::new(host).unwrap();
        self.agents.registry.bindings().remove(&host)
    }
}

/// The heartbeat route's answer to `body`, with the certificate of the host `host`.
async fn beat_as(agents: &Arc<Agents>, host: &str, body: Bytes) -> Result<HeartbeatReply, Refused> {
    let id = HostId::new(host).unwrap();
    let who = Extension(Credential::of_host_certificate(&id));
    let certified = Extension(CertifiedHost(id));
    let agents = State(Arc::clone(agents));
    let reply = super::heartbeat(agents, who, certified, None, Ok(body)).await;
    reply.map(|Json(reply)| reply)
}

/// The server state the host book is read from, over `rig`'s agent side.
fn shared_over(rig: &Rig) -> Shared {
    let token = parse_token(&[b'q'; 40]).unwrap();
    Shared::new(token, Arc::clone(&rig.agents))
}

/// Asserts that the first hello of a host commits its binding alone, before the registry takes
/// the hello: when that commit fails (once, by the store's fault hook) the hello is refused 503
/// `state-write-failed`, the store holds no binding and the registry holds nothing; once the
/// fault is gone the same hello is admitted with exactly one more commit, which holds the
/// binding of the host's certificate. It catches a binding committed after the registry took the
/// hello (a crash between would lose it), and a failed commit that changes memory.
#[tokio::test]
async fn hello_binds_the_host_in_one_commit() {
    let rig = rig("one_commit");
    let before = rig.commits();
    rig.store.fail_next();
    let failed = rig.hello("host-a", "burst").await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.commits(), before);
    assert_eq!(rig.binding("host-a"), Value::Null);
    assert!(rig.held("host-a").is_none());

    let admitted = rig.hello("host-a", "burst").await;
    admitted.expect("the same hello is admitted once the fault is gone");
    assert_eq!(rig.commits(), before + 1, "the binding in one commit");
    let binding = rig.binding("host-a");
    assert_eq!(binding["version"], 1);
    assert_eq!(binding["credential"], who_of("host-a").to_hex().as_str());
    assert_eq!(binding["instance_provider"], "lambdalabs");
    assert_eq!(binding["instance_id"], "i-1");
    assert_eq!(rig.keys("hosts/"), ["hosts/binding/host-a".to_owned()]);
}

/// Asserts that a heartbeat that brings a taint has it committed with the host's binding
/// before the registry records it: when the commit fails the heartbeat is answered 503
/// `state-write-failed` with a warning naming the failure, the registry holds no taint and
/// the store none; the beat repeated is acknowledged, and both then hold the taint. It
/// catches a taint acknowledged before it is durable (the agent stops reporting it, and a
/// restart would make the host clean) and a failed commit that leaves the taint in memory.
#[tokio::test]
async fn a_heartbeat_taint_is_committed_before_it_is_acknowledged() {
    let rig = rig("taint");
    let welcome = rig.hello("host-a", "burst").await.unwrap();
    let taint = json!({ "reason": "residue", "check": "tmp-files" });
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.beat("host-a", &welcome, taint.clone());
    let failed = failed.await.expect_err("the commit fails");
    drop(capture);
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    logged(&lines, "WARN", "a heartbeat's taint could not be committed");
    assert!(rig.held("host-a").unwrap().taint().is_none());
    assert_eq!(rig.binding("host-a")["taint_reason"], Value::Null);

    let reply = rig.beat("host-a", &welcome, taint).await;
    assert!(reply.unwrap().taint_ack);
    let binding = rig.binding("host-a");
    assert_eq!(binding["taint_reason"], "residue");
    assert_eq!(binding["taint_check"], "tmp-files");
    let held = rig.held("host-a").unwrap();
    assert_eq!(
        held.taint().map(|taint| taint.reason.as_str()),
        Some("residue")
    );
}

/// Asserts that a cordon and its lift are committed before the registry takes them: a
/// cordon whose commit fails is `Unsaved`, logged as an error naming the failure, and
/// leaves the host uncordoned in the registry and in the store; one that commits holds in
/// both, until the lease's end: on the server's clock in the registry, and on the store's in
/// the binding, which another server over the store reads (the two clocks are apart here: the
/// store's starts at its origin); a lift whose commit fails leaves the cordon in both, and one
/// that commits lifts it in both. A host the registry does not hold is `NotHeld` either way.
/// It catches a cordon or a lift that takes effect before it is durable, so a restart would
/// undo it, and a binding that stores the server's clock for the store's.
#[tokio::test]
async fn cordon_and_uncordon_commit_before_they_take_effect() {
    let rig = rig("cordon");
    rig.hello("host-a", "burst").await.unwrap();
    let host = HostId::new("host-a").unwrap();
    let lease = Duration::from_secs(60);
    let cordon_until = |rig: &Rig| rig.agents.registry.own_host(&host).unwrap().cordon_until;
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.agents.cordon(&host, lease, Asker::Unnamed).await;
    drop(capture);
    assert_eq!(failed.err(), Some(CordonFault::Unsaved));
    logged(&lines, "ERROR", "cannot commit the cordon of a host");
    assert_eq!(cordon_until(&rig), None);
    assert_eq!(rig.binding("host-a")["cordon_until_ms"], Value::Null);

    let store_ms = js_state::Store::now(&*rig.store)
        .unwrap()
        .as_duration()
        .as_millis();
    assert!(
        rig.agents
            .cordon(&host, lease, Asker::Unnamed)
            .await
            .unwrap()
            .placed
    );
    let stored = u64::try_from(store_ms).unwrap() + 60_000;
    let until = START_MS + 60_000;
    assert_ne!(
        stored, until,
        "the store's clock and the server's are apart"
    );
    assert_eq!(rig.binding("host-a")["cordon_until_ms"], stored);
    assert_eq!(cordon_until(&rig), Some(Timestamp::from_millis(until)));

    rig.store.fail_next();
    let failed = rig.agents.uncordon(&host, None).await;
    assert_eq!(failed.err(), Some(CordonFault::Unsaved));
    assert_eq!(rig.binding("host-a")["cordon_until_ms"], stored);
    assert_eq!(cordon_until(&rig), Some(Timestamp::from_millis(until)));

    assert!(rig.agents.uncordon(&host, None).await.unwrap().lifted);
    assert_eq!(rig.binding("host-a")["cordon_until_ms"], Value::Null);
    assert_eq!(cordon_until(&rig), None);

    let ghost = HostId::new("ghost").unwrap();
    let unknown = rig.agents.cordon(&ghost, lease, Asker::Unnamed).await;
    assert_eq!(unknown.err(), Some(CordonFault::NotHeld));
    let unknown = rig.agents.uncordon(&ghost, None).await;
    assert_eq!(unknown.err(), Some(CordonFault::NotHeld));
}

/// Asserts that a cordon whose store cannot read its clock (its database stopped) is
/// `Unsaved` and takes effect nowhere: the binding's end would be put on the store's clock by
/// that reading, so nothing is committed and the registry holds no cordon. Once the clock
/// reads again the cordon is placed. It catches a cordon end stored by a skew the store did not
/// give (one a second server would read as lapsed, or as holding for hours).
#[tokio::test]
async fn a_cordon_whose_store_cannot_read_its_clock_is_unsaved() {
    let rig = rig("cordon_unclocked");
    rig.hello("host-a", "burst").await.unwrap();
    let host = HostId::new("host-a").unwrap();
    let lease = Duration::from_secs(60);
    rig.store.fail_clock(true);
    let commits = rig.store.commit_stats().txns;
    let failed = rig.agents.cordon(&host, lease, Asker::Unnamed).await;
    assert_eq!(failed.err(), Some(CordonFault::Unsaved));
    assert_eq!(rig.store.commit_stats().txns, commits);
    assert_eq!(rig.binding("host-a")["cordon_until_ms"], Value::Null);
    let own = rig.agents.registry.own_host(&host).unwrap();
    assert_eq!(own.cordon_until, None);

    rig.store.fail_clock(false);
    let placed = rig.agents.cordon(&host, lease, Asker::Unnamed).await;
    assert!(placed.unwrap().placed);
}

/// Asserts that the hot path reads and writes no store: with the store's next commit and
/// next read both set to fail, a heartbeat, a read of the host book and the host's read of
/// itself all succeed, and the commit and the read are still set to fail afterwards. (A hello
/// reads its host's binding row, which it is judged on: a known host's hello is
/// `a_known_hosts_hello_reads_its_binding_row_and_writes_nothing`.) It catches a store call on
/// any of them: the call would have been the one to fail, or would have used the failure up.
#[tokio::test]
async fn heartbeat_and_host_book_reads_make_no_store_call() {
    let rig = rig("no_calls");
    rig.hello("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(60));
    let shared = shared_over(&rig);
    let welcome = rig.hello("host-a", "burst").await;
    let welcome = welcome.expect("a known host's hello");
    rig.store.fail_after(0);
    rig.store.fail_read_after(0);
    let reply = rig.beat("host-a", &welcome, Value::Null).await;
    assert!(!reply.expect("a heartbeat").taint_ack);
    let book = crate::hosts::listing(&shared);
    let listed = serde_json::to_value(&book).unwrap();
    assert!(listed.to_string().contains("host-a"), "{listed}");
    let target = axum::extract::Path("host-a".to_owned());
    let certified = Extension(CertifiedHost(HostId::new("host-a").unwrap()));
    let shown = own_host::show(State(Arc::clone(&rig.agents)), certified, target).await;
    assert!(shown.is_ok());

    assert!(rig.store.get("probe").is_err(), "no read reached the store");
    let probe = Txn::new().put("probe", "v", Cond::Any);
    assert!(rig.store.txn(probe).is_err(), "no commit reached the store");
}

/// Asserts that hellos racing at one server are taken one at a time: each runs its seam between
/// the binding's commit and the registry taking the hello with the host writes' gate held, and
/// all four hosts are admitted, each with the binding of its own certificate. The hellos are
/// polled together on one task, so each that waits for the gate is parked at a known point, and
/// nothing waits on timing. It catches a hello path that lets a second hello in between the
/// judgement and the registry taking the hello.
#[tokio::test]
async fn concurrent_hellos_are_taken_one_at_a_time() {
    let rig = rig("concurrent");
    let agents = &rig.agents;
    let alone = move || {
        let held = agents.host_writes.try_lock().is_err();
        assert!(held, "the seam runs with the gate held");
    };
    let hello = |host: &'static str| {
        let body = Ok(body(host, "burst"));
        serve_hello(agents, host, body, &alone)
    };
    let (a, b, c, d) = tokio::join!(
        hello("host-0"),
        hello("host-1"),
        hello("host-2"),
        hello("host-3")
    );
    for answer in [a, b, c, d] {
        let Json(welcome) = answer.expect("every host is admitted");
        assert_eq!(welcome.epoch, 1, "each host's first session");
    }
    let bound = rig.keys("hosts/binding/");
    assert_eq!(bound.len(), 4);
    for host in ["host-0", "host-1", "host-2", "host-3"] {
        let credential = rig.binding(host)["credential"].clone();
        assert_eq!(credential, who_of(host).to_hex().as_str());
    }
}

/// Asserts that a hello commits the host's binding alone before the registry is asked: when the
/// commit fails it is refused 503 `state-write-failed`, with a warning naming the failure, and
/// the registry holds nothing; a hello the registry refuses commits nothing (it is judged before
/// the binding is written); an admitted one leaves the binding to the certificate's digest. It
/// catches a binding left to the file-mode save after the hello (a crash in between would lose
/// it) and a binding written for a host the registry refused.
#[tokio::test]
async fn a_hello_commits_its_binding_alone_and_none_when_refused() {
    let rig = rig("pooled");
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.hello("host-a", "burst").await;
    drop(capture);
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    logged(
        &lines,
        "WARN",
        "agent hello refused: the binding could not be committed",
    );
    assert!(rig.held("host-a").is_none());

    let before = rig.commits();
    let refused = rig.hello("host-b", "elsewhere").await;
    assert_eq!(refused.expect_err("another pool").status.as_u16(), 403);
    assert_eq!(rig.commits(), before);
    assert_eq!(rig.binding("host-b"), Value::Null);

    rig.hello("host-a", "burst").await.expect("admitted");
    assert_eq!(
        rig.binding("host-a")["credential"],
        who_of("host-a").to_hex().as_str()
    );
}

/// Asserts that a host that said hello before a restart over a store that no longer holds its
/// binding (the tick dropped it once the registry forgot the host) has the binding committed
/// alone before the registry takes the hello: a failed commit refuses the hello 503
/// `state-write-failed` with nothing held, and the hello repeated is admitted with one commit
/// that holds the binding. It catches a binding taken by the registry with no commit.
#[tokio::test]
async fn a_hello_whose_binding_is_gone_commits_the_binding_alone() {
    let rig = rig("again_rebound");
    rig.hello("host-a", "burst").await.unwrap();
    let gone = Txn::new().delete("hosts/binding/host-a", Cond::Any);
    rig.store.inner().txn(gone).unwrap().unwrap();
    let Rig {
        store, clock, dir, ..
    } = rig;
    let rig = over(dir, store, clock);

    rig.store.fail_next();
    let failed = rig.hello("host-a", "burst").await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    assert!(rig.held("host-a").is_none());
    let before = rig.commits();
    rig.hello("host-a", "burst").await.expect("admitted again");
    assert_eq!(rig.commits(), before + 1);
    let credential = rig.binding("host-a")["credential"].clone();
    assert_eq!(credential, who_of("host-a").to_hex().as_str());
}

/// Asserts that once the registry forgets a silent host the tick drops its binding, and that a
/// tick whose commit fails counts an error and leaves the binding for the next. It catches the
/// binding of a forgotten host kept forever.
#[tokio::test]
async fn the_tick_drops_the_bindings_of_forgotten_hosts() {
    let rig = rig("tick");
    rig.hello("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(28 * 3600));
    rig.store.fail_next();
    rig.agents.refresh().await;
    assert_eq!(rig.agents.errors(), 1);
    assert_ne!(rig.binding("host-a"), Value::Null);
    rig.agents.refresh().await;
    assert!(rig.held("host-a").is_none());
    assert_eq!(rig.binding("host-a"), Value::Null);
}

/// Asserts that the stored hosts keep what a newer build wrote: a binding with a higher
/// version and a field this build does not know is read, and the next write of the key writes
/// the field back (with this build's version). It catches a write that drops a newer build's
/// data, and a reader that refuses a newer version.
#[test]
fn stored_hosts_keep_the_fields_a_newer_build_wrote() {
    let store = fake();
    let digest = Credential::of_token("t").to_hex();
    let binding = json!({ "version": 2, "credential": digest, "later": [1] });
    let txn = Txn::new().put("hosts/binding/host-a", binding.to_string(), Cond::Any);
    store.txn(txn).unwrap().unwrap();
    let kept: Arc<dyn Store> = store.clone();

    let (bindings, read) =
        BindingStore::in_store(Arc::clone(&kept), crate::agent_state::epoch_clock()).unwrap();
    let host = HostId::new("host-a").unwrap();
    let bound = Binding::new(Credential::of_token("t"), None);
    assert_eq!(read, BTreeMap::from([(host.clone(), bound.clone())]));
    let cordoned = BTreeMap::from([(host, bound.with_cordon(Some(7)))]);
    // The server's clock is at the epoch: the end is written on the store's, as far past the
    // store's reading as it is past the server's.
    let store_ms = u64::try_from(store.now().unwrap().as_duration().as_millis()).unwrap();
    bindings.save(|| cordoned, &|_| false).unwrap();
    let written = store.get("hosts/binding/host-a").unwrap().unwrap().value;
    let written: Value = serde_json::from_slice(&written).unwrap();
    let expected = json!({
        "version": 1,
        "credential": digest,
        "cordon_until_ms": store_ms + 7,
        "later": [1],
    });
    assert_eq!(written, expected);
}

/// Asserts that a stored value that cannot be read as written refuses the read with a
/// reason naming its key and what is wrong, as a damaged file is refused: a binding that is not
/// the key's JSON, a binding whose credential is no digest or whose key is no host id, and a
/// store that cannot be read. It catches a start that reads a damaged store as fewer bindings
/// (a booked host's binding lost).
#[test]
fn a_stored_host_that_cannot_be_read_as_written_refuses_the_read() {
    let digest = Credential::of_token("t").to_hex();
    let cases = [
        (
            "hosts/binding/host-a",
            "{}".to_owned(),
            "hosts/binding/host-a does not hold this key's fields",
        ),
        (
            "hosts/binding/host-a",
            json!({ "version": 1, "credential": "abc" }).to_string(),
            "hosts/binding/host-a: a credential is not 64 lower-case hex digits",
        ),
        (
            "hosts/binding/Bad Id",
            json!({ "version": 1, "credential": digest }).to_string(),
            "hosts/binding/Bad Id: a host id is not valid",
        ),
    ];
    for (key, value, reason) in cases {
        let store = fake();
        store
            .txn(Txn::new().put(key, value, Cond::Any))
            .unwrap()
            .unwrap();
        let kept: Arc<dyn Store> = store;
        let bindings = BindingStore::in_store(kept, crate::agent_state::epoch_clock()).err();
        assert_eq!(bindings.as_deref(), Some(reason), "{key}");
    }

    let store = fake();
    store.fail_read_after(0);
    let failed = BindingStore::in_store(store.clone(), crate::agent_state::epoch_clock())
        .err()
        .unwrap();
    assert!(failed.contains(js_jobs::interfere::INJECTED), "{failed}");
}

/// Asserts that two servers over one store cannot lose a taint write to each other. Both
/// hold the host with no taint. The second is told taint `a` and commits it. The first, which
/// still holds none, is then told taint `b`: it is refused 503 `state-write-failed`, the
/// store keeps `a`, and the first takes `a` from the store (a report must not overwrite a
/// taint the store holds, and the refused server is put level with the store). The store is
/// then given `b` (as the first's own beat would leave it); the second, which still holds
/// `a`, is told `a` is cleared: refused the same way, the store keeps `b`, the second takes
/// `b` and nothing is acknowledged (a clear must not erase a newer taint). It catches the host
/// binding's unconditional write, which let either lose.
#[tokio::test]
async fn two_servers_do_not_overwrite_each_others_taint() {
    let first = rig("two_first");
    let welcome = first.hello("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "two_second");
    let second_welcome = second.hello("host-a", "burst").await.unwrap();
    let a = json!({ "reason": "residue", "check": "daemonised-child", "run_id": "r1" });
    let b = json!({ "reason": "residue", "check": "process-as-job-user", "run_id": "r2" });

    let reply = second.beat("host-a", &second_welcome, a.clone());
    assert!(reply.await.unwrap().taint_ack);
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");
    let commits = first.commits();
    let late = first.beat("host-a", &welcome, b.clone());
    let late = late
        .await
        .expect_err("the store holds a taint the server does not");
    assert_eq!(
        (late.status.as_u16(), late.error),
        (503, "state-write-failed")
    );
    assert_eq!(first.commits(), commits, "nothing was written");
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");
    let held = first.held("host-a").unwrap();
    assert_eq!(held.taint().unwrap().run_id.as_deref(), Some("r1"));

    let newer = first.binding("host-a");
    let mut newer = newer.as_object().unwrap().clone();
    newer.insert("taint_run_id".to_owned(), json!("r2"));
    newer.insert("taint_check".to_owned(), json!("process-as-job-user"));
    let put = Txn::new().put(
        "hosts/binding/host-a",
        Value::Object(newer).to_string(),
        Cond::Any,
    );
    first.store.inner().txn(put).unwrap().unwrap();
    let commits = first.commits();
    let stale = second.beat_clearing("host-a", &second_welcome, (a, Value::Null));
    let stale = stale.await.expect_err("the store holds a newer taint");
    assert_eq!(
        (stale.status.as_u16(), stale.error),
        (503, "state-write-failed")
    );
    assert_eq!(first.commits(), commits, "nothing was written");
    assert_eq!(first.binding("host-a")["taint_run_id"], "r2");
    let held = second.held("host-a").unwrap();
    assert_eq!(held.taint().unwrap().run_id.as_deref(), Some("r2"));
    drop(b);
}

/// Asserts that a taint commit refuses what it cannot check against the store: a binding
/// another server rewrote between the read and the commit (the write is conditional on the
/// version read), a binding the
/// store no longer holds (another server dropped it) and a read that fails are errors that
/// write nothing, and a value that is not JSON of a binding is an error naming its key, and a binding with a
/// bad credential is the error a file entry gives. It catches a
/// commit that puts the taint over a binding it could not read.
#[tokio::test]
async fn a_taint_commit_refuses_what_it_cannot_read() {
    let rig = rig("unreadable");
    rig.hello("host-a", "burst").await.unwrap();
    let host = HostId::new("host-a").unwrap();
    let taint = Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: Some("daemonised-child".to_owned()),
    };
    let held = rig.held("host-a").unwrap();
    let change = rig.agents.store.change(&host, held.with_taint(Some(taint)));
    let name = "hosts/binding/host-a";

    rig.store.fail_read_after(0);
    let failed = rig.agents.store.commit_taint(&change, None).unwrap_err();
    assert!(failed.to_string().contains(INJECTED), "{failed}");

    let row = rig.store.inner().get(name).unwrap().unwrap();
    let other = rig.store.inner();
    let (value, version) = (row.value.clone(), row.version);
    rig.store.before_next(move || {
        let bump = Txn::new().put(name, value, Cond::Version(version));
        other.txn(bump).unwrap().unwrap();
    });
    let raced = rig.agents.store.commit_taint(&change, None).unwrap_err();
    assert_eq!(
        raced.to_string(),
        "the store refused the write of hosts/binding/host-a"
    );
    assert_eq!(rig.binding("host-a")["taint_reason"], Value::Null);

    let garbage = Txn::new().put(name, "not json", Cond::Any);
    rig.store.inner().txn(garbage).unwrap().unwrap();
    let damaged = rig.agents.store.commit_taint(&change, None).unwrap_err();
    assert_eq!(
        damaged.to_string(),
        "hosts/binding/host-a does not hold this key's fields"
    );

    let odd = Txn::new().put(name, r#"{"version":1,"credential":"zz"}"#, Cond::Any);
    rig.store.inner().txn(odd).unwrap().unwrap();
    let odd = rig.agents.store.commit_taint(&change, None).unwrap_err();
    assert_eq!(
        odd.to_string(),
        "a credential is not 64 lower-case hex digits"
    );

    let gone = Txn::new().delete(name, Cond::Any);
    rig.store.inner().txn(gone).unwrap().unwrap();
    let missing = rig.agents.store.commit_taint(&change, None).unwrap_err();
    assert_eq!(
        missing.to_string(),
        "hosts/binding/host-a is not in the store"
    );
    assert_eq!(rig.binding("host-a"), Value::Null, "nothing was written");
}

/// Asserts that a store holding `hosts/join/` rows (the join tokens and host credentials of a
/// release that admitted hosts by token, one of them damaged) is served: a hello with a
/// certificate, a heartbeat and the tick work as over a store without them, and every row of the
/// prefix is left as it was, with the same bytes and the same version. It catches a server
/// that reads the prefix (the damaged row would refuse its start), or writes it (a release
/// before this one would find its tokens changed).
#[tokio::test]
async fn a_store_holding_join_rows_is_served_and_the_rows_are_left() {
    let rig = rig("join_rows");
    let digest = Credential::of_token("t").to_hex();
    let row = json!({
        "version": 1,
        "provider": "lambdalabs",
        "instance_id": "i-1",
        "expires_at_ms": 1u64,
    });
    let txn = Txn::new()
        .put(format!("hosts/join/{digest}"), row.to_string(), Cond::Any)
        .put("hosts/join/damaged", "not json", Cond::Any);
    rig.store.inner().txn(txn).unwrap().unwrap();
    let read = |rig: &Rig| rig.store.inner().scan_prefix("hosts/join/").unwrap();
    let before = read(&rig);
    assert_eq!(before.len(), 2);

    let Rig {
        store, clock, dir, ..
    } = rig;
    let rig = over(dir, store, clock);
    let welcome = rig.hello("host-a", "burst").await.unwrap();
    rig.beat("host-a", &welcome, Value::Null).await.unwrap();
    rig.agents.refresh().await;
    assert!(rig.held("host-a").is_some());
    assert_eq!(read(&rig), before);
}
