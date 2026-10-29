//! Usage over real sockets: what a heartbeat's load does and does not do, when the sink
//! is flushed and pruned, who may read the series, and what the log says. The sink is an
//! in-memory one (see `usage_support`), so the server's part is what is tested.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_lease_server::{Cli, DropReason, KeySource, NodeRow, ProfileKey, RunMinute, UsageCounters};
use js_types::LeaseId;
use js_types::sched::WorkKey;
use js_types::sched::usage::CurveSource;
use serde_json::{Value, json};

use crate::agents_support::{JOIN, JOIN_B, Session, agent_args, join, post};
use crate::runs_support::{beat, held, lease, queued, result, verdicts};
use crate::support::{
    Harness, LogCapture, Reply, Scratch, TOKEN, args_with, base_args, config, logged_with, request,
};
use crate::usage_support::{Fake, Seen, beat_load, load, start_over, start_usage};

const MINUTE: Duration = Duration::from_secs(60);

/// The reply to a GET of `path` with the client token.
async fn get(h: &Harness, path: &str) -> crate::support::Reply {
    h.call("GET", path, None).await
}

/// Drives one run of lease `1-1` on the host of `me` to a clean end: queued, started,
/// running with a load that carries minute 0 of the lease, exited with a result that is
/// accepted, and then the two beats of grace. The lease is the first this server grants.
async fn finish_run(h: &Harness, me: &Session) {
    beat(h, me, &[], &[]).await;
    let lease = lease(h).await;
    let run = queued(h, &lease, 1000).await;
    beat(h, me, &[], &[]).await;
    let mut running = me.beat();
    running["held"] = json!([held(&run, "running")]);
    running["load"] = load(1, 1_000_000, &[0]);
    let sent = post(h, "/v1/agent/heartbeat", Some(JOIN), &running).await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    let reply = beat(
        h,
        me,
        &[held(&run, "exited_unacked")],
        &[result(&run, &lease)],
    )
    .await;
    assert_eq!(verdicts(&reply), [format!("{run} ACCEPTED")]);
    beat(h, me, &[], &[]).await;
    beat(h, me, &[], &[]).await;
}

/// Asserts the central promise of the load: a heartbeat whose load is bad (a working set
/// over the cap) is still answered 200, its result is acknowledged and recorded as ever,
/// the load is counted as rejected, and the sink is never given it. It catches a server
/// that refuses the whole beat for its telemetry, which would lose the results, the held
/// list and the taint report that share the body, and make the agent resend them for ever.
#[tokio::test]
async fn a_beat_with_a_bad_load_still_delivers_its_results() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_bad_load", &fake, &[]).await;
    let me = join(&h, "burst-1").await;
    beat(&h, &me, &[], &[]).await;
    let lease = lease(&h).await;
    let run = queued(&h, &lease, 1000).await;
    beat(&h, &me, &[], &[]).await;
    beat(&h, &me, &[held(&run, "running")], &[]).await;
    let mut body = me.beat();
    body["held"] = json!([held(&run, "exited_unacked")]);
    body["results"] = json!([result(&run, &lease)]);
    let mut bad = load(1, 1000, &[0]);
    bad["working_set_bytes"] = json!(1u64 << 46);
    body["load"] = bad;
    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(verdicts(&reply.json()), ["run-1-1 ACCEPTED"]);
    let listing = get(&h, &format!("/v1/leases/{lease}/runs")).await;
    assert_eq!(listing.json()["runs"][0]["outcome"], "succeeded");
    assert_eq!(h.running.usage_counters().loads_rejected, 1);
    assert!(fake.seen().is_empty());
}

/// Asserts that a `load` that is not a load (an object missing its required numbers) is no
/// load: the beat is served, nothing is counted as rejected and the sink is not given a
/// reading. It catches a decode that fails the beat for a field of the telemetry.
#[tokio::test]
async fn a_load_that_is_not_a_load_is_no_load() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_no_load", &fake, &[]).await;
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
    let h = start_usage("usage_refused", &fake, &[]).await;
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
    let h = start_usage("usage_schedule", &fake, &[]).await;
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

