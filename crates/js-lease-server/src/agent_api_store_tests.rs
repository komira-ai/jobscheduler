//! The hello, the heartbeat, the cordon and the tick over a server whose bindings and join
//! tokens are kept in the state store: what each commits, in how many transactions, before
//! what it changes in memory, and what a failed commit leaves. The store is the in-memory
//! fake behind a wrapper that fails the commit or the read it is told to.

use std::collections::BTreeMap;
use std::path::Path as FilePath;
use std::time::Duration;

use js_agent_wire::{InstanceIdentity, Taint};
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
use crate::join_tokens::JoinTokens;

#[path = "agent_api_store_clear_tests.rs"]
mod clear;

#[path = "agent_api_host_cert_tests.rs"]
mod host_cert;

#[path = "agent_api_overlap_tests.rs"]
mod overlap;

/// When a rig's clock starts, in milliseconds.
const START_MS: u64 = 5_000_000;

fn instance() -> InstanceIdentity {
    InstanceIdentity {
        provider: "lambdalabs".to_string(),
        instance_id: "i-1".to_string(),
    }
}

/// The body of a hello of `host` in the pool `pool`, naming the instance `i-1`, that asks
/// for a host credential when `asking`.
fn body(host: &str, pool: &str, asking: bool) -> Bytes {
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
        "accepts_host_credential": asking,
    });
    Bytes::from(hello.to_string())
}

/// A fresh fake store behind the wrapper that fails what it is told to.
fn fake() -> Arc<Interfere<Fake>> {
    let clock = Arc::new(js_state::ManualClock::default());
    Arc::new(Interfere::new(Fake::with_flaw(clock, Flaw::None)))
}

/// The agent side of a server over the pool `burst`, its bindings and join tokens kept in
/// `store`.
struct Rig {
    agents: Arc<Agents>,
    store: Arc<Interfere<Fake>>,
    clock: ManualClock,
    /// The digest of the pool's join token.
    pool: Credential,
    dir: TestDir,
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
    let pool = agents.tokens.identify(&"j".repeat(40)).unwrap();
    Rig {
        agents: Arc::new(agents),
        store,
        clock,
        pool,
        dir,
    }
}

impl Rig {
    /// Mints a join token for the instance `i-1` and returns its digest.
    fn mint(&self) -> Credential {
        let ttl = Duration::from_secs(600);
        let minted = self.agents.joins.mint(instance(), ttl).unwrap();
        Credential::of_token(&minted.token)
    }

    /// The hello of `host` in `pool` with the join token whose digest is `who`.
    async fn hello(&self, who: &Credential, host: &str, pool: &str) -> Result<Welcome, Refused> {
        let body = Ok(body(host, pool, false));
        let welcome = serve_hello(&self.agents, who.clone(), Some(who.clone()), body, &|| ());
        welcome.await.map(|Json(welcome)| welcome)
    }

    /// The hello of `host` in `pool` with the pool's join token.
    async fn pooled(&self, host: &str, pool: &str) -> Result<Welcome, Refused> {
        let body = Ok(body(host, pool, false));
        let welcome = serve_hello(&self.agents, self.pool.clone(), None, body, &|| ());
        welcome.await.map(|Json(welcome)| welcome)
    }

