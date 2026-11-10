//! Usage over real sockets: what a heartbeat's load does and does not do, when the sink
//! is flushed and pruned, who may read the series, and what the log says. The sink is an
//! in-memory one (see `usage_support`), so the server's part is what is tested.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_lease_server::{Cli, DropReason, NodeRow, UsageCounters};
use serde_json::json;

use crate::agents_support::{JOIN, JOIN_B, Session, agent_args, join, post};
use crate::job_hosts::{cpu_hello, placing_args};
use crate::jobs_support::{
    JobBody, beat_with, exited, job_agent, job_state, one_start, running, submit_body,
};
use crate::support::{
    Harness, Kept, LogCapture, Reply, Scratch, TOKEN, args_with, base_args, config, logged_with,
    request,
};
use crate::usage_support::{Fake, Seen, beat_load, load, start_over, start_usage};

const MINUTE: Duration = Duration::from_secs(60);

/// The reply to a GET of `path` with the client token.
async fn get(h: &Harness, path: &str) -> crate::support::Reply {
    h.call("GET", path, None).await
}

/// A server that places jobs on CPU hosts (see `job_hosts`) with the fake as its sink.
async fn start_jobs(name: &str, fake: &Arc<Fake>) -> Harness {
    let scratch = Scratch::new(name);
    let args = placing_args(&scratch, Kept::Files);
    start_over(scratch, args, fake).await
}

/// Submits the job `id`, has a tick place it, and returns the allocation id the start the
/// agent `me` is sent names; the job is then running on the host of `me`.
async fn start_job(h: &Harness, me: &Session, id: &str) -> String {
    let queued = submit_body(h, &JobBody::new(id).json()).await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    h.ticks.step().await;
    let (started, allocation) = one_start(&beat_with(h, me, &[]).await);
    assert_eq!(started, id);
    beat_with(h, me, &[running(id)]).await;
    allocation
}

/// Asserts the central promise of the load: a heartbeat whose load is bad (a working set
/// over the cap) is still answered 200, the job result it carries is acknowledged and
/// recorded as ever, the load is counted as rejected, and the sink is never given it. It
/// catches a server that refuses the whole beat for its telemetry, which would lose the
/// results, the job list and the taint report that share the body, and make the agent
/// resend them for ever.
#[tokio::test]
async fn a_beat_with_a_bad_load_still_delivers_its_results() {
    let fake = Arc::new(Fake::default());
    let h = start_jobs("usage_bad_load", &fake).await;
    let me = job_agent(&h, &cpu_hello("burst-1")).await;
    let allocation = start_job(&h, &me, "bad-load").await;
    let mut body = me.beat();
    body["jobs"] = json!([exited("bad-load", &allocation)]);
    let mut bad = load(1, 1000, &[0]);
    bad["working_set_bytes"] = json!(1u64 << 46);
    body["load"] = bad;
    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json()["job_acks"],
        json!([{ "job_id": "bad-load", "ack": "ACCEPTED" }])
    );
    assert_eq!(job_state(&h, "bad-load").await, "JOB_STATE_SUCCEEDED");
    assert_eq!(h.running.usage_counters().loads_rejected, 1);
    assert!(fake.seen().is_empty());
}
/// Asserts that a `load` that is not a load (an object missing its required numbers) is no
/// load: the beat is served, nothing is counted as rejected and the sink is not given a
/// reading. It catches a decode that fails the beat for a field of the telemetry.
#[tokio::test]
async fn a_load_that_is_not_a_load_is_no_load() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_no_load", &fake).await;
    let me = join(&h, "burst-1").await;
    let reply = beat_load(&h, &me, json!({ "mono_usec": "x" })).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(h.running.usage_counters(), UsageCounters::default());
    assert!(fake.seen().is_empty());
}

