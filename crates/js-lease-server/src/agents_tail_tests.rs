//! The tail counters wired into the server, in process: the agent side assembled from the
//! flags as the server assembles it, a host registered with the registry, and leases
//! started and stopped on its provider. With a usage store the counters are kept in it and
//! a server assembled again over the same files counts on from them; with none they are
//! in memory only. No listener is bound and no connection is made.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;
use js_provider::{Pool, StartAck, StartSpec};
use js_registry::{Credential, ERROR_EDGES_MS, TailCounters};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Cond, Store, SystemClock, Txn};
use js_testkit::dir::TestDir;
use js_types::{Admission, ClassId, LeaseId, Request, ResourceClass, Resources};
use js_types::{ManualClock, Timestamp};
use js_usage::{StoreTailSink, TailSink, TailSinkCounters};
use serde_json::json;

use crate::adapt::SharedClock;
use crate::agents::{Agents, assemble};
use crate::config::{Cli, Config};

/// The join token of the pool.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// A pool of two machines whose class books a whole host.
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

fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tail_counters");
    TestDir::under(root, name)
}

fn write(dir: &Path, name: &str, text: &str, mode: u32) -> OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.into_os_string()
}

/// A server with agent registration over [`POOL`] and `extra` flags; usage is in
/// `usage.db` beside the lease file unless `extra` turns it off.
fn config(dir: &Path, extra: &[&str]) -> Config {
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

/// The agent side of a server over the files in `dir`, with hosts `a` and `b` registered.
fn agents(dir: &Path, extra: &[&str]) -> Arc<Agents> {
    let config = config(dir, extra);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(1_000_000)));
    let (_, agents) =
        assemble(config.hosts, config.usage, &clock, &config.state_file, None).unwrap();
    let agents = agents.unwrap();
    for host in ["a", "b"] {
        hello(&agents, host);
    }
    agents
}

/// Registers `host` with the registry of `agents`.
fn hello(agents: &Agents, host: &str) {
    let body = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    });
    let hello: Hello = serde_json::from_value(body).unwrap();
    let credential = Credential::of_token(JOIN);
    agents.registry.hello(&hello, &credential).unwrap();
}

/// Starts and stops lease `sequence` on a free host: one lease ended with no estimate.
fn one_lease(agents: &Agents, sequence: u64) {
    let book = ResourceClass::new(ClassId::new("book").unwrap(), Admission::BookAll, 0);
    let mut provider = agents.registry.provider().with_classes(vec![book.clone()]);
    let request = Request {
        class: book.id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: Default::default(),
        max_wall: Duration::from_secs(600),
        labels: Default::default(),
        validation_run: None,
        probe_host: None,
    };
    let lease = LeaseId::new(1, sequence);
    let spec = StartSpec::new(lease, request, &[book]).unwrap();
    assert!(matches!(provider.start(spec), StartAck::Started { .. }));
    provider.stop(&lease);
}

/// `n` leases ended with no estimate, and nothing else.
fn unestimated(n: u64) -> TailCounters {
    TailCounters {
        ended_unestimated: n,
        ..TailCounters::default()
    }
}

/// Asserts that a server with a usage store keeps the tail counters in it: what one
/// server counted and flushed at its shutdown, a server assembled again over the same
/// files reports, and counts on from. Catches: a server that does not give the registry
/// the usage store's sink (the counters would restart at zero), and a flush that does not
/// write the tail counters.
#[tokio::test]
async fn the_tail_counters_are_kept_in_the_usage_store_across_a_restart() {
    let dir = scratch("restart");
    {
        let first = agents(dir.path(), &[]);
        one_lease(&first, 1);
        assert_eq!(first.registry.tail_counters(), unestimated(1));
        first.usage.as_ref().unwrap().finish().await;
    }
    let second = agents(dir.path(), &[]);
    assert_eq!(second.registry.tail_counters(), unestimated(1));
    one_lease(&second, 2);
    assert_eq!(second.registry.tail_counters(), unestimated(2));
    let counted = second.usage.as_ref().unwrap().counters();
    assert_eq!(counted.tail, TailSinkCounters::default());
}