    /// The heartbeat of `host` in the session `welcome` opened, carrying `taint` (`null` for
    /// none), authenticated as `who` and, for a one-time token, `one_time`.
    async fn beat(
        &self,
        (who, one_time): (&Credential, Option<Credential>),
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
        let one_time = one_time.map(|presented| Extension(OneTime(presented)));
        let agents = State(Arc::clone(&self.agents));
        let body = Ok(Bytes::from(beat.to_string()));
        let reply = super::heartbeat(agents, Extension(who.clone()), (one_time, None), body).await;
        reply.map(|Json(reply)| reply)
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
        let agents = State(Arc::clone(&self.agents));
        let body = Ok(Bytes::from(beat.to_string()));
        let reply =
            super::heartbeat(agents, Extension(self.pool.clone()), (None, None), body).await;
        reply.map(|Json(reply)| reply)
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

    /// The stored row of the token or host credential whose digest is `who`.
    fn row(&self, who: &Credential) -> Value {
        self.stored(&format!("hosts/join/{}", who.to_hex()))
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

/// The server state the host book is read from, over `rig`'s agent side.
fn shared_over(rig: &Rig) -> Shared {
    let token = parse_token(&[b'q'; 40]).unwrap();
    Shared::new(token, Arc::clone(&rig.agents))
}

/// Asserts that the first hello with a join token commits the token's use and the host's
/// binding in one transaction, before the registry is asked: when that commit fails (once,
/// by the store's fault hook) the hello is refused 503 `join-token-unsaved`, the store holds
/// the token unused and no binding, and memory agrees (the token serves no host, the
/// registry holds nothing); once the fault is gone the same hello is admitted with exactly
/// one more commit, which holds both. It catches a use and a binding written in two
/// commits (a crash between them would leave one without the other), and a failed commit
/// that changes memory.
#[tokio::test]
async fn hello_consumes_the_token_and_binds_the_host_in_one_commit() {
    let rig = rig("one_commit");
    let who = rig.mint();
    let before = rig.commits();
    rig.store.fail_next();
    let failed = rig.hello(&who, "host-a", "burst").await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "join-token-unsaved")
    );
    assert_eq!(rig.commits(), before);
    assert_eq!(rig.row(&who)["used_by"], Value::Null);
    assert_eq!(rig.binding("host-a"), Value::Null);
    let unused = rig.agents.joins.serves(&who, "host-a");
    assert_eq!(unused, Err(JoinRefusal::NotAdmitted));
    assert!(rig.held("host-a").is_none());

    let admitted = rig.hello(&who, "host-a", "burst").await;
    admitted.expect("the same hello is admitted once the fault is gone");
    assert_eq!(
        rig.commits(),
        before + 1,
        "the use and the binding in one commit"
    );
    assert_eq!(rig.row(&who)["used_by"], "host-a");
    let binding = rig.binding("host-a");
    assert_eq!(binding["version"], 1);
    assert_eq!(binding["credential"], who.to_hex().as_str());
    assert_eq!(binding["instance_provider"], "lambdalabs");
    assert_eq!(binding["instance_id"], "i-1");
    assert_eq!(rig.agents.joins.serves(&who, "host-a"), Ok(()));
    assert_eq!(
        rig.keys("hosts/"),
        [
            "hosts/binding/host-a".to_owned(),
            format!("hosts/join/{}", who.to_hex())
        ]
    );
}

/// Asserts that a hello the registry refuses after the token's use was committed (it names
/// another pool) is answered with the registry's refusal, and its compensating transaction
/// gives the token back and takes the binding away: two commits in all, the stored row
/// unused, no binding, the token serving no host, and the token then registers a host of
/// the pool. A hello that asked for a host credential gets the token back too, with no
/// credential row left. It catches a refused hello that leaves the token consumed (an
/// instance stranded until its boot time runs out) or the binding of a host the registry
/// never admitted.
#[tokio::test]
async fn a_registry_refused_hello_returns_the_token_by_compensation() {
    let rig = rig("compensated");
    let who = rig.mint();
    let before = rig.commits();
    let refused = rig.hello(&who, "host-a", "elsewhere").await;
    let refused = refused.expect_err("the registry refuses the pool");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, Refusal::UnknownPool.code())
    );
    assert_eq!(rig.commits(), before + 2, "the use and its compensation");
    assert_eq!(rig.row(&who)["used_by"], Value::Null);
    assert_eq!(rig.binding("host-a"), Value::Null);
    let unused = rig.agents.joins.serves(&who, "host-a");
    assert_eq!(unused, Err(JoinRefusal::NotAdmitted));

    let asking = Ok(body("host-b", "elsewhere", true));
    let asked = serve_hello(&rig.agents, who.clone(), Some(who.clone()), asking, &|| ()).await;
    asked.expect_err("the registry refuses the pool again");
    let token_row = format!("hosts/join/{}", who.to_hex());
    assert_eq!(rig.keys("hosts/"), [token_row]);
    assert_eq!(rig.row(&who)["used_by"], Value::Null);

    rig.hello(&who, "host-a", "burst")
        .await
        .expect("the token is good");
    assert_eq!(rig.row(&who)["used_by"], "host-a");
}