/// Asserts that a beat the registry refuses records nothing: a stale epoch (409), a wrong
/// session (403) and an unknown host (409) each carry a good load and none reaches the
/// sink, while the next good beat of the host does, with its host, epoch and sequence. It
/// catches the hook placed before the registry's checks, which would let a host that is
/// not who it says it is write another's series.
#[tokio::test]
async fn a_beat_the_registry_refuses_records_nothing() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_refused", &fake).await;
    let me = join(&h, "burst-1").await;
    let mut stale = me.clone();
    stale.epoch += 1;
    let mut forged = me.clone();
    forged.secret = "0".repeat(64);
    let mut ghost = me.clone();
    ghost.host = "ghost".to_string();
    for (session, status) in [(&stale, 409), (&forged, 403), (&ghost, 409)] {
        let reply = beat_load(&h, session, load(1, 1000, &[0])).await;
        assert_eq!(reply.status, status, "{}", reply.body);
    }
    assert!(fake.seen().is_empty());
    let reply = beat_load(&h, &me, load(7, 1000, &[0])).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let seen = Seen {
        host: "burst-1".to_string(),
        epoch: me.epoch,
        seq: 7,
        minute: 16,
    };
    assert_eq!(fake.seen(), vec![seen]);
}

/// Asserts when the sink is flushed and pruned: the prune at the first tick, the flush
/// once five minutes have passed by the server's clock and not at four, the prune again
/// when the day changes, and one more flush at shutdown whatever the schedule says. It
/// catches a flush on every tick (a write per five seconds), a flush that reads the real
/// clock, a prune that never repeats, and a shutdown that drops the unwritten minutes.
#[tokio::test]
async fn flush_is_every_five_minutes_and_at_shutdown_and_prune_is_daily() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_schedule", &fake).await;
    h.ticks.step().await;
    assert_eq!((fake.flushes(), fake.prunes()), (vec![], vec![16]));
    h.clock.advance(4 * MINUTE);
    h.ticks.step().await;
    assert_eq!((fake.flushes(), fake.prunes()), (vec![], vec![16]));
    h.clock.advance(MINUTE);
    h.ticks.step().await;
    assert_eq!((fake.flushes(), fake.prunes()), (vec![21], vec![16]));
    h.clock.advance(Duration::from_secs(24 * 3600));
    h.ticks.step().await;
    assert_eq!(
        (fake.flushes(), fake.prunes()),
        (vec![21, 1461], vec![16, 1461])
    );
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    assert_eq!(fake.flushes(), vec![21, 1461, 1461]);
    drop(scratch);
}

/// Asserts the log: one `node minute` line for each closed minute of each node (two nodes,
/// three minutes, six lines), written when the minutes are flushed and not once for each
/// of the twelve beats that made them. The beats are handled on the pool for blocking
/// calls, so the lines are read from the process-wide subscriber, which sees that pool.
/// It catches a line per heartbeat, which at twelve beats a minute a node would drown the
/// log, and a minute logged twice.
#[tokio::test]
async fn each_closed_node_minute_is_logged_once_and_not_for_each_beat() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_log", &fake).await;
    let (a, b) = (join(&h, "logged-1").await, join(&h, "logged-2").await);
    for seq in 1..=3u64 {
        for twice in [0, 10] {
            beat_load(&h, &a, load(seq + twice, seq * 1_000_000, &[])).await;
            beat_load(&h, &b, load(seq + twice, seq * 2_000_000, &[])).await;
        }
        h.clock.advance(MINUTE);
    }
    let minute_lines = || {
        let mut lines = logged_with("node minute");
        lines.retain(|line| line.contains("host=logged-"));
        lines
    };
    assert_eq!(minute_lines().len(), 0, "{:?}", minute_lines());
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let lines = minute_lines();
    assert_eq!(lines.len(), 6, "{lines:?}");
    let text = lines.join("\n");
    for needle in [
        "host=logged-1",
        "host=logged-2",
        "minute=16",
        "minute=18",
        "cpu_millis=3000",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    drop(scratch);
}

