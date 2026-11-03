//! `request_id` through the lease routes, over the rig of the mark tests: one registered
//! host, `a`, in a pool that serves a share class beside the whole-node class, so an
//! ordinary lease can keep the host busy while a whole-node request waits for it. The
//! routes are called without a socket, on a manual clock; a restart drops the server with
//! no final tick, as a crash would. Nothing here sleeps.

use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_types::RequestId;
use serde_json::{Value, json};

use super::whole_view_tests::Rig;
use super::{acquire, hosts, list, release};

fn id(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
}

/// A whole-node request the host fits, under `request_id`.
fn whole(request_id: &str) -> Value {
    json!({
        "class": "whole-node",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1 },
        "capabilities": { "os": "macos" },
        "max_wall_seconds": 600,
        "request_id": request_id,
    })
}

/// An ordinary request of the share class.
fn ordinary() -> Value {
    json!({
        "class": "share",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1 },
        "capabilities": { "os": "macos" },
        "max_wall_seconds": 600,
    })
}

async fn answer(reply: Result<Response, super::ApiError>) -> (StatusCode, Value) {
    let reply = reply.unwrap_or_else(IntoResponse::into_response);
    let status = reply.status();
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn post(rig: &Rig, body: &Value) -> (StatusCode, Value) {
    let bytes = Bytes::from(body.to_string());
    answer(acquire(State(rig.shared.clone()), bytes).await).await
}

/// Posts `body`, which must be answered 202 pending; the lease id.
async fn pending(rig: &Rig, body: &Value) -> String {
    let (status, body) = post(rig, body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

async fn delete(rig: &Rig, lease: &str) -> (StatusCode, Value) {
    answer(release(State(rig.shared.clone()), Path(lease.to_string())).await).await
}

/// The listed state of lease `lease`, or `gone`.
async fn state(rig: &Rig, lease: &str) -> String {
    let (_, body) = answer(Ok(list(State(rig.shared.clone())).await)).await;
    let leases = body["leases"].as_array().unwrap().clone();
    let found = leases.into_iter().find(|listed| listed["id"] == lease);
    found.map_or("gone".to_string(), |listed| {
        listed["state"].as_str().unwrap().to_string()
    })
}

/// The listing of host `a`.
async fn host_a(rig: &Rig) -> Value {
    let (_, body) = answer(Ok(hosts(State(rig.shared.clone())).await)).await;
    let listed = body["hosts"].as_array().unwrap().clone();
    listed.into_iter().find(|host| host["id"] == "a").unwrap()
}

/// What the lease file holds now.
fn file(rig: &Rig) -> String {
    std::fs::read_to_string(rig.file()).unwrap()
}

/// Whether the registry's ledger has a row for `request`.
fn in_ledger(rig: &Rig, request: &str) -> bool {
    let ledger = rig.registry.reservation_ledger();
    ledger.iter().any(|row| row.request_id == request)
}

/// A rig whose host is busy with an ordinary lease and set aside for `r1`, waiting under
/// a pending lease, with the ledger in the lease file. The ordinary lease and the waiting
/// lease's ids.
async fn reserved(name: &str) -> (Rig, String, String) {
    let rig = Rig::new(name).await;
    let busy = rig.lease().await;
    let waiting = pending(&rig, &whole("r1")).await;
    rig.shared.tick().await;
    rig.shared.tick().await;
    assert!(file(&rig).contains("request_id = \"r1\""));
    (rig, busy, waiting)
}

/// Asserts the starvation rule through the routes, with an ordinary lease on the host. A
/// whole-node acquire under an id is 202 pending, and the tick sets the busy host aside for
/// it while the ordinary lease still holds it. Once that lease is released, an ordinary
/// acquire is refused `no-capacity`: the quiet host is kept for the request, not handed to
/// the newcomer. The next tick commits the host to the waiting lease, which the host
/// listing shows as `held` with that lease. It catches a reservation that a stream of
/// ordinary leases can overtake, and a drained host never committed.
#[tokio::test]
async fn a_waiting_request_drains_a_host_busy_with_an_ordinary_lease() {
    let (rig, busy, waiting) = reserved("request_id_drain").await;
    let marks = rig.registry.reservations();
    assert_eq!(marks.len(), 1);
    assert_eq!(
        (marks[0].request.as_str(), marks[0].host.as_str()),
        ("r1", "a")
    );
    assert_eq!(state(&rig, &busy).await, "held");

    assert_eq!(delete(&rig, &busy).await.0, StatusCode::OK);
    let (status, body) = post(&rig, &ordinary()).await;
    assert_eq!(
        (status, body),
        (StatusCode::CONFLICT, json!({ "error": "no-capacity" }))
    );

    rig.shared.tick().await;
    let host = host_a(&rig).await;
    assert_eq!(
        (&host["mark"], &host["lease"]),
        (&json!("held"), &json!(waiting)),
        "{host}"
    );
    assert_eq!(state(&rig, &waiting).await, "pending");
}

/// Asserts that a cancel is in the lease file before the answer. With the waiting
/// request's row in the file, a release of its lease leaves no row for it there, and a
/// server started over the file with no tick between (a crash) has no reservation and no
/// waiting request for it. It catches a cancel the file learns of only at the next tick,
/// which a crash turns into a reservation for a lease that is gone.
#[tokio::test]
async fn a_cancel_is_written_before_it_is_answered() {
    let (rig, _busy, waiting) = reserved("request_id_cancel_written").await;
    let (status, body) = delete(&rig, &waiting).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "state": "freed" }))
    );
    assert!(
        !file(&rig).contains("request_id = \"r1\""),
        "{}",
        file(&rig)
    );

    let rig = rig.restart(true).await;
    assert!(rig.registry.reservations().is_empty());
    assert_eq!(rig.registry.queue_position(&id("r1")), None);
}