/// Asserts what a failed compensation leaves (its commit fails after the use was
/// committed): the token consumed by the host and the host's binding, both in the store and
/// in memory (the token serves that host and refuses another as used). Both or neither
/// holds; the host joins again with a new token for its instance, and its binding then
/// moves to that token. It catches a failed compensation that gives the token back in
/// memory alone (a restart would take it away again) or drops one of the two.
#[tokio::test]
async fn a_failed_compensation_leaves_the_binding_and_the_consumed_token() {
    let rig = rig("compensation_failed");
    let who = rig.mint();
    rig.store.fail_after(1);
    let refused = rig.hello(&who, "host-a", "elsewhere").await;
    assert_eq!(refused.expect_err("refused").status.as_u16(), 403);
    assert_eq!(rig.row(&who)["used_by"], "host-a");
    assert_eq!(rig.binding("host-a")["credential"], who.to_hex().as_str());
    assert_eq!(rig.agents.joins.serves(&who, "host-a"), Ok(()));
    let other = rig.hello(&who, "host-b", "burst").await;
    let other = other.expect_err("the token is the host's");
    assert_eq!(
        (other.status.as_u16(), other.error),
        (403, "join-token-used")
    );

    let fresh = rig.mint();
    rig.hello(&fresh, "host-a", "burst")
        .await
        .expect("the host joins again");
    assert_eq!(rig.binding("host-a")["credential"], fresh.to_hex().as_str());
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
    let welcome = rig.pooled("host-a", "burst").await.unwrap();
    let taint = json!({ "reason": "residue", "check": "tmp-files" });
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.beat((&rig.pool, None), "host-a", &welcome, taint.clone());
    let failed = failed.await.expect_err("the commit fails");
    drop(capture);
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    logged(&lines, "WARN", "a heartbeat's taint could not be committed");
    assert!(rig.held("host-a").unwrap().taint().is_none());
    assert_eq!(rig.binding("host-a")["taint_reason"], Value::Null);

    let reply = rig.beat((&rig.pool, None), "host-a", &welcome, taint).await;
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
    rig.pooled("host-a", "burst").await.unwrap();
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
    rig.pooled("host-a", "burst").await.unwrap();
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
/// next read both set to fail, a known host's hello (after a silence past its grace, with
/// the token that registered it), a heartbeat, a read of the host book and the host's read
/// of itself all succeed, and the commit and the read are still set to fail afterwards. It
/// catches a store call on any of them: the call would have been the one to fail, or would
/// have used the failure up.
#[tokio::test]
async fn heartbeat_and_host_book_reads_make_no_store_call() {
    let rig = rig("no_calls");
    let who = rig.mint();
    rig.hello(&who, "host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(60));
    let shared = shared_over(&rig);
    rig.store.fail_after(0);
    rig.store.fail_read_after(0);

    let welcome = rig.hello(&who, "host-a", "burst").await;
    let welcome = welcome.expect("a known host's hello");
    let one_time = (&who, Some(who.clone()));
    let reply = rig.beat(one_time, "host-a", &welcome, Value::Null).await;
    assert!(!reply.expect("a heartbeat").taint_ack);
    let book = crate::hosts::listing(&shared);
    let listed = serde_json::to_value(&book).unwrap();
    assert!(listed.to_string().contains("host-a"), "{listed}");
    let target = axum::extract::Path("host-a".to_owned());
    let presented = Some(Extension(OneTime(who.clone())));
    let shown = own_host::show(State(Arc::clone(&rig.agents)), (presented, None), target).await;
    assert!(shown.is_ok());

    assert!(rig.store.get("probe").is_err(), "no read reached the store");
    let probe = Txn::new().put("probe", "v", Cond::Any);
    assert!(rig.store.txn(probe).is_err(), "no commit reached the store");
}

/// Asserts that hellos racing for one fresh token are taken one at a time and exactly one
/// is admitted: each runs its seam between the use and the registry with the host writes'
/// gate held, the others are refused 403 `join-token-used`, the stored row names the
/// winner and the store holds the winner's binding alone. The hellos are polled together on
/// one task, so each that waits for the gate is parked at a known point, and nothing waits
/// on timing. It catches a hello path that lets a second hello in between the use and the
/// registry's answer (where its compensation could undo what the other did), and two hosts
/// registered with one token.
#[tokio::test]
async fn concurrent_hellos_with_one_token_admit_exactly_one() {
    let rig = rig("concurrent");
    let who = rig.mint();
    let agents = &rig.agents;
    let alone = move || {
        let held = agents.host_writes.try_lock().is_err();
        assert!(held, "the seam runs with the gate held");
    };
    let hello = |host: &'static str| {
        let body = Ok(body(host, "burst", false));
        serve_hello(agents, who.clone(), Some(who.clone()), body, &alone)
    };
    let (a, b, c, d) = tokio::join!(
        hello("host-0"),
        hello("host-1"),
        hello("host-2"),
        hello("host-3")
    );
    let answers = [a, b, c, d];
    let admitted: Vec<usize> = (0..answers.len())
        .filter(|at| answers[*at].is_ok())
        .collect();
    assert_eq!(admitted.len(), 1, "{answers:?}");
    for refused in answers.iter().filter_map(|answer| answer.as_ref().err()) {
        assert_eq!(
            (refused.status.as_u16(), refused.error),
            (403, "join-token-used")
        );
    }
    let winner = format!("host-{}", admitted[0]);
    assert_eq!(rig.row(&who)["used_by"], winner.as_str());
    let bound = rig.keys("hosts/binding/");
    assert_eq!(bound, [format!("hosts/binding/{winner}")]);
}

/// Asserts that a hello with the pool's token commits the host's binding alone before the
/// registry is asked: when the commit fails it is refused 503 `state-write-failed`, with a
/// warning naming the failure, and the registry holds nothing; a hello the registry refuses
/// takes its binding back in a second commit; an admitted one leaves the binding to the
/// pool token's digest. It catches a pool token's binding left to the file-mode save after
/// the hello (a crash in between would lose it) and a binding kept for a host the registry
/// refused.
#[tokio::test]
async fn a_pool_token_hello_commits_the_binding_first_and_takes_it_back_when_refused() {
    let rig = rig("pooled");
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.pooled("host-a", "burst").await;
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
    let refused = rig.pooled("host-b", "elsewhere").await;
    assert_eq!(refused.expect_err("another pool").status.as_u16(), 403);
    assert_eq!(rig.commits(), before + 2);
    assert_eq!(rig.binding("host-b"), Value::Null);

    rig.pooled("host-a", "burst").await.expect("admitted");
    assert_eq!(
        rig.binding("host-a")["credential"],
        rig.pool.to_hex().as_str()
    );
}

/// Asserts that a token that registered its host, presented again after a restart over a
/// store that no longer holds the host's binding (the tick dropped it once the registry
/// forgot the host), has the binding committed alone before the registry is asked: a
/// failed commit refuses the hello 503 `join-token-unsaved` with nothing held, and the
/// hello repeated is admitted with one commit that holds the binding. It catches a binding
/// taken by the registry with no commit.
#[tokio::test]
async fn a_used_token_whose_binding_is_gone_commits_the_binding_alone() {
    let rig = rig("again_rebound");
    let who = rig.mint();
    rig.hello(&who, "host-a", "burst").await.unwrap();
    let gone = Txn::new().delete("hosts/binding/host-a", Cond::Any);
    rig.store.inner().txn(gone).unwrap().unwrap();
    let Rig {
        store, clock, dir, ..
    } = rig;
    let rig = over(dir, store, clock);

    rig.store.fail_next();
    let failed = rig.hello(&who, "host-a", "burst").await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "join-token-unsaved")
    );
    assert!(rig.held("host-a").is_none());
    let before = rig.commits();
    rig.hello(&who, "host-a", "burst")
        .await
        .expect("admitted again");
    assert_eq!(rig.commits(), before + 1);
    assert_eq!(rig.binding("host-a")["credential"], who.to_hex().as_str());
}