/// Asserts that without a key source of its own the server learns nothing from a clean
/// run and says so: the run is counted as `runs_without_key`, the sink is not given it,
/// and the reads still work. It catches a server that silently learns under a made-up key
/// and one that fails the run for lacking one.
#[tokio::test]
async fn without_a_key_source_runs_are_counted_and_nothing_is_learned() {
    let fake = Arc::new(Fake::default());
    let scratch = Scratch::new("usage_nokeys");
    let args = agent_args(&scratch, false);
    let config = config(args).unwrap().with_usage_sink(fake.clone());
    let h = Harness::over(scratch, config).await;
    let me = join(&h, "burst-1").await;
    finish_run(&h, &me).await;
    assert!(fake.ended().is_empty());
    assert_eq!(h.running.usage_counters().runs_without_key, 1);
    assert_eq!(get(&h, "/v1/usage/nodes/burst-1").await.status, 200);
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
    let h = start_usage("usage_log", &fake, &[]).await;
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
    let h = start_usage("usage_drops", &fake, &[]).await;
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
/// path is kept, `--usage-off` leaves no file, and `--profile-store-bytes` defaults to
/// 256 MiB, takes a positive number, and refuses zero, text, and `--usage-off` together
/// with `--usage-db`. It catches a default that lands in the working directory, a zero
/// budget that would evict every profile it learns, and two flags that contradict.
#[test]
fn the_flags_decide_where_usage_lives_and_how_much_it_may_keep() {
    let scratch = Scratch::new("usage_flags");
    let default = config(base_args(&scratch)).unwrap();
    assert_eq!(default.usage_db(), Some(scratch.path("usage.db").as_path()));
    assert_eq!(default.profile_store_bytes(), 268_435_456);
    let given = [
        "--usage-db",
        "/var/lib/js/u.db",
        "--profile-store-bytes",
        "1",
    ];
    let given = config(args_with(&scratch, &given)).unwrap();
    assert_eq!(given.usage_db(), Some(Path::new("/var/lib/js/u.db")));
    assert_eq!(given.profile_store_bytes(), 1);
    let off = config(args_with(&scratch, &["--usage-off"])).unwrap();
    assert_eq!(off.usage_db(), None);
    let refused: [&[&str]; 4] = [
        &["--profile-store-bytes", "0"],
        &["--profile-store-bytes", "lots"],
        &["--profile-store-bytes", "-1"],
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
    let mut args = agent_args(&scratch, false);
    args.push("--usage-off".into());
    let h = start_over(scratch, args, &fake, &[]).await;
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
    let h = start_usage("usage_rows", &fake, &[]).await;
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

/// Asserts that a clean run is handed to the sink by the real routes: queued, started,
/// running with a load that carries minute 0 of its lease, and then exited with a result
/// the registry accepts; two beats later the sink is given the key the key source named,
/// the lease and the minutes the beats carried, once. It catches a clean run that never
/// reaches the sink.
#[tokio::test]
async fn a_clean_run_reaches_the_sink_after_its_grace_with_its_minutes() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_clean", &fake, &[("run-1-1", "act.build")]).await;
    let me = join(&h, "burst-1").await;
    finish_run(&h, &me).await;
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].key.as_str(), "act.build");
    assert_eq!(ended[0].lease, LeaseId::new(1, 1));
    let minute = RunMinute {
        minute: 0,
        cores_milli: 1500,
        ws_bytes: 4096,
    };
    assert_eq!(ended[0].series.minutes, vec![minute]);
    assert_eq!(h.running.usage_counters().runs_learned, 1);
}

/// Asserts that a sink that fails costs a count for each failed call and nothing else: a
/// prune that fails at the first tick is one fault, the ticks go on, and `/healthz` still
/// says the loop runs. It catches a failure that unwinds into the tick loop and stops the
/// server expiring leases.
#[tokio::test]
async fn a_failing_sink_is_counted_and_the_ticks_go_on() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_faults", &fake, &[]).await;
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
    let h = start_usage("usage_api_rows", &fake, &[]).await;
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
    let h = start_usage("usage_paging", &fake, &[]).await;
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
    let h = start_usage("usage_auth", &fake, &[]).await;
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
    let h = start_usage("usage_strict", &fake, &[]).await;
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

/// Asserts that a key shows no curve below three runs and a curve at three, the third
/// run being a real one: a clean run (accepted `succeeded`) of the lease, named by the key
/// source, is handed to the sink after its grace with the lease, the key and the minutes
/// the beats carried. It catches a clean run that never reaches the sink and a curve
/// offered too early.
#[tokio::test]
async fn a_key_has_a_curve_at_three_runs_the_third_being_a_real_run() {
    let fake = Arc::new(Fake::default());
    fake.learn("act.build", 2);
    let h = start_usage("usage_curve", &fake, &[("run-1-1", "act.build")]).await;
    let before = get(&h, "/v1/usage/keys/act.build").await.json();
    assert_eq!(
        (before["runs"].clone(), before["curve"].clone()),
        (json!(2), json!(null))
    );
    let me = join(&h, "burst-1").await;
    finish_run(&h, &me).await;
    let after = get(&h, "/v1/usage/keys/act.build").await.json();
    assert_eq!(after["runs"], 3);
    let point = json!({ "minute": 0, "cores_milli": 1500, "ws_bytes": 4096 });
    assert_eq!(after["curve"], json!([point]));
    assert_eq!(
        (after["last_used_day"].clone(), after["stale"].clone()),
        (json!(0), json!(false))
    );
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].key.as_str(), "act.build");
    assert_eq!(ended[0].lease, LeaseId::new(1, 1));
    let minute = RunMinute {
        minute: 0,
        cores_milli: 1500,
        ws_bytes: 4096,
    };
    assert_eq!(ended[0].series.minutes, vec![minute]);
    assert_eq!(h.running.usage_counters().runs_learned, 1);
}