/// Asserts that a usage store whose tail counters cannot be read (their edges row is of a
/// version this build does not read) leaves the server starting and counting from zero,
/// and the stored row as it was. Catches: a start refused over the tail counters, a
/// broken row read as counts, and one overwritten.
#[tokio::test]
async fn tail_counters_that_cannot_be_read_start_from_zero() {
    let dir = scratch("unread");
    let key = "usage/tail/error_edges_ms";
    {
        let clock = Arc::new(SystemClock::default());
        let db = dir.path().join("usage.db");
        let store = SqliteStore::open(&db, clock, Options::default()).unwrap();
        let txn = Txn::new().put(key, vec![9], Cond::Any);
        store.txn(txn).unwrap().unwrap();
    }
    let served = agents(dir.path(), &[]);
    assert_eq!(served.registry.tail_counters(), TailCounters::default());
    one_lease(&served, 1);
    assert_eq!(served.registry.tail_counters(), unestimated(1));
    let usage = served.usage.as_ref().unwrap();
    usage.finish().await;
    assert_eq!(usage.counters().tail.writes_failed, 1);
}

/// Asserts that a server with `--usage-off` has no usage store and counts the tail
/// counters in memory exactly as before. Catches: a server without a usage store that
/// refuses to start, or whose registry stops counting.
#[test]
fn without_a_usage_store_the_counters_are_in_memory() {
    let dir = scratch("off");
    let served = agents(dir.path(), &["--usage-off"]);
    assert!(served.usage.is_none());
    one_lease(&served, 1);
    assert_eq!(served.registry.tail_counters(), unestimated(1));
}

/// Asserts the limit of what a stop without the final flush keeps: the counts a flush
/// wrote survive it, and the counts made after that flush are gone from the store (they
/// were in memory only). Catches: counts written at every start or stop (the second server
/// would read both), and a flushed baseline lost or rewritten by the counts that followed.
#[tokio::test]
async fn a_stop_without_the_final_flush_keeps_what_was_flushed_and_loses_the_rest() {
    let dir = scratch("unflushed");
    {
        let first = agents(dir.path(), &[]);
        one_lease(&first, 1);
        first.usage.as_ref().unwrap().finish().await;
        one_lease(&first, 2);
        assert_eq!(first.registry.tail_counters(), unestimated(2));
    }
    let second = agents(dir.path(), &[]);
    assert_eq!(second.registry.tail_counters(), unestimated(1));
}

/// Asserts that the tail counters are kept in the usage store `--usage-db` names, away from
/// the lease file: that file holds them after the final flush, nothing is made beside the
/// lease file, and a server started again with the same flag counts on from them. Catches:
/// the counters kept in a store of their own beside the lease file, and a server that reads
/// them from anywhere but the named store.
#[tokio::test]
async fn the_tail_counters_are_kept_in_the_store_usage_db_names() {
    let dir = scratch("named-lease");
    let elsewhere = scratch("named-usage");
    let db = elsewhere.path().join("counters.db");
    let named = db.to_str().unwrap().to_owned();
    let flags = ["--usage-db", named.as_str()];
    {
        let first = agents(dir.path(), &flags);
        one_lease(&first, 1);
        first.usage.as_ref().unwrap().finish().await;
    }
    assert!(!dir.path().join("usage.db").exists());
    {
        let clock = Arc::new(SystemClock::default());
        let store = SqliteStore::open(&db, clock, Options::default()).unwrap();
        let stored = StoreTailSink::new(Arc::new(store), ERROR_EDGES_MS).load();
        assert_eq!(stored.unwrap().ended_unestimated, 1);
    }
    let second = agents(dir.path(), &flags);
    assert_eq!(second.registry.tail_counters(), unestimated(1));
}
