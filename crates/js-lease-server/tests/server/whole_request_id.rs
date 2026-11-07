//! `request_id` on `POST /v1/leases`: a whole-node request waits for a host under it (the
//! registry's wait list, reservation and drain, reached through the HTTP route), a repeat
//! of the acquire answers the lease it made, and the id outlives a restart. A body without
//! the field is answered as before. That a release of the waiting lease cancels its
//! reservation is a scenario of the simulation cell (`crates/js-sim-cell/tests/
//! migrated_whole_host.rs`), at scale.

use std::ffi::OsString;
use std::time::Duration;

use js_types::RequestId;
use serde_json::{Value, json};

use crate::agents_support::{POOL, Session, agent_args, beat, beat_preparing, entry, hosts, join};
use crate::support::{Harness, Reply, Scratch, lease_body, replace};

const ID: &str = "nightly-42";

/// The flags of a server whose registered pool serves `whole-node`, with a grace so long
/// that a test agent need not beat to stay serving.
fn args(scratch: &Scratch) -> Vec<OsString> {
    let args = agent_args(scratch, false);
    let pool = POOL
        .replace("gpu-host", "whole-node")
        .replace("max_machines = 2\n", "max_machines = 2\ngrace = \"2h\"\n");
    scratch.write("agent-pool.toml", &pool, 0o644);
    args
}

async fn start(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = args(&scratch);
    Harness::with(scratch, args, None).await
}

/// Stops `first` and starts another server over its files with `--boot resume`.
async fn restarted(first: Harness) -> Harness {
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = replace(args(&scratch), "--boot", "resume");
    Harness::with(scratch, args, None).await
}

/// The whole-node request, without a request id.
fn plain() -> Value {
    let mut body = lease_body();
    body["class"] = json!("whole-node");
    body
}

/// The whole-node request under [`ID`].
fn named() -> Value {
    let mut body = plain();
    body["request_id"] = json!(ID);
    body
}

async fn acquire(h: &Harness, body: &Value) -> Reply {
    h.call("POST", "/v1/leases", Some(body)).await
}

/// Posts `body`, which must be answered 202 pending; the lease id.
async fn pending(h: &Harness, body: &Value) -> String {
    let asked = acquire(h, body).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    let id = asked.json()["id"].as_str().unwrap().to_string();
    assert_eq!(asked.json(), json!({ "id": id, "state": "pending" }));
    id
}

/// A whole-node lease without an id, made held on `session`'s host (the one quiet host):
/// the agent is told of it and reports it ready. Its id.
async fn hold(h: &Harness, session: &Session) -> String {
    let id = pending(h, &plain()).await;
    let reply = beat_preparing(h, session).await;
    assert_eq!(reply.json()["exclusive"], json!(id), "{}", reply.body);
    h.ticks.step().await;
    assert_eq!(state(h, &id).await, "held");
    id
}

/// The state lease `id` is listed in, or `gone`.
async fn state(h: &Harness, id: &str) -> String {
    let list = h.call("GET", "/v1/leases", None).await.json();
    let leases = list["leases"].as_array().unwrap();
    let found = leases.iter().find(|lease| lease["id"] == id);
    found.map_or("gone".to_string(), |lease| {
        lease["state"].as_str().unwrap().to_string()
    })
}

/// The ids the lease list holds, in order.
async fn lease_ids(h: &Harness) -> Vec<String> {
    let list = h.call("GET", "/v1/leases", None).await.json();
    let leases = list["leases"].as_array().unwrap();
    let ids = leases.iter().map(|lease| lease["id"].as_str().unwrap());
    ids.map(str::to_string).collect()
}

async fn release(h: &Harness, id: &str) {
    let freed = h.call("DELETE", &format!("/v1/leases/{id}"), None).await;
    assert_eq!(freed.status, 200, "{}", freed.body);
    assert_eq!(freed.json(), json!({ "state": "freed" }));
}

/// Two heartbeats of `session`: the second follows the stop the first crossed, so the
/// host is serving with nothing held.
async fn settle(h: &Harness, session: &Session) {
    beat(h, session).await;
    beat(h, session).await;
}

/// `session`'s host is told of lease `id` and reports it ready; the tick after makes it
/// held there.
async fn ready(h: &Harness, session: &Session, id: &str) {
    let reply = beat_preparing(h, session).await;
    assert_eq!(reply.json()["exclusive"], json!(id), "{}", reply.body);
    h.ticks.step().await;
    let list = h.call("GET", "/v1/leases", None).await.json();
    let lease = list["leases"].as_array().unwrap();
    let lease = lease.iter().find(|lease| lease["id"] == id).unwrap();
    assert_eq!(lease["state"], "held", "{lease}");
    assert_eq!(lease["host"], session.host.as_str(), "{lease}");
}

fn request_id() -> RequestId {
    RequestId::new(ID).unwrap()
}

