//! Usage over real sockets: what a heartbeat's load does and does not do, when the sink
//! is flushed and pruned, who may read the series, and what the log says. The sink is an
//! in-memory one (see `usage_support`), so the server's part is what is tested.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use js_lease_server::{Cli, DropReason, NodeRow, RunMinute, UsageCounters};
use js_types::LeaseId;
use serde_json::json;

use crate::agents_support::{JOIN, Session, agent_args, join, post};
use crate::runs_support::{beat, held, lease, queued, result, verdicts};
use crate::support::{Harness, LogCapture, Scratch, args_with, base_args, config};
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

/// Asserts that a run queued without a work key (the server's key source by default) is not
/// learned from and says so: the run is counted as `runs_without_key`, the sink is not given
/// it, and the reads still work. It catches a server that silently learns under a made-up
/// key and one that fails the run for lacking one.
#[tokio::test]
async fn a_run_queued_without_a_work_key_is_counted_and_nothing_is_learned() {
    let fake = Arc::new(Fake::default());
    let scratch = Scratch::new("usage_nokeys");
    let args = agent_args(&scratch, false);
    let config = config(args).unwrap().with_usage_sink(fake.clone());
    let h = Harness::over(scratch, config).await;
    let me = join(&h, "burst-1").await;
    finish_run(&h, &me).await;
    assert!(fake.ended().is_empty());
    assert_eq!(h.running.usage_counters().runs_without_key, 1);
}

/// Asserts the log: one `node minute` line for each closed minute of each node (two nodes,
/// three minutes, six lines), written when the minutes are flushed and not once for each
/// of the twelve beats that made them. It catches a line per heartbeat, which at twelve
/// beats a minute a node would drown the log, and a minute logged twice.
#[tokio::test]
async fn each_closed_node_minute_is_logged_once_and_not_for_each_beat() {
    let log = LogCapture::start();
    let fake = Arc::new(Fake::default());
    let h = start_usage("usage_log", &fake, &[]).await;
    let (a, b) = (join(&h, "burst-1").await, join(&h, "burst-2").await);
    for seq in 1..=3u64 {
        for twice in [0, 10] {
            beat_load(&h, &a, load(seq + twice, seq * 1_000_000, &[])).await;
            beat_load(&h, &b, load(seq + twice, seq * 2_000_000, &[])).await;
        }
        h.clock.advance(MINUTE);
    }
    assert_eq!(log.text().matches("node minute").count(), 0);
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let text = log.text();
    assert_eq!(text.matches("node minute").count(), 6, "{text}");
    for needle in [
        "host=burst-1",
        "host=burst-2",
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

/// Asserts `--usage-off`: heartbeats with a load are served as ever and reach no sink,
/// ticks flush and prune nothing, and nothing is counted. It catches an off switch that
/// still records, and one that breaks heartbeats.
#[tokio::test]
async fn with_usage_off_nothing_is_recorded_and_beats_are_served() {
    let fake = Arc::new(Fake::default());
    let scratch = Scratch::new("usage_off");
    let mut args = agent_args(&scratch, false);
    args.push("--usage-off".into());
    let h = start_over(scratch, args, &fake, &[]).await;
    let me = join(&h, "burst-1").await;
    let reply = beat_load(&h, &me, load(1, 1000, &[0])).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
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