/// Asserts what the tick commits: a credential time that a heartbeat found due (a step
/// later than the stored one) is written by the tick, not by the heartbeat, and moved only
/// once the commit succeeded; a failed commit counts an error and leaves the stored time,
/// which the next tick writes. Once the registry forgets the silent host, the tick drops its
/// binding. It catches a renewal written on the hot path, one held in memory without its
/// commit, one lost after a failed commit, and the binding of a forgotten host kept forever.
#[tokio::test]
async fn the_tick_commits_due_credential_times_and_drops_forgotten_hosts() {
    let rig = rig("tick");
    let who = rig.mint();
    let welcome = rig.hello(&who, "host-a", "burst").await.unwrap();
    let used = rig.row(&who)["credential_expires_at_ms"].clone();
    assert_eq!(used, START_MS + 86_400_000);
    rig.clock.advance(Duration::from_secs(2 * 3600));
    let one_time = (&who, Some(who.clone()));
    rig.beat(one_time, "host-a", &welcome, Value::Null)
        .await
        .unwrap();
    assert_eq!(rig.row(&who)["credential_expires_at_ms"], used);

    rig.store.fail_next();
    rig.agents.refresh().await;
    assert_eq!(rig.agents.errors(), 1);
    assert_eq!(rig.row(&who)["credential_expires_at_ms"], used);
    rig.agents.refresh().await;
    assert_eq!(rig.agents.errors(), 1);
    let renewed = START_MS + 2 * 3_600_000 + 86_400_000;
    assert_eq!(rig.row(&who)["credential_expires_at_ms"], renewed);

    rig.clock.advance(Duration::from_secs(26 * 3600));
    rig.agents.refresh().await;
    assert!(rig.held("host-a").is_none());
    assert_eq!(rig.binding("host-a"), Value::Null);
}