/// Asserts that without agent registration there is no usage side: the paths are unknown
/// routes. It catches routes mounted over nothing.
#[tokio::test]
async fn without_agents_the_usage_routes_do_not_exist() {
    let h = Harness::new("usage_static").await;
    assert_eq!(get(&h, "/v1/usage/nodes/gpu-01").await.status, 404);
}

/// Asserts that a store that fails costs a 503 `usage-unavailable` on the read routes and
/// a count each, and that a failing prune at a tick does not stop the server ticking. It
/// catches a failure answered as an empty result (a quiet node and a broken store looking
/// alike) and one that panics a handler.
#[tokio::test]
async fn a_failing_store_is_a_503_and_is_counted() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_store_faults", &fake, &[]).await;
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
    let h = start_usage("usage_parked", &fake, &[]).await;
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

/// Asserts that minutes are filed only for leases the registry holds for the host that sends
/// them: a host sends minutes for a lease the registry holds for another host and for a
/// lease nobody holds, `minutes_unheld` counts both, and the series of the lease's own host
/// is what that host sent. It catches a server that lets any host's minutes into any
/// lease's series, which would let one node teach admission another run's cost.
#[tokio::test]
async fn minutes_for_a_lease_held_by_another_host_or_by_none_are_not_filed() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_unheld", &fake, &[("run-1-1", "act.build")]).await;
    let me = join(&h, "burst-1").await;
    beat(&h, &me, &[], &[]).await;
    let lease = lease(&h).await;
    assert_eq!(lease, "1-1");
    let other = join(&h, "burst-2").await;
    let minute = |lease: &str, minute: u16| {
        json!({
            "lease_id": lease, "minute": minute, "cpu_mean_millis": 9000,
            "cpu_peak_window_millis": 9500, "ws_max_bytes": 999_999,
        })
    };
    let mut foreign = load(1, 1000, &[]);
    foreign["covered"] = json!([]);
    foreign["minutes"] = json!([minute("1-1", 1), minute("9-9", 0)]);
    let reply = beat_load(&h, &other, foreign).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(h.running.usage_counters().minutes_unheld, 2);

    let run = queued(&h, &lease, 1000).await;
    beat(&h, &me, &[], &[]).await;
    let mut running = me.beat();
    running["held"] = json!([held(&run, "running")]);
    running["load"] = load(1, 1_000_000, &[0]);
    let sent = post(&h, "/v1/agent/heartbeat", Some(JOIN), &running).await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    let exited = [held(&run, "exited_unacked")];
    let reply = beat(&h, &me, &exited, &[result(&run, &lease)]).await;
    assert_eq!(verdicts(&reply), [format!("{run} ACCEPTED")]);
    beat(&h, &me, &[], &[]).await;
    beat(&h, &me, &[], &[]).await;
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    let own = RunMinute {
        minute: 0,
        cores_milli: 1500,
        ws_bytes: 4096,
    };
    assert_eq!(ended[0].series.minutes, vec![own]);
    assert_eq!(h.running.usage_counters().minutes_unheld, 2);
}

/// Asserts that a sink that panics on a reading costs the beat nothing: the beat that
/// carries both the load and a result is answered 200 with the result accepted, and the
/// panic is counted as a fault. It catches a panic that fails the reply after the registry
/// accepted the beat, which makes the agent send the result again to be told it is a
/// duplicate.
#[tokio::test]
async fn a_sink_that_panics_on_a_reading_costs_the_beat_nothing() {
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_panic", &fake, &[]).await;
    let me = join(&h, "burst-1").await;
    beat(&h, &me, &[], &[]).await;
    let lease = lease(&h).await;
    let run = queued(&h, &lease, 1000).await;
    beat(&h, &me, &[], &[]).await;
    fake.panic_on_readings();
    let mut body = me.beat();
    body["held"] = json!([held(&run, "exited_unacked")]);
    body["results"] = json!([result(&run, &lease)]);
    body["load"] = load(1, 1000, &[0]);
    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(verdicts(&reply.json()), [format!("{run} ACCEPTED")]);
    assert_eq!(h.running.usage_counters().faults, 1);
}