/// Asserts that readings the sink drops are counted and warned about once for a host and
/// minute, with the reasons and their counts, whatever the number of beats: three dropped
/// readings in one minute are one warning that says `no-baseline=3`. It catches a warning
/// for each beat (the log flooded by one lying node) and one that loses the reason.
#[tokio::test]
async fn dropped_readings_are_warned_once_for_a_host_and_minute() {
    let log = LogCapture::start();
    let fake = Arc::new(Fake::default());
    fake.drop_readings(DropReason::NoBaseline);
    let h = start_usage("usage_drops", &fake).await;
    let me = join(&h, "burst-1").await;
    for seq in 1..=3 {
        beat_load(&h, &me, load(seq, 1000, &[])).await;
    }
    assert_eq!(h.running.usage_counters().loads_dropped, 3);
    h.clock.advance(MINUTE);
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let text = log.text();
    assert_eq!(text.matches("node readings dropped").count(), 1, "{text}");
    assert!(text.contains("no-baseline=3"), "{text}");
    drop(scratch);
}

/// Asserts the flags: `--usage-db` defaults to `usage.db` beside `--state-file`, a given
/// path is kept, `--usage-off` leaves no file, `--profile-store-bytes` defaults to 256 MiB,
/// `--history-retention-days` to 90, each takes a positive number and refuses zero and
/// text, and `--usage-off` is refused together with `--usage-db`. It catches a default
/// that lands in the working directory, a zero budget that would evict every profile it
/// learns, a zero retention that would prune every record the day it is written, and two
/// flags that contradict.
#[test]
fn the_flags_decide_where_usage_lives_and_how_much_it_may_keep() {
    let scratch = Scratch::new("usage_flags");
    let default = config(base_args(&scratch)).unwrap();
    assert_eq!(default.usage_db(), Some(scratch.path("usage.db").as_path()));
    assert_eq!(default.profile_store_bytes(), 268_435_456);
    assert_eq!(default.history_retention_days(), 90);
    let given = [
        "--usage-db",
        "/var/lib/js/u.db",
        "--profile-store-bytes",
        "1",
        "--history-retention-days",
        "1",
    ];
    let given = config(args_with(&scratch, &given)).unwrap();
    assert_eq!(given.usage_db(), Some(Path::new("/var/lib/js/u.db")));
    assert_eq!(given.profile_store_bytes(), 1);
    assert_eq!(given.history_retention_days(), 1);
    let off = config(args_with(&scratch, &["--usage-off"])).unwrap();
    assert_eq!(off.usage_db(), None);
    let refused: [&[&str]; 6] = [
        &["--profile-store-bytes", "0"],
        &["--profile-store-bytes", "lots"],
        &["--profile-store-bytes", "-1"],
        &["--history-retention-days", "0"],
        &["--history-retention-days", "90d"],
        &["--usage-off", "--usage-db", "u.db"],
    ];
    for extra in refused {
        let parsed = Cli::try_parse_from(args_with(&scratch, extra));
        assert!(parsed.is_err(), "{extra:?}");
    }
}

/// Asserts `--usage-off`: the routes do not exist (404 for the client token), heartbeats
/// with a load are served as ever and reach no sink, and nothing is counted. It catches an
/// off switch that still records, and one that breaks heartbeats.
#[tokio::test]
async fn with_usage_off_there_are_no_routes_and_nothing_is_recorded() {
    let fake = Arc::new(Fake::default());
    let scratch = Scratch::new("usage_off");
    let mut args = agent_args(&scratch);
    args.push("--usage-off".into());
    let h = start_over(scratch, args, &fake).await;
    let me = join(&h, "burst-1").await;
    let reply = beat_load(&h, &me, load(1, 1000, &[0])).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(get(&h, "/v1/usage/nodes/burst-1").await.status, 404);
    assert_eq!(get(&h, "/v1/usage/keys/act.build").await.status, 404);
    h.ticks.step().await;
    assert!(fake.seen().is_empty());
    assert_eq!((fake.flushes(), fake.prunes()), (vec![], vec![]));
    assert_eq!(h.running.usage_counters(), UsageCounters::default());
    assert!(h.running.usage_curves().is_none());
}