/// Asserts that a credential time found due for a token that is revoked before the tick
/// writes nothing at the tick: the revoked row stays gone and no commit is made.
#[test]
fn a_due_time_of_a_token_revoked_before_the_tick_writes_nothing() {
    let store = fake();
    let clock = ManualClock::new(Timestamp::from_millis(START_MS));
    let random = Box::new(SequentialSessions::new());
    let kept: Arc<dyn Store> = store.clone();
    let joins = JoinTokens::in_store(kept, SharedClock::new(clock.clone()), random).unwrap();
    let minted = joins.mint(instance(), Duration::from_secs(600)).unwrap();
    let who = Credential::of_token(&minted.token);
    joins.admit(&who, "host-a", Some(&instance())).unwrap();
    clock.advance(Duration::from_secs(2 * 3600));
    assert_eq!(joins.serves(&who, "host-a"), Ok(()));
    assert_eq!(joins.revoke(&instance()), Ok(1));
    let before = store.commit_stats().txns;
    assert!(joins.flush());
    assert_eq!(store.commit_stats().txns, before);
    assert!(store.scan_prefix("hosts/join/").unwrap().is_empty());
}

/// Asserts that the stored hosts keep what a newer build wrote: a binding and a token row
/// with a higher version and a field this build does not know are read, and the next write
/// of each key writes the field back (with this build's version). It catches a write that
/// drops a newer build's data, and a reader that refuses a newer version.
#[test]
fn stored_hosts_keep_the_fields_a_newer_build_wrote() {
    let store = fake();
    let digest = Credential::of_token("t").to_hex();
    let binding = json!({ "version": 2, "credential": digest, "later": [1] });
    let row = json!({
        "version": 2,
        "provider": "lambdalabs",
        "instance_id": "i-1",
        "expires_at_ms": 9_000_000u64,
        "later": "x",
    });
    let txn = Txn::new()
        .put("hosts/binding/host-a", binding.to_string(), Cond::Any)
        .put(format!("hosts/join/{digest}"), row.to_string(), Cond::Any);
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
    bindings.save(|| cordoned).unwrap();
    let written = store.get("hosts/binding/host-a").unwrap().unwrap().value;
    let written: Value = serde_json::from_slice(&written).unwrap();
    let expected = json!({
        "version": 1,
        "credential": digest,
        "cordon_until_ms": store_ms + 7,
        "later": [1],
    });
    assert_eq!(written, expected);

    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(START_MS)));
    let random = Box::new(SequentialSessions::new());
    let joins = JoinTokens::in_store(kept, clock, random).unwrap();
    let who = Credential::of_token("t");
    joins.admit(&who, "host-b", Some(&instance())).unwrap();
    let written = store
        .get(&format!("hosts/join/{digest}"))
        .unwrap()
        .unwrap()
        .value;
    let written: Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(
        (&written["version"], &written["used_by"], &written["later"]),
        (&json!(1), &json!("host-b"), &json!("x"))
    );
}

