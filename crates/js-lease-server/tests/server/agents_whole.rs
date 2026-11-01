//! The whole-node class over HTTP: a lease on a registered host is committed and stays
//! pending until the host's agent reports it ready, and a run waits for the same report.
//! (`gpu-host` has not opted into this gate; its tests are the other agent tests.)

use serde_json::{Value, json};

use std::time::Duration;

use crate::agents_support::{JOIN, POOL, Session, agent_args, beat, beat_preparing, join, post};
use crate::runs_support::{queue, run_body, runs};
use crate::support::{Harness, Scratch, T0, lease_body, replace};

/// The pool file of a pool whose class is `whole-node`.
fn whole_pool() -> String {
    POOL.replace("gpu-host", "whole-node")
}

/// The base request, for the `whole-node` class.
fn whole_body() -> Value {
    let mut body = lease_body();
    body["class"] = json!("whole-node");
    body
}

/// The flags of a server whose registered pool serves `whole-node`.
fn args(scratch: &Scratch) -> Vec<std::ffi::OsString> {
    let args = agent_args(scratch, false);
    scratch.write("agent-pool.toml", &whole_pool(), 0o644);
    args
}

/// A server with agent registration on and a `whole-node` pool.
async fn start(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = args(&scratch);
    Harness::with(scratch, args, None).await
}

/// The heartbeat of `session` that reports its host ready for lease `id`.
async fn report_ready(h: &Harness, session: &Session, id: &str) {
    let mut message = session.beat();
    message["exclusive_ready"] = json!(id);
    let ready = post(h, "/v1/agent/heartbeat", Some(JOIN), &message).await;
    assert_eq!(ready.status, 200, "{}", ready.body);
}

/// Asserts the commit over HTTP. A whole-node request is 202 pending and the host's
/// heartbeat reply names the lease in `exclusive`; the lease is not granted at the next
/// tick, only after the agent reports `exclusive_ready` for exactly that lease. It
/// catches a lease granted before the host is prepared and a reply that does not tell
/// the agent what to prepare for.
#[tokio::test]
async fn a_whole_node_lease_is_pending_until_the_agent_reports_ready() {
    let h = start("whole_pending").await;
    let me = join(&h, "burst-1").await;
    let asked = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    let id = asked.json()["id"].as_str().unwrap().to_string();

    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &me.beat()).await;
    assert_eq!(reply.json()["exclusive"], json!(id));
    h.ticks.step().await;
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_ne!(listed["leases"][0]["host"], "burst-1", "{listed}");

    report_ready(&h, &me, &id).await;
    h.ticks.step().await;
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["host"], "burst-1", "{listed}");
}

/// Asserts the run gate of a whole-node lease over HTTP. A lease restored by a restart
/// sits on a host whose agent has said hello again but has not reported `exclusive_ready`
/// for it (the preparation is in memory only): a run is 409 `not-ready` and nothing is
/// queued; once the agent reports ready for exactly that lease the same request is
/// accepted 202. It catches a run queued on an unprepared host, a restored lease that
/// reads ready, and a refusal with no HTTP code.
#[tokio::test]
async fn a_run_waits_for_a_restored_whole_node_lease_to_be_ready() {
    let first = start("whole_not_ready").await;
    let old = join(&first, "burst-1").await;
    let asked = first.call("POST", "/v1/leases", Some(&whole_body())).await;
    let id = asked.json()["id"].as_str().unwrap().to_string();
    report_ready(&first, &old, &id).await;
    first.ticks.step().await;

    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let resumed = replace(args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, resumed, None).await;
    let me = join(&h, "burst-1").await;
    let early = queue(&h, &id, &run_body(1000)).await;
    assert_eq!(early.status, 409, "{}", early.body);
    assert_eq!(early.json(), json!({ "error": "not-ready" }));
    assert!(runs(&h, &id).await.is_empty());

    report_ready(&h, &me, &id).await;
    let accepted = queue(&h, &id, &run_body(1000)).await;
    assert_eq!(accepted.status, 202, "{}", accepted.body);
}