/// Asserts that the minutes of heartbeats are written only by a flush, and as rows of the
/// minutes they were taken in: two readings in two minutes are two rows with the node's CPU,
/// working set and lease count, in minute order, and none exists before the flush. It
/// catches rows written (or lost) at the wrong moment, and a row for the wrong minute.
#[tokio::test]
async fn the_minutes_of_beats_are_written_as_rows_by_a_flush() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_rows", &fake).await;
    let me = join(&h, "burst-1").await;
    beat_load(&h, &me, load(1, 5_000_000, &[])).await;
    h.clock.advance(MINUTE);
    beat_load(&h, &me, load(2, 8_000_000, &[])).await;
    h.ticks.step().await;
    assert!(fake.written("burst-1").is_empty());
    h.clock.advance(4 * MINUTE);
    h.ticks.step().await;
    let row = |minute, cpu_millis| NodeRow {
        minute,
        cpu_millis,
        ws_max_bytes: 1_000_000,
        leases: 1,
    };
    assert_eq!(fake.written("burst-1"), vec![row(16, 5000), row(17, 8000)]);
    assert!(fake.written("burst-2").is_empty());
}

/// Asserts that a sink that fails costs a count for each failed call and nothing else: a
/// prune that fails at the first tick is one fault, the ticks go on, and `/healthz` still
/// says the loop runs. It catches a failure that unwinds into the tick loop and stops the
/// server expiring leases.
#[tokio::test]
async fn a_failing_sink_is_counted_and_the_ticks_go_on() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_faults", &fake).await;
    fake.fail();
    h.ticks.step().await;
    h.clock.advance(5 * MINUTE);
    h.ticks.step().await;
    assert_eq!(h.running.usage_counters().faults, 2);
    assert_eq!(get(&h, "/healthz").await.status, 200);
}

/// Asserts that the minutes of heartbeats reach the read API only once a flush closed
/// them: two readings in two minutes are rows of those minutes with the node's CPU,
/// working set and lease count, in minute order, and the rows of minutes outside the asked
/// range are not returned. It catches rows served before they are closed and a range that
/// is not applied.
#[tokio::test]
async fn the_minutes_of_beats_are_rows_after_a_flush() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_api_rows", &fake).await;
    let me = join(&h, "burst-1").await;
    beat_load(&h, &me, load(1, 5_000_000, &[])).await;
    h.clock.advance(MINUTE);
    beat_load(&h, &me, load(2, 8_000_000, &[])).await;
    assert_eq!(
        get(&h, "/v1/usage/nodes/burst-1").await.json()["rows"],
        json!([])
    );
    h.clock.advance(4 * MINUTE);
    h.ticks.step().await;
    let reply = get(&h, "/v1/usage/nodes/burst-1?from=16&to=17").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let row = |minute, cpu| json!({ "minute": minute, "cpu_millis": cpu, "ws_bytes": 1_000_000, "leases": 1 });
    assert_eq!(reply.json()["host"], "burst-1");
    assert_eq!(reply.json()["rows"], json!([row(16, 5000), row(17, 8000)]));
    assert_eq!(reply.json()["next"], json!(null));
    let one = get(&h, "/v1/usage/nodes/burst-1?from=17&to=17").await;
    assert_eq!(one.json()["rows"], json!([row(17, 8000)]));
}