/// Asserts recovery from a crash between a release's two writes. The first write holds the
/// lease as releasing and the ledger as it was, with the request waiting; a server started
/// over exactly that file binds the request to the releasing lease, and its first tick stops
/// the lease again, which cancels the request: no waiter, no reservation, the lease gone and
/// no row for the request in the file. The same file is what a failed second write leaves.
/// It catches a crash or a failed write at that point that leaves a request waiting, and a
/// host set aside, for a lease that is gone.
#[tokio::test]
async fn a_crash_between_the_release_writes_cancels_the_request_at_the_next_tick() {
    let (rig, _busy, waiting) = reserved("request_id_release_crash").await;
    let text = file(&rig);
    let releasing = text.replace("state = \"pending\"", "state = \"releasing\"");
    assert_ne!(releasing, text);
    std::fs::write(rig.file(), releasing).unwrap();

    let rig = rig.restart(true).await;
    assert_eq!(rig.registry.queue_position(&id("r1")), Some(0));
    rig.shared.tick().await;
    assert_eq!(rig.registry.queue_position(&id("r1")), None);
    assert!(rig.registry.reservations().is_empty());
    assert_eq!(state(&rig, &waiting).await, "gone");
    assert!(
        !file(&rig).contains("request_id = \"r1\""),
        "{}",
        file(&rig)
    );
}

/// Asserts that the tick whose presentation placed a waiting request writes the ledger as
/// it then stands: the file has a row for the request exactly when the registry still does.
/// It catches a file one tick behind the presentations, which a crash after the commit
/// turns into a request restored to a queue it has left.
#[tokio::test]
async fn the_tick_that_places_a_request_writes_the_ledger_it_leaves() {
    let (rig, busy, _waiting) = reserved("request_id_commit_written").await;
    delete(&rig, &busy).await;
    rig.shared.tick().await;
    assert_eq!(rig.registry.queue_position(&id("r1")), None);
    let written = file(&rig).contains("request_id = \"r1\"");
    assert_eq!(written, in_ledger(&rig, "r1"), "{}", file(&rig));
}