/// A server with agent registration on and a `whole-node` pool that also sets `keys`
/// (pool file lines, ahead of the tables), with a grace period so long that a test agent
/// need not send heartbeats to stay serving.
async fn start_with(name: &str, keys: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = agent_args(&scratch, false);
    let pool = whole_pool().replace("max_machines = 2\n", &format!("max_machines = 2\n{keys}\n"));
    scratch.write("agent-pool.toml", &pool, 0o644);
    Harness::with(scratch, args, None).await
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The state and times of lease `id` as the list shows them.
async fn listed(h: &Harness, id: &str) -> Option<Value> {
    let list = h.call("GET", "/v1/leases", None).await.json();
    let leases = list["leases"].as_array().unwrap();
    leases.iter().find(|lease| lease["id"] == id).cloned()
}

/// Asserts the clock of a whole-node lease over HTTP. The request asks for a 60 s ttl
/// and the agent reports ready 100 s after the commit: the lease stays pending, and is
/// not expired by its ttl, until then; once ready it is held with its expiry one ttl
/// after the report and its end one maximum wall time after the report. An older client
/// that only polls the list sees `pending` and then `held`. It catches a ttl counted
/// from the commit (the lease would be gone), a held lease before the report, and times
/// counted from the commit.
#[tokio::test]
async fn a_whole_node_lease_counts_its_ttl_from_the_ready_report() {
    let h = start_with("whole_clock", "").await;
    let me = join(&h, "burst-1").await;
    let mut body = whole_body();
    body["ttl_seconds"] = json!(60);
    let asked = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    let id = asked.json()["id"].as_str().unwrap().to_string();

    for _ in 0..20 {
        h.clock.advance(secs(5));
        assert_eq!(beat(&h, &me).await.status, 200);
        h.ticks.step().await;
        let lease = listed(&h, &id).await.expect("a pending lease is listed");
        assert_eq!(lease["state"], "pending", "{lease}");
    }
    beat_preparing(&h, &me).await;
    h.ticks.step().await;
    let ready = T0 + 100_000;
    let lease = listed(&h, &id).await.unwrap();
    assert_eq!(lease["state"], "held", "{lease}");
    assert_eq!(lease["host"], "burst-1");
    assert_eq!(lease["expires_at_ms"], ready + 60_000, "{lease}");
    assert_eq!(lease["ends_at_ms"], ready + 7_200_000, "{lease}");
}

/// Asserts that an operator's `prepare_timeout` is the lease table's wait, not only the
/// registry's: with `prepare_timeout = "20m"` a lease that is reported ready after
/// fifteen minutes (past the 600 s default) is held. It catches a pool key that is
/// parsed and never handed to the lease table, which would drop the lease at ten minutes.
#[tokio::test]
async fn an_operator_prepare_timeout_reaches_the_lease_table() {
    let h = start_with(
        "whole_operator_timeout",
        "prepare_timeout = \"20m\"\ngrace = \"2h\"",
    )
    .await;
    let me = join(&h, "burst-1").await;
    let asked = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    let id = asked.json()["id"].as_str().unwrap().to_string();

    h.clock.advance(secs(601));
    h.ticks.step().await;
    let lease = listed(&h, &id).await.expect("still waiting after 601 s");
    assert_eq!(lease["state"], "pending", "{lease}");
    h.clock.advance(secs(299));
    beat_preparing(&h, &me).await;
    h.ticks.step().await;
    let lease = listed(&h, &id).await.unwrap();
    assert_eq!(lease["state"], "held", "{lease}");
}

/// The request `whole_body()` makes, as the registry keys its records.
fn whole_request() -> js_types::Request {
    let resources = js_types::Resources::new(1000, 1024, 1, Some("nvidia-a100".into()));
    js_types::Request {
        class: js_types::ClassId::new("whole-node").unwrap(),
        resources: resources.unwrap(),
        capabilities: [("isa".to_string(), "x86-64-v3".to_string())].into(),
        max_wall: secs(7200),
        labels: [("team".to_string(), "infra".to_string())].into(),
        validation_run: None,
    }
}

/// The registry's record of `whole_request()`.
fn record(h: &Harness) -> Option<js_registry::RequestRecord> {
    h.running
        .registry()
        .unwrap()
        .request_record(&whole_request())
}

/// Posts a whole-node lease that must be answered 202, and returns its id.
async fn ask(h: &Harness) -> String {
    let asked = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    asked.json()["id"].as_str().unwrap().to_string()
}

/// Two heartbeats of `session`: the second follows any stop the first crossed, so the
/// host is serving with nothing held and can take a lease again.
async fn settle(h: &Harness, session: &Session) {
    beat(h, session).await;
    beat(h, session).await;
}

/// Asserts the prepare deadline over HTTP, as the registry sees it, and the second try.
/// Two hosts; the agent of the host that took the lease is silent (it hears of the lease
/// and never reports ready). One second before `prepare_timeout` the lease is pending;
/// the tick at the deadline drops it, and the registry has ended it as a failed prepare:
/// the request has one `prepare_failures` and the silent host is excluded for
/// `reserve_backoff` from the deadline. The same request then goes to the other host. It
/// catches a lease the table stops at its deadline without the registry hearing of a
/// failed prepare (no count, no exclusion), a lease left pending past its deadline, and
/// a retry that picks the host that just failed.
#[tokio::test]
async fn a_silent_agent_fails_the_prepare_and_the_retry_goes_elsewhere() {
    let h = start_with(
        "whole_silent",
        "prepare_timeout = \"60s\"\nreserve_backoff = \"120s\"\ngrace = \"2h\"",
    )
    .await;
    let first = join(&h, "burst-1").await;
    let second = join(&h, "burst-2").await;
    let id = ask(&h).await;
    let named = |reply: &crate::support::Reply| reply.json()["exclusive"] == json!(id);
    let (one, two) = (beat(&h, &first).await, beat(&h, &second).await);
    let failed = if named(&one) { "burst-1" } else { "burst-2" };
    assert!(named(&one) != named(&two), "{} {}", one.body, two.body);

    h.clock.advance(secs(59));
    h.ticks.step().await;
    assert!(listed(&h, &id).await.is_some());
    assert_eq!(record(&h), None);
    h.clock.advance(secs(1));
    h.ticks.step().await;
    assert!(listed(&h, &id).await.is_none(), "dropped at the deadline");
    let kept = record(&h).expect("the failed prepare is recorded");
    assert_eq!(kept.prepare_failures, 1);
    assert!(!kept.terminal);
    let until = js_lease::Timestamp::from_millis(T0 + 60_000 + 120_000);
    let host = js_types::HostId::new(failed).unwrap();
    let excluded = [(host, js_registry::Exclusion::Until(until))];
    assert_eq!(kept.exclusions, excluded.into());
    let reason = kept.last.expect("the failure is recorded").code;
    assert_eq!(reason, "prepare-timeout");

    settle(&h, &first).await;
    settle(&h, &second).await;
    let retry = ask(&h).await;
    let (one, two) = (beat(&h, &first).await, beat(&h, &second).await);
    let (on_first, on_second) = (
        one.json()["exclusive"] == json!(retry),
        two.json()["exclusive"] == json!(retry),
    );
    let went = if on_first { "burst-1" } else { "burst-2" };
    assert!(on_first != on_second, "{} {}", one.body, two.body);
    assert_ne!(went, failed);
}

/// Asserts that the exclusion is what keeps the retry off the silent host: the pool has
/// only that host, so the same request is refused 409 `no-capacity` (nothing is placed
/// and nothing is pending) until `reserve_backoff` has passed since the deadline, and is
/// placed there, 202, from the first moment it has. It catches a failed prepare that
/// excludes nothing (the retry would be placed at once, again and again) and an exclusion
/// that lasts longer or shorter than `reserve_backoff`.
#[tokio::test]
async fn a_silent_host_is_left_out_of_the_retry_for_reserve_backoff() {
    let h = start_with(
        "whole_backoff",
        "prepare_timeout = \"60s\"\nreserve_backoff = \"120s\"\ngrace = \"2h\"",
    )
    .await;
    let me = join(&h, "burst-1").await;
    let id = ask(&h).await;
    assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(id));
    h.clock.advance(secs(60));
    h.ticks.step().await;
    assert!(listed(&h, &id).await.is_none());
    settle(&h, &me).await;

    let refused = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "no-capacity" }));
    h.clock.advance(secs(119));
    settle(&h, &me).await;
    let refused = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(
        h.call("GET", "/v1/leases", None).await.json()["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    h.clock.advance(secs(1));
    let retry = ask(&h).await;
    assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(retry));
}