/// Asserts paging and the default window: an answer holds at most 1,440 rows and `next`
/// is the minute to ask from for the rest (null at the end); with no range the answer is
/// the last 60 minutes up to now, both ends included; and a range that ends before it
/// begins is a 400. It catches a page of 1,439 or 1,441, a `next` that skips or repeats a
/// minute, and a default of 59 or 61 minutes.
#[tokio::test]
async fn rows_come_in_pages_and_the_default_is_the_last_hour() {
    let fake = Arc::new(Fake::default());
    fake.seed_rows("burst-1", 0..1500);
    let h = start_usage("usage_paging", &fake).await;
    let first = get(&h, "/v1/usage/nodes/burst-1?from=0&to=2000")
        .await
        .json();
    assert_eq!(first["rows"].as_array().unwrap().len(), 1440);
    assert_eq!(
        (first["next"].clone(), first["from"].clone()),
        (json!(1440), json!(0))
    );
    assert_eq!(first["to"], 2000);
    let rest = get(&h, "/v1/usage/nodes/burst-1?from=1440&to=2000")
        .await
        .json();
    assert_eq!(rest["rows"].as_array().unwrap().len(), 60);
    assert_eq!(rest["next"], json!(null));
    h.clock.advance(984 * MINUTE);
    let last = get(&h, "/v1/usage/nodes/burst-1").await.json();
    let rows = last["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 60);
    assert_eq!(
        (rows[0]["minute"].clone(), rows[59]["minute"].clone()),
        (json!(941), json!(1000))
    );
    assert_eq!(
        rows[0],
        json!({ "minute": 941, "cpu_millis": 1000, "ws_bytes": 2048, "leases": 1 })
    );
    let backwards = get(&h, "/v1/usage/nodes/burst-1?from=9&to=8").await;
    assert_eq!(backwards.status, 400);
    let nobody = get(&h, "/v1/usage/nodes/nobody").await;
    assert_eq!(
        (nobody.status, nobody.json()["rows"].clone()),
        (200, json!([]))
    );
}

/// Asserts the access rule of both routes: no token, a wrong token and either join token
/// get a 401 with an empty body; the client token gets the data (an empty list for a node
/// with no rows, 404 `unknown-key` for a key never seen, and the profile for one that was);
/// another method is a 405; an unknown path under `/v1/usage` is a 404 for the client
/// token and a 401 without it. It catches a join token that may read what clients may
/// read, a route left open, and a wrong method or path answered as if it were a read.
#[tokio::test]
async fn the_read_routes_are_for_the_client_token_only() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_auth", &fake).await;
    let paths = ["/v1/usage/nodes/burst-1", "/v1/usage/keys/act.build"];
    for path in paths {
        for token in [None, Some(JOIN), Some(JOIN_B), Some("nope")] {
            let reply = request(h.addr(), "GET", path, token, None).await;
            assert_eq!(reply.status, 401, "{path} {token:?}");
            assert_eq!(reply.body, "");
        }
        for method in ["POST", "PUT", "DELETE"] {
            let reply = request(h.addr(), method, path, Some(TOKEN), None).await;
            assert_eq!(reply.status, 405, "{method} {path}");
        }
    }
    let nodes = get(&h, paths[0]).await;
    assert_eq!(nodes.status, 200, "{}", nodes.body);
    let missing = get(&h, paths[1]).await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json(), json!({ "error": "unknown-key" }));
    fake.learn("act.build", 1);
    let known = get(&h, paths[1]).await;
    assert_eq!(known.status, 200, "{}", known.body);
    for path in ["/v1/usage", "/v1/usage/other"] {
        assert_eq!(get(&h, path).await.status, 404, "{path}");
        let bare = request(h.addr(), "GET", path, None, None).await;
        assert_eq!(bare.status, 401, "{path}");
    }
}