/// Asserts the whole path through the HTTP route. Both hosts are busy with whole-node
/// leases of other requests; a whole-node acquire under a request id is answered 202
/// pending, not refused, and stays listed pending across ticks while no host can be set
/// aside. Once one of the leases is released, the next tick sets its host aside for the
/// request (`GET /v1/hosts` shows `mark: "reserved"` and `reserved_for` with the id) while
/// the host is still settling; once the host is quiet the next tick commits it, the agent
/// is told of exactly this lease, and after its ready report the lease is held there. It
/// catches a route that never hands the id to the registry (the acquire would be refused
/// `no-capacity`), a waiting lease the tick drops (the registry has no booking for it to
/// observe), and a reservation that is made but never committed (nothing would present
/// the request again).
#[tokio::test]
async fn whole_node_request_with_id_is_queued_and_reserves_a_host() {
    let h = start("request_id_queued").await;
    let first = join(&h, "burst-1").await;
    let second = join(&h, "burst-2").await;
    let one = hold(&h, &first).await;
    hold(&h, &second).await;

    let id = pending(&h, &named()).await;
    h.ticks.step().await;
    assert_eq!(state(&h, &id).await, "pending");
    let listing = hosts(&h).await;
    for host in ["burst-1", "burst-2"] {
        assert_eq!(entry(&listing, host).get("reserved_for"), None, "{listing}");
    }
    assert_eq!(
        h.running.registry().unwrap().queue_position(&request_id()),
        Some(0)
    );

    release(&h, &one).await;
    h.ticks.step().await;
    let host = entry(&hosts(&h).await, "burst-1");
    assert_eq!(host["mark"], "reserved", "{host}");
    assert_eq!(host["reserved_for"], ID, "{host}");
    assert_eq!(state(&h, &id).await, "pending");

    settle(&h, &first).await;
    h.ticks.step().await;
    assert_eq!(state(&h, &id).await, "pending");
    ready(&h, &first, &id).await;
    let host = entry(&hosts(&h).await, "burst-1");
    assert_eq!(host.get("reserved_for"), None, "{host}");
    assert_eq!(
        h.running.registry().unwrap().queue_position(&request_id()),
        None
    );
}

/// Asserts retry safety. A repeat of the acquire under the same id and body, while the
/// request waits, is answered with the same pending lease, and nothing is added to the
/// list; once the lease is held, a repeat is answered 201 with the same id, its host and
/// the expiry the list shows. It catches a repeat that makes a second lease (the case a
/// lost answer turns into a second host) and a repeat answered with another state than
/// the lease's own.
#[tokio::test]
async fn same_request_id_returns_the_same_lease() {
    let h = start("request_id_same").await;
    let me = join(&h, "burst-1").await;
    let busy = hold(&h, &me).await;
    let id = pending(&h, &named()).await;
    assert_eq!(pending(&h, &named()).await, id);
    assert_eq!(lease_ids(&h).await, [busy.clone(), id.clone()]);

    release(&h, &busy).await;
    h.ticks.step().await;
    settle(&h, &me).await;
    h.ticks.step().await;
    ready(&h, &me, &id).await;
    let list = h.call("GET", "/v1/leases", None).await.json();
    let ends = list["leases"][0]["ends_at_ms"].clone();
    let again = acquire(&h, &named()).await;
    assert_eq!(again.status, 201, "{}", again.body);
    let held = json!({ "id": id, "host": "burst-1", "ends_at_ms": ends });
    assert_eq!(again.json(), held);
    assert_eq!(lease_ids(&h).await, [id]);
}

/// Asserts the typed conflict. The same request id with other resources, and with
/// another class, is 409 `request-id-conflict`; the waiting lease and its place on the
/// wait list are as they were. It catches a changed request taken for the same one (the
/// client would hold a lease that is not what it asked for) and a conflict that makes or
/// drops a lease.
#[tokio::test]
async fn same_request_id_different_body_is_a_conflict() {
    let h = start("request_id_conflict").await;
    let me = join(&h, "burst-1").await;
    let busy = hold(&h, &me).await;
    let id = pending(&h, &named()).await;

    let mut more = named();
    more["resources"]["cpu_millis"] = json!(2000);
    let mut other = named();
    other["class"] = json!("gpu-host");
    for body in [more, other] {
        let refused = acquire(&h, &body).await;
        assert_eq!(refused.status, 409, "{}", refused.body);
        assert_eq!(refused.json(), json!({ "error": "request-id-conflict" }));
    }
    assert_eq!(lease_ids(&h).await, [busy, id]);
    assert_eq!(
        h.running.registry().unwrap().queue_position(&request_id()),
        Some(0)
    );
}