/// Asserts that a stored value that cannot be read as written refuses the read with a
/// reason naming its key and what is wrong, as a damaged file is refused: a binding or a row
/// that is not the key's JSON, a binding whose credential is no digest or whose key is no
/// host id, a row whose key is no digest, and a store that cannot be read. It catches a
/// start that reads a damaged store as fewer bindings or tokens (a booked host's binding
/// lost, a used token given back).
#[test]
fn a_stored_host_that_cannot_be_read_as_written_refuses_the_read() {
    let digest = Credential::of_token("t").to_hex();
    let row = json!({ "version": 1, "provider": "p", "instance_id": "i", "expires_at_ms": 1 });
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
        (
            "hosts/join/k",
            "[]".to_owned(),
            "hosts/join/k does not hold this key's fields",
        ),
        (
            "hosts/join/abc",
            row.to_string(),
            "hosts/join/abc: a token hash is not 64 lower-case hex digits",
        ),
    ];
    for (key, value, reason) in cases {
        let store = fake();
        store
            .txn(Txn::new().put(key, value, Cond::Any))
            .unwrap()
            .unwrap();
        let kept: Arc<dyn Store> = store;
        let bindings =
            BindingStore::in_store(Arc::clone(&kept), crate::agent_state::epoch_clock()).err();
        let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(START_MS)));
        let random = Box::new(SequentialSessions::new());
        let joins = JoinTokens::in_store(kept, clock, random).err();
        assert_eq!(bindings.or(joins).as_deref(), Some(reason), "{key}");
    }

    let store = fake();
    store.fail_read_after(0);
    let failed = BindingStore::in_store(store.clone(), crate::agent_state::epoch_clock())
        .err()
        .unwrap();
    assert!(failed.contains(js_jobs::interfere::INJECTED), "{failed}");
    store.fail_read_after(0);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(START_MS)));
    let random = Box::new(SequentialSessions::new());
    let failed = JoinTokens::in_store(store, clock, random).err().unwrap();
    assert!(failed.contains(js_jobs::interfere::INJECTED), "{failed}");
}

/// Asserts that two servers over one store cannot lose a taint write to each other. Both
/// hold the host with no taint. The second is told taint `a` and commits it. The first, which
/// still holds none, is then told taint `b`: it is refused 503 `state-write-failed`, the
/// store keeps `a` and the first holds none (a report must not overwrite or revive a taint
/// the store holds). The first server then has the taint cleared and `b` stored (as the
/// first's own beat would leave it); the second, which still holds `a`, is told `a` is
/// cleared: refused the same way, the store keeps `b`, the second still holds `a` and nothing
/// is acknowledged (a clear must not erase a newer taint). It catches the host binding's
/// unconditional write, which let either lose.
#[tokio::test]
async fn two_servers_do_not_overwrite_each_others_taint() {
    let first = rig("two_first");
    let welcome = first.pooled("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "two_second");
    let second_welcome = second.pooled("host-a", "burst").await.unwrap();
    let a = json!({ "reason": "residue", "check": "daemonised-child", "run_id": "r1" });
    let b = json!({ "reason": "residue", "check": "process-as-job-user", "run_id": "r2" });

    let reply = second.beat((&second.pool, None), "host-a", &second_welcome, a.clone());
    assert!(reply.await.unwrap().taint_ack);
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");
    let commits = first.commits();
    let late = first.beat((&first.pool, None), "host-a", &welcome, b.clone());
    let late = late
        .await
        .expect_err("the store holds a taint the server does not");
    assert_eq!(
        (late.status.as_u16(), late.error),
        (503, "state-write-failed")
    );
    assert_eq!(first.commits(), commits, "nothing was written");
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");
    assert!(first.held("host-a").unwrap().taint().is_none());

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
    assert_eq!(held.taint().unwrap().run_id.as_deref(), Some("r1"));
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
    rig.pooled("host-a", "burst").await.unwrap();
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