/// Asserts the bound on silent tries. With `max_replace = 2` and a host whose agent never
/// reports ready, the second missed deadline makes the request terminal with two
/// `prepare_failures`, and the next ask for it is refused 409 and clears the record,
/// even after the exclusion has run out. It catches a deadline that is never counted
/// (the request would be placed on the silent host without bound).
#[tokio::test]
async fn a_request_is_terminal_after_max_replace_silent_prepares() {
    let h = start_with(
        "whole_terminal",
        "prepare_timeout = \"60s\"\nreserve_backoff = \"10s\"\nmax_replace = 2\ngrace = \"2h\"",
    )
    .await;
    let me = join(&h, "burst-1").await;
    for tries in 1..=2 {
        let id = ask(&h).await;
        assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(id));
        h.clock.advance(secs(60));
        h.ticks.step().await;
        assert!(listed(&h, &id).await.is_none());
        let kept = record(&h).unwrap();
        assert_eq!(kept.prepare_failures, tries);
        assert_eq!(kept.terminal, tries == 2);
        h.clock.advance(secs(10));
        settle(&h, &me).await;
    }
    let refused = h.call("POST", "/v1/leases", Some(&whole_body())).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(record(&h), None);
}

/// Asserts that a class booked as a whole host (`gpu-host`) is not gated: on a
/// registered host it is held at once, 201, and the agent is told to prepare nothing.
/// It catches a gate that spreads from the whole-node class to the class that has not
/// opted in.
#[tokio::test]
async fn a_gpu_host_lease_on_a_registered_host_is_held_at_once() {
    let h = crate::agents_support::start_agents("whole_book_all", false).await;
    let me = join(&h, "burst-1").await;
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.status, 201, "{}", granted.body);
    assert_eq!(granted.json()["host"], "burst-1");
    let reply = beat_preparing(&h, &me).await;
    assert_eq!(reply.json().get("exclusive"), None, "{}", reply.body);
}