/// Asserts D55 through the HTTP path. A request under an id has its host set aside when
/// the server stops: the host's lease was released, so the host is granted to the request
/// with nothing to drain and no drain deadline. After a `--boot resume` over the same
/// lease file, a repeat of the acquire is answered with the same pending lease. Ten
/// seconds later (inside `regain_grace`) the host says hello and is listed as it was:
/// reserved for the id, with no drain deadline either. The next tick presents the request
/// again under its id, which claims the restored record: the request takes that host,
/// nothing was dropped as unclaimed, and after the ready report the lease is held there.
/// It catches a request id lost with the process (the repeat would make a second lease),
/// a reservation that does not come back, and a restored record nobody presents (it would
/// be dropped and counted, and the request would wait on with no host). A drain deadline
/// kept across a restart, above the boot floor, is asserted in `agents_whole`.
#[tokio::test]
async fn request_id_survives_a_restart_and_rebinds() {
    let first = start("request_id_restart").await;
    let me = join(&first, "burst-1").await;
    let busy = hold(&first, &me).await;
    let id = pending(&first, &named()).await;
    release(&first, &busy).await;
    first.ticks.step().await;
    let host = entry(&hosts(&first).await, "burst-1");
    assert_eq!(host["reserved_for"], ID, "{host}");
    let deadline = host.get("drain_deadline_ms").cloned();
    assert_eq!(deadline, None, "{host}");
    first.ticks.step().await;

    let h = restarted(first).await;
    assert_eq!(pending(&h, &named()).await, id);
    h.clock.advance(Duration::from_secs(10));
    let me = join(&h, "burst-1").await;
    let host = entry(&hosts(&h).await, "burst-1");
    assert_eq!(host["mark"], "reserved", "{host}");
    assert_eq!(host["reserved_for"], ID, "{host}");
    assert_eq!(host.get("drain_deadline_ms").cloned(), deadline, "{host}");

    h.ticks.step().await;
    let registry = h.running.registry().unwrap();
    assert_eq!(registry.dropped_reservations(), 0);
    assert_eq!(registry.queue_position(&request_id()), None);
    ready(&h, &me, &id).await;
}

/// Asserts that a body without `request_id` is answered byte for byte as before: the 201
/// of a static host is the text `golden/gpu_path.txt` pins, the lease file has no
/// `request` row, and a whole-node request with no quiet host is refused `no-capacity`,
/// the exact body, with nothing queued. It catches a field that changes the answer, or
/// the file, of every client that does not send it, and a whole-node request queued
/// without an id.
#[tokio::test]
async fn no_request_id_is_byte_identical_to_today() {
    let h = Harness::new("request_id_absent").await;
    let granted = acquire(&h, &lease_body()).await;
    assert_eq!(granted.status, 201);
    assert_eq!(
        granted.body,
        "{\"id\":\"1-1\",\"host\":\"gpu-01\",\"ends_at_ms\":8200000}"
    );
    let file = std::fs::read_to_string(h.scratch.state()).unwrap();
    assert!(!file.contains("request"), "{file}");

    let w = start("request_id_absent_whole").await;
    let me = join(&w, "burst-1").await;
    hold(&w, &me).await;
    let refused = acquire(&w, &plain()).await;
    assert_eq!(
        (refused.status, refused.body.as_str()),
        (409, "{\"error\":\"no-capacity\"}")
    );
    assert!(
        w.running
            .registry()
            .unwrap()
            .reservation_ledger()
            .is_empty()
    );
}

/// Asserts the field's check at its edges. A string that is not a request id (spaces,
/// empty, 64 bytes, a letter outside ASCII) is a 400 whose reason names `request_id`, and a
/// value that is not a string is a 400 as any mistyped field is; none makes a lease. An id
/// of 63 bytes is taken. `null` is no id: the answer is byte for byte the answer to the
/// same body without the field, and so is the lease file it leaves. It catches an id the
/// registry would key a request by without checking it, a limit off by one, and a `null`
/// read as anything but absent.
#[tokio::test]
async fn bad_request_id_is_a_400_naming_the_field() {
    let h = Harness::new("request_id_bad").await;
    let long = "a".repeat(64);
    for bad in ["not an id", "", long.as_str(), "caf\u{e9}"] {
        let mut body = lease_body();
        body["request_id"] = json!(bad);
        let refused = acquire(&h, &body).await;
        assert_eq!(refused.status, 400, "{bad:?}: {}", refused.body);
        let expected = json!({
            "error": "bad-request",
            "reason": "request_id is not a valid request id",
        });
        assert_eq!(refused.json(), expected, "{bad:?}");
    }
    let mut number = lease_body();
    number["request_id"] = json!(7);
    let refused = acquire(&h, &number).await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert_eq!(refused.json()["error"], "bad-request");
    assert!(lease_ids(&h).await.is_empty());
    let mut longest = lease_body();
    longest["request_id"] = json!("a".repeat(63));
    assert_eq!(acquire(&h, &longest).await.status, 201);

    let without = Harness::new("request_id_without").await;
    let nulled = Harness::new("request_id_null").await;
    let mut body = lease_body();
    let plain = acquire(&without, &body).await;
    body["request_id"] = Value::Null;
    let null = acquire(&nulled, &body).await;
    assert_eq!(
        (null.status, null.body.as_str()),
        (plain.status, plain.body.as_str())
    );
    let file = |h: &Harness| std::fs::read_to_string(h.scratch.state()).unwrap();
    assert_eq!(file(&nulled), file(&without));
}