/// Asserts that an id, a key or a query that is not exactly one is a 400 whose answer and
/// whose log hold none of the text sent: `..`, `.`, escapes (`%2e`, `%2f`), a slash, a
/// capital, a segment one over its cap, a leading dot or dash in a key, a bad number, a
/// repeated or unknown parameter, and a range backwards; while the longest good id and key
/// are served. It catches a path trick reaching a store key, and an error or a log line
/// that repeats the request, which would carry a client's text into the logs.
#[tokio::test]
async fn ids_keys_and_queries_are_read_strictly_and_never_echoed() {
    let log = LogCapture::start();
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_strict", &fake).await;
    let (long_id, long_key) = ("a".repeat(64), "k".repeat(129));
    let bad_ids = [
        "..",
        ".",
        "%2e",
        "%2e%2e",
        "a/b",
        "a%2fb",
        "UPPER",
        "Zq9Leak",
        long_id.as_str(),
    ];
    for id in bad_ids {
        let reply = get(&h, &format!("/v1/usage/nodes/{id}")).await;
        assert_eq!(reply.status, 400, "{id}");
        assert_eq!(reply.json()["error"], "bad-request", "{id}");
        assert!(!reply.body.contains("Zq9"), "{id}");
    }
    let bad_keys = [
        "..",
        ".",
        ".hidden",
        "-x",
        "%2e",
        "a/b",
        "UPPER",
        "Zq9Leak",
        long_key.as_str(),
    ];
    for key in bad_keys {
        let reply = get(&h, &format!("/v1/usage/keys/{key}")).await;
        assert_eq!(reply.status, 400, "{key}");
        assert!(!reply.body.contains("Zq9"), "{key}");
    }
    let bad_queries = [
        "from=Zq9Leak",
        "to=Zq9Leak",
        "Zq9Leak=1",
        "from=1&from=2",
        "from=5&to=4",
        "from=-1",
        "from=",
        "from",
    ];
    for query in bad_queries {
        let reply = get(&h, &format!("/v1/usage/nodes/burst-1?{query}")).await;
        assert_eq!(reply.status, 400, "{query}");
        assert!(!reply.body.contains("Zq9"), "{query}");
    }
    let good = get(&h, &format!("/v1/usage/nodes/{}", "a".repeat(63))).await;
    assert_eq!(good.status, 200);
    let key = get(&h, &format!("/v1/usage/keys/{}", "k".repeat(128))).await;
    assert_eq!(key.status, 404);
    let text = log.text();
    assert!(!text.contains("Zq9"), "{text}");
    assert!(!text.contains("%2"), "{text}");
}

/// Asserts that a store that fails costs a 503 `usage-unavailable` on the read routes and
/// a count each, and that a failing prune at a tick does not stop the server ticking. It
/// catches a failure answered as an empty result (a quiet node and a broken store looking
/// alike) and one that panics a handler.
#[tokio::test]
async fn a_failing_store_is_a_503_and_is_counted() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_store_faults", &fake).await;
    fake.fail();
    for path in ["/v1/usage/nodes/burst-1", "/v1/usage/keys/act.build"] {
        let reply = get(&h, path).await;
        assert_eq!(reply.status, 503, "{path}");
        assert_eq!(reply.json(), json!({ "error": "usage-unavailable" }));
    }
    h.ticks.step().await;
    assert_eq!(h.running.usage_counters().faults, 3);
    assert_eq!(get(&h, "/healthz").await.status, 200);
}

/// Asserts that a reading handed to the sink is handled off the async thread: while the
/// sink holds one beat's reading (it waits for the test to release it), `/healthz` is
/// answered, another host's beat with its own reading is served, and the held beat is
/// answered 200 once released. It catches a sink called on the server's async thread, where
/// a slow store write would stop every other request, the ticks and the health check.
#[tokio::test]
async fn a_reading_held_in_the_sink_does_not_stop_other_requests() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_parked", &fake).await;
    let (slow, quick) = (join(&h, "burst-1").await, join(&h, "burst-2").await);
    let (mut arrived, release) = fake.park_next_reading();
    let mut parked = Box::pin(beat_load(&h, &slow, load(1, 1000, &[])));
    // The reading must reach the sink. If the beat is answered first it never will, and the
    // test says so rather than wait for an arrival that cannot come.
    tokio::select! {
        biased;
        got = arrived.recv() => assert!(got.is_some(), "the gate closed before the reading arrived"),
        reply = &mut parked => panic!(
            "the beat was answered {} before its reading reached the sink: {}",
            reply.status, reply.body
        ),
    }
    let meanwhile = async {
        assert_eq!(get(&h, "/healthz").await.status, 200);
        let other = beat_load(&h, &quick, load(1, 1000, &[])).await;
        assert_eq!(other.status, 200, "{}", other.body);
        release.send(()).unwrap();
    };
    let (reply, ()) = tokio::join!(&mut parked, meanwhile);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(!fake.read_inline());
    assert_eq!(fake.seen().len(), 2);
}