/// Asserts the whole path of a resumed reservation over HTTP: a lease file whose ledger
/// holds a reservation of `r9` on `burst-1`, due thirty seconds after the boot's clock
/// reading, is resumed with `--boot resume`, and once the agent says hello the host
/// lists as reserved for `r9`, with its deadline raised to the boot plus
/// `RESERVATION_BOOT_GRACE` (not the stored deadline, which is earlier, and not the
/// stored plus the grace). It catches a server that opens the table and never gives the
/// ledger to the registry, and a restart grace that never reaches the table's settings.
#[tokio::test]
async fn a_resumed_lease_file_marks_the_host_with_the_deadline_floored() {
    let scratch = Scratch::new("whole_resume_rows");
    let args = replace(args(&scratch), "--boot", "resume");
    let text = format!(
        "version = 1\nterm = 3\nnext_sequence = 1\nlease_count = 0\nreservation_count = 1\n\n\
         [[reservation]]\nrequest_id = \"r9\"\nhost = \"burst-1\"\nreserved_at_ms = {T0}\n\
         deadline_ms = {}\nqueue_place = 0\nqueued_since_ms = {T0}\ngive_ups = 1\n",
        T0 + 30_000
    );
    std::fs::write(scratch.state(), text).unwrap();
    let h = Harness::with(scratch, args, None).await;
    join(&h, "burst-1").await;

    let listing = crate::agents_support::hosts(&h).await;
    let host = crate::agents_support::entry(&listing, "burst-1");
    assert_eq!(host["mark"], "reserved", "{host}");
    assert_eq!(host["reserved_for"], "r9", "{host}");
    let floor = T0 + js_lease_server::RESERVATION_BOOT_GRACE.as_secs() * 1000;
    assert_eq!(host["drain_deadline_ms"], floor, "{host}");
}

/// Asserts what the reservation ledger leaves out. A request that came over HTTP carries
/// no request id, so the registry keeps its failed prepare under its content, and a
/// record under content is not written to the lease file: after the silent agent has
/// failed the prepare, the registry holds the record and the ledger is empty. It catches
/// a ledger that tries to name a request with no id.
#[tokio::test]
async fn a_failed_prepare_of_a_request_without_an_id_is_not_in_the_ledger() {
    let h = start_with(
        "whole_ledger_content",
        "prepare_timeout = \"60s\"\ngrace = \"2h\"",
    )
    .await;
    let me = join(&h, "burst-1").await;
    let id = ask(&h).await;
    assert_eq!(beat(&h, &me).await.json()["exclusive"], json!(id));
    h.clock.advance(secs(60));
    h.ticks.step().await;
    assert_eq!(
        record(&h)
            .expect("the failure is recorded")
            .prepare_failures,
        1
    );
    assert!(
        h.running
            .registry()
            .unwrap()
            .reservation_ledger()
            .is_empty()
    );
}