/// Asserts that the server as started by its flags records into a file and offers admission
/// the curves: the file `usage.db` appears beside the lease file, `usage_curves` is there,
/// and a sink supplied by a test that learns none offers none. It catches a server that
/// starts with the sink that records nothing, and curves that admission cannot reach.
#[tokio::test]
async fn the_started_server_keeps_a_usage_file_and_offers_curves() {
    let scratch = Scratch::new("usage_file");
    let db = scratch.path("usage.db");
    let args = agent_args(&scratch, false);
    let h = Harness::over(scratch, config(args).unwrap()).await;
    assert!(db.exists());
    assert!(h.running.usage_curves().is_some());
    let fake = Arc::new(Fake::default());
    let other = start_usage("usage_no_curves", &fake, &[]).await;
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
    let args = agent_args(&scratch, false);
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

/// A key source that gives every run the profile it was built with, as a caller does that
/// supplies one name for all the runs of a kind of work.
struct Named(ProfileKey);

impl KeySource for Named {
    fn key_of(&self, _: &str) -> Option<ProfileKey> {
        Some(self.0.clone())
    }
}

/// A load of the wire that carries minute 0 of `lease`, the `seq`th the host sent.
fn load_of(lease: &str, seq: u64) -> Value {
    json!({
        "seq": seq,
        "mono_usec": seq * 5_000_000,
        "cpu_usage_usec": seq * 1_000_000,
        "working_set_bytes": 1_000_000,
        "covered": [lease],
        "minutes": [{
            "lease_id": lease, "minute": 0, "cpu_mean_millis": 1500,
            "cpu_peak_window_millis": 2500, "ws_max_bytes": 4096,
        }],
    })
}

/// Drives one run on a fresh lease of the host of `me` to a clean end through the real
/// routes, as `finish_run` does, and releases the lease. `seq` counts the loads the host
/// has sent; it is advanced.
async fn finish_run_on_a_fresh_lease(h: &Harness, me: &Session, seq: &mut u64) {
    beat(h, me, &[], &[]).await;
    let lease = lease(h).await;
    let run = queued(h, &lease, 1000).await;
    beat(h, me, &[], &[]).await;
    *seq += 1;
    let mut running = me.beat();
    running["held"] = json!([held(&run, "running")]);
    running["load"] = load_of(&lease, *seq);
    let sent = post(h, "/v1/agent/heartbeat", Some(JOIN), &running).await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    let exited = [held(&run, "exited_unacked")];
    let reply = beat(h, me, &exited, &[result(&run, &lease)]).await;
    assert_eq!(verdicts(&reply), [format!("{run} ACCEPTED")]);
    beat(h, me, &[], &[]).await;
    beat(h, me, &[], &[]).await;
    let released = h.call("DELETE", &format!("/v1/leases/{lease}"), None).await;
    assert_eq!(released.status, 200, "{}", released.body);
    beat(h, me, &[], &[]).await;
}

/// Asserts that the server keeps a run's profile under the name the caller supplied and
/// that admission reads it through the names the caller gave (`with_curve_names`): after
/// two clean runs named `est.row-1` there is no curve, after the third both work keys the
/// caller maps to that name have one (so two different work keys share one profile), and a
/// work key the caller maps to no name has none. It catches a server that drops the
/// caller's names, keeps the profile under a name of its own, or keys it by the work key.
#[tokio::test]
async fn a_profile_is_kept_under_the_supplied_name_and_read_through_the_callers_names() {
    let scratch = Scratch::new("usage_names");
    let args = agent_args(&scratch, false);
    let name = ProfileKey::parse("est.row-1").unwrap();
    let config = config(args)
        .unwrap()
        .with_key_source(Arc::new(Named(name.clone())))
        .with_curve_names(move |key| (key.class == "Build/Pool-1").then(|| name.clone()));
    let h = Harness::over(scratch, config).await;
    let me = join(&h, "burst-1").await;
    let curves = h.running.usage_curves().unwrap();
    let first = WorkKey::new("Build/Pool-1", "aaaa/1");
    let second = WorkKey::new("Build/Pool-1", "bbbb/2");
    let mut seq = 0;
    for _ in 0..2 {
        finish_run_on_a_fresh_lease(&h, &me, &mut seq).await;
    }
    assert!(curves.curve(&first).is_none());
    finish_run_on_a_fresh_lease(&h, &me, &mut seq).await;
    assert!(curves.curve(&first).is_some());
    assert!(curves.curve(&second).is_some());
    assert!(curves.curve(&WorkKey::new("Other", "aaaa/1")).is_none());
    assert_eq!(h.running.usage_counters().runs_learned, 3);
}