/// Asserts the cancel of a waiting lease restored by a restart, before any tick has
/// presented it again: the restored request is bound to the lease at the start, so the
/// release takes it off the wait list and frees the host's mark at once. It catches a
/// restored entry that only a presentation ties to its lease, which a release before the
/// first tick would leave behind, holding the host for nobody.
#[tokio::test]
async fn a_restored_waiting_lease_is_cancelled_by_a_release_before_any_tick() {
    let (rig, _busy, waiting) = reserved("request_id_restored_cancel").await;
    let rig = rig.restart(true).await;
    assert_eq!(rig.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(delete(&rig, &waiting).await.0, StatusCode::OK);
    assert_eq!(rig.registry.queue_position(&id("r1")), None);
    assert!(rig.registry.reservations().is_empty());
    assert!(!in_ledger(&rig, "r1"));
}

/// Asserts that a cancel forgets the request. `r1` gives its host up at the drain deadline
/// (one give-up, the host excluded for it); a release of its lease leaves no row for `r1`
/// in the ledger, and an acquire under `r1` again is a new request: a new lease, no
/// give-up, and the host set aside for it at the next tick. It catches a cancelled
/// request whose failures a later request under the id inherits.
#[tokio::test]
async fn a_cancel_after_a_give_up_leaves_the_id_fresh() {
    let (rig, _busy, waiting) = reserved("request_id_give_up").await;
    rig.advance(31);
    rig.shared.tick().await;
    let row = rig.registry.reservation_ledger();
    let row = row.iter().find(|row| row.request_id == "r1").unwrap();
    assert_eq!((row.give_ups, row.exclusions.len()), (1, 1));

    delete(&rig, &waiting).await;
    assert!(!in_ledger(&rig, "r1"));
    let again = pending(&rig, &whole("r1")).await;
    assert_ne!(again, waiting);
    rig.shared.tick().await;
    let row = rig.registry.reservation_ledger();
    let row = row.iter().find(|row| row.request_id == "r1").unwrap();
    assert_eq!((row.give_ups, row.exclusions.len()), (0, 0));
    assert_eq!(rig.registry.reservations()[0].request, id("r1"));
}

/// Asserts the recovery of a waiting request after a crash with no client action: the
/// first tick presents the restored lease again, which keeps it pending under the same
/// id and on the wait list; once the ordinary lease is released, a tick commits the host
/// to it. It catches a restored lease that waits for its client to repeat the acquire.
#[tokio::test]
async fn a_restart_presents_the_waiting_request_with_no_client() {
    let (rig, busy, waiting) = reserved("request_id_restart_alone").await;
    let rig = rig.restart(true).await;
    rig.shared.tick().await;
    assert_eq!(state(&rig, &waiting).await, "pending");
    assert_eq!(rig.registry.queue_position(&id("r1")), Some(0));
    delete(&rig, &busy).await;
    rig.shared.tick().await;
    let host = host_a(&rig).await;
    assert_eq!(&host["lease"], &json!(waiting), "{host}");
}

/// Asserts a restart between the commit and the agent's ready report. The lease is
/// pending with no host in the file; after the crash the first tick presents it again and
/// it is committed to the same host under the same lease id, the only lease there. It
/// catches a committed lease lost with the process, and a second booking for it.
#[tokio::test]
async fn a_committed_lease_is_committed_again_after_a_restart() {
    let rig = Rig::new("request_id_committed").await;
    let waiting = pending(&rig, &whole("r1")).await;
    rig.shared.tick().await;
    assert_eq!(host_a(&rig).await["mark"], "held");
    let rig = rig.restart(true).await;
    rig.shared.tick().await;
    let host = host_a(&rig).await;
    assert_eq!(
        (&host["mark"], &host["lease"]),
        (&json!("held"), &json!(waiting)),
        "{host}"
    );
    assert_eq!(host.get("leases"), None, "{host}");
    assert_eq!(state(&rig, &waiting).await, "pending");
}

/// Asserts acquires that race: two at once under one id and one request make one lease,
/// both answered with it; two at once under one id with different requests make one lease
/// and one `request-id-conflict`; distinct ids make distinct leases. With two requests
/// waiting, a release of the first one's lease moves the second to the front, and the next
/// tick sets the host aside for it. It catches a repeat that races the first acquire into
/// a second lease, and a cancel that disturbs another request.
#[tokio::test]
async fn acquires_at_once_under_one_id_make_one_lease() {
    let rig = Rig::new("request_id_race").await;
    rig.lease().await;
    let (first, again) = (whole("r1"), whole("r1"));
    let (one, two) = tokio::join!(post(&rig, &first), post(&rig, &again));
    assert_eq!((one.0, two.0), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    assert_eq!(one.1, two.1);
    let r1 = one_id(&one.1, &two.1);
    let mut bigger = whole("r2");
    bigger["resources"]["cpu_millis"] = json!(2000);
    let same = whole("r2");
    let (one, two) = tokio::join!(post(&rig, &same), post(&rig, &bigger));
    let mut statuses = [one.0, two.0];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::ACCEPTED, StatusCode::CONFLICT]);
    let r2 = one_id(&one.1, &two.1);
    assert_ne!(r1, r2);
    assert_eq!(rig.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(rig.registry.queue_position(&id("r2")), Some(1));

    delete(&rig, &r1).await;
    assert_eq!(rig.registry.queue_position(&id("r2")), Some(0));
    rig.shared.tick().await;
    assert_eq!(rig.registry.reservations()[0].request, id("r2"));
    assert_eq!(state(&rig, &r2).await, "pending");
}

/// The lease id of whichever of two answers is not a conflict.
fn one_id(one: &Value, two: &Value) -> String {
    let named = [one, two].into_iter().find_map(|body| body["id"].as_str());
    named.unwrap().to_string()
}

/// Asserts a request whose hosts are gone: after a restart the host never says hello
/// again, the request waits, presented at every tick (so it is never dropped as unclaimed),
/// and once no host could serve it for the pool's `regain_grace` the registry refuses it
/// and the tick drops the lease and the request. It catches a waiting lease kept forever
/// for a pool that cannot serve it.
#[tokio::test]
async fn a_request_with_no_host_left_is_dropped_after_regain_grace() {
    let (rig, _busy, waiting) = reserved("request_id_no_host").await;
    let rig = rig.restart(false).await;
    rig.shared.tick().await;
    assert_eq!(state(&rig, &waiting).await, "pending");
    rig.advance(300);
    rig.shared.tick().await;
    assert_eq!(state(&rig, &waiting).await, "pending");
    assert_eq!(rig.registry.queue_position(&id("r1")), Some(0));
    rig.advance(300);
    rig.shared.tick().await;
    assert_eq!(state(&rig, &waiting).await, "gone");
    assert_eq!(rig.registry.queue_position(&id("r1")), None);
}

/// Asserts a presentation that comes later than the pool's `regain_grace` after the last
/// one (a tick far slower than the grace): the registry has let the request go, and the
/// late presentation enters it again. In the same gap the ordinary lease lapsed by its
/// ttl, so that tick also commits the quiet host to the waiting lease: the lease is
/// pending on the host and nothing is left on the wait list. It catches a lease dropped,
/// or left pending with no request, by a slow tick.
#[tokio::test]
async fn a_late_presentation_enters_the_request_again() {
    let (rig, _busy, waiting) = reserved("request_id_late").await;
    rig.advance(601);
    rig.shared.tick().await;
    assert_eq!(state(&rig, &waiting).await, "pending");
    assert_eq!(rig.registry.queue_position(&id("r1")), None);
    assert_eq!(host_a(&rig).await["lease"], json!(waiting));
}