/// Asserts that a sink that panics on a reading costs the beat nothing: the beat that
/// carries both the load and a job result is answered 200 with the result accepted, and the
/// panic is counted as a fault. It catches a panic that fails the reply after the registry
/// accepted the beat, which makes the agent send the result again to be told it is a
/// duplicate.
#[tokio::test]
async fn a_sink_that_panics_on_a_reading_costs_the_beat_nothing() {
    let fake = Arc::new(Fake::default());
    let h = start_jobs("usage_panic", &fake).await;
    let me = job_agent(&h, &cpu_hello("burst-1")).await;
    let allocation = start_job(&h, &me, "panics").await;
    fake.panic_on_readings();
    let mut body = me.beat();
    body["jobs"] = json!([exited("panics", &allocation)]);
    body["load"] = load(1, 1000, &[0]);
    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json()["job_acks"],
        json!([{ "job_id": "panics", "ack": "ACCEPTED" }])
    );
    assert_eq!(h.running.usage_counters().faults, 1);
}
/// Asserts that the server as started by its flags records into a file and offers admission
/// the curves: the file `usage.db` appears beside the state file, `usage_curves` is there,
/// and a sink supplied by a test that learns none offers none. It catches a server that
/// starts with the sink that records nothing, and curves that admission cannot reach.
#[tokio::test]
async fn the_started_server_keeps_a_usage_file_and_offers_curves() {
    let scratch = Scratch::new("usage_file");
    let db = scratch.path("usage.db");
    let args = agent_args(&scratch);
    let h = Harness::over(scratch, config(args).unwrap()).await;
    assert!(db.exists());
    assert!(h.running.usage_curves().is_some());
    let fake = Arc::new(Fake::default());
    let other = start_usage("usage_no_curves", &fake).await;
    assert!(other.running.usage_curves().is_none());
}

/// Asserts the routes over the real store, with no sink supplied: two beats a minute
/// apart are rows of their minutes after a flush, in the usage file the flags name, and
/// are served from it in minute order, within the asked range; a key nothing was learned
/// under is 404 `unknown-key`. It catches routes that read the wrong store (or a sink
/// that records but cannot read back), a range not applied by the stored series, and a
/// missing key answered as an empty profile.
#[tokio::test]
async fn the_started_server_serves_the_rows_it_flushed_to_its_file() {
    let scratch = Scratch::new("usage_file_rows");
    let args = agent_args(&scratch);
    let h = Harness::over(scratch, config(args).unwrap()).await;
    let me = join(&h, "burst-1").await;
    beat_load(&h, &me, load(1, 5_000_000, &[])).await;
    h.clock.advance(MINUTE);
    beat_load(&h, &me, load(2, 8_000_000, &[])).await;
    h.clock.advance(4 * MINUTE);
    h.ticks.step().await;
    let all = get(&h, "/v1/usage/nodes/burst-1?from=0&to=100").await;
    assert_eq!(all.status, 200, "{}", all.body);
    let minutes = |reply: &Reply| -> Vec<u64> {
        let rows = reply.json()["rows"].clone();
        let rows = rows.as_array().unwrap().clone();
        rows.iter().map(|r| r["minute"].as_u64().unwrap()).collect()
    };
    let served = minutes(&all);
    assert!(!served.is_empty(), "{}", all.body);
    assert!(
        served.windows(2).all(|pair| pair[0] < pair[1]),
        "{served:?}"
    );
    let last = *served.last().unwrap();
    let from_last = get(
        &h,
        &format!("/v1/usage/nodes/burst-1?from={last}&to={last}"),
    )
    .await;
    assert_eq!(minutes(&from_last), vec![last]);
    let after = get(
        &h,
        &format!("/v1/usage/nodes/burst-1?from={}&to=100", last + 1),
    )
    .await;
    assert_eq!(minutes(&after), Vec::<u64>::new());
    let other = get(&h, "/v1/usage/nodes/burst-2?from=16&to=17").await;
    assert_eq!(other.json()["rows"], json!([]));
    let key = get(&h, "/v1/usage/keys/act.build").await;
    assert_eq!(key.status, 404);
    assert_eq!(key.json(), json!({ "error": "unknown-key" }));
}
