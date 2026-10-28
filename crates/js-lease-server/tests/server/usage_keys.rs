//! What a run's work key does end to end: it is named when the run is queued, kept with
//! the run, and is what a clean run is learned under. The sink is the in-memory one of
//! `usage_support` where the test is about what the server hands it, and the server's own
//! store where the test is about the curve that results.

use std::sync::Arc;

use js_lease_server::UsageCounters;
use js_types::profile_key::profile_key;
use js_types::sched::WorkKey;
use js_types::sched::usage::CurveSource;
use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, agent_args, join, post};
use crate::runs_support::{beat, held, lease, queue, run_body, runs};
use crate::support::{Harness, Scratch, config};
use crate::usage_support::Fake;

/// The server with agents on and the key source it starts with, over `fake` when given
/// and over the usage file it opens when not.
async fn start(name: &str, fake: Option<&Arc<Fake>>) -> Harness {
    let scratch = Scratch::new(name);
    let args = agent_args(&scratch, false);
    let config = config(args).unwrap();
    let config = match fake {
        Some(fake) => config.with_usage_sink(fake.clone()),
        None => config,
    };
    Harness::over(scratch, config).await
}

/// A run body that names the work `class` and `dedup`.
fn keyed(class: &str, dedup: &str) -> Value {
    let mut body = run_body(1000);
    body["work_key"] = json!({ "class": class, "dedup": dedup });
    body
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

/// The result of `run` that ended as `outcome`, with the exit status that outcome needs.
fn ended_as(run: &str, lease: &str, outcome: &str) -> Value {
    let code = match outcome {
        "succeeded" => json!(0),
        "failed" => json!(1),
        _ => Value::Null,
    };
    let mut result = json!({ "run_id": run, "lease_id": lease, "outcome": outcome });
    if !code.is_null() {
        result["exit_code"] = code;
    }
    result
}

/// Drives one run of `body` on a fresh lease of the host of `me` to the end `outcome`
/// names, through the real routes: queued, started, running with a load, exited with an
/// accepted result, the two beats of grace, and the lease released. `seq` is the count of
/// loads the host has sent; it is advanced.
async fn run_to_end(h: &Harness, me: &Session, seq: &mut u64, body: &Value, outcome: &str) {
    beat(h, me, &[], &[]).await;
    let lease = lease(h).await;
    let queued = queue(h, &lease, body).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    let run = queued.json()["run_id"].as_str().unwrap().to_string();
    beat(h, me, &[], &[]).await;
    *seq += 1;
    let mut running = me.beat();
    running["held"] = json!([held(&run, "running")]);
    running["load"] = load_of(&lease, *seq);
    let sent = post(h, "/v1/agent/heartbeat", Some(JOIN), &running).await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    let exited = [held(&run, "exited_unacked")];
    let reply = beat(h, me, &exited, &[ended_as(&run, &lease, outcome)]).await;
    assert_eq!(reply["result_acks"][0]["ack"], "ACCEPTED");
    beat(h, me, &[], &[]).await;
    beat(h, me, &[], &[]).await;
    let released = h.call("DELETE", &format!("/v1/leases/{lease}"), None).await;
    assert_eq!(released.status, 200, "{}", released.body);
    beat(h, me, &[], &[]).await;
}

/// Asserts that a run queued with a work key, once it ends cleanly, is given to the sink
/// under the profile name of that key, whether it succeeded or failed: two runs, one of
/// each, are two runs of one profile name. It catches a server that takes the key from
/// anywhere but the run's own field, and a profile name other than the one admission
/// reads curves by.
#[tokio::test]
async fn a_clean_run_with_a_work_key_reaches_the_sink_under_its_profile_name() {
    let fake = Arc::new(Fake::default());
    let h = start("usage_keys_clean", Some(&fake)).await;
    let me = join(&h, "burst-1").await;
    let body = keyed("builds", "ab12/34");
    let mut seq = 0;
    run_to_end(&h, &me, &mut seq, &body, "succeeded").await;
    run_to_end(&h, &me, &mut seq, &body, "failed").await;
    let name = profile_key(&WorkKey::new("builds", "ab12/34"));
    let ended = fake.ended();
    let names = ended.iter().map(|end| end.key.as_str()).collect::<Vec<_>>();
    assert_eq!(names, [name.as_str(), name.as_str()]);
    assert_eq!(h.running.usage_counters().runs_learned, 2);
    assert_eq!(h.running.usage_counters().runs_without_key, 0);
}

/// Asserts that a run queued without a work key is counted and not stored, for a body that
/// leaves the field out (what every client before the field sends) and for one that sends
/// it as null. It catches a server that learns under a made-up key, and one that refuses
/// the old body.
#[tokio::test]
async fn a_run_without_a_work_key_is_counted_and_not_stored() {
    let fake = Arc::new(Fake::default());
    let h = start("usage_keys_none", Some(&fake)).await;
    let me = join(&h, "burst-1").await;
    let mut null = run_body(1000);
    null["work_key"] = Value::Null;
    let mut seq = 0;
    run_to_end(&h, &me, &mut seq, &run_body(1000), "succeeded").await;
    run_to_end(&h, &me, &mut seq, &null, "succeeded").await;
    assert!(fake.ended().is_empty());
    assert_eq!(h.running.usage_counters().runs_without_key, 2);
}

/// Asserts that a run with a work key that did not run to a clean end is not stored and not
/// counted as keyless: a run that failed to start, one that was killed, one that timed out
/// and one that ran out of memory. It catches a run learned from a measurement of work that
/// did not finish.
#[tokio::test]
async fn a_run_that_did_not_end_cleanly_is_not_stored_whatever_its_key() {
    let fake = Arc::new(Fake::default());
    let h = start("usage_keys_unclean", Some(&fake)).await;
    let me = join(&h, "burst-1").await;
    let body = keyed("builds", "ab12/34");
    let mut seq = 0;
    for outcome in ["failed_to_start", "killed", "timed_out", "out_of_memory"] {
        run_to_end(&h, &me, &mut seq, &body, outcome).await;
    }
    assert!(fake.ended().is_empty());
    assert_eq!(h.running.usage_counters(), UsageCounters::default());
}

/// Asserts the curve the server learns on its own store: after two clean runs of one work
/// key there is none, after the third there is one for that key and for no other key.
/// It catches a store that is never given the runs, and a curve under the wrong name.
#[tokio::test]
async fn three_clean_runs_of_one_work_key_give_a_curve() {
    let h = start("usage_keys_curve", None).await;
    let me = join(&h, "burst-1").await;
    let key = WorkKey::new("builds", "ab12/34");
    let body = keyed("builds", "ab12/34");
    let curves = h.running.usage_curves().unwrap();
    let mut seq = 0;
    for _ in 0..2 {
        run_to_end(&h, &me, &mut seq, &body, "succeeded").await;
    }
    assert!(curves.curve(&key).is_none());
    run_to_end(&h, &me, &mut seq, &body, "succeeded").await;
    assert!(curves.curve(&key).is_some());
    assert!(curves.curve(&WorkKey::new("builds", "other")).is_none());
    assert_eq!(h.running.usage_counters().runs_learned, 3);
}

/// Asserts that a work key the server cannot keep is refused with 422 `invalid-work-key`
/// and queues nothing (an empty class, a class or dedup text over 256 bytes), that a key
/// of exactly 256 bytes is accepted, and that a key with a field of its own making is a
/// 400 like any body that is not the route's. It catches a key that is kept unbounded in
/// memory, and a refusal that still queues the run.
#[tokio::test]
async fn a_work_key_the_server_cannot_keep_is_refused_and_queues_nothing() {
    let h = start("usage_keys_refused", None).await;
    let me = join(&h, "burst-1").await;
    beat(&h, &me, &[], &[]).await;
    let lease = lease(&h).await;
    let long = "x".repeat(257);
    let refused = [keyed("", "d"), keyed(&long, "d"), keyed("c", &long)];
    for body in refused {
        let reply = queue(&h, &lease, &body).await;
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert_eq!(reply.json()["error"], "invalid-work-key");
    }
    let mut odd = run_body(1000);
    odd["work_key"] = json!({ "class": "c", "dedup": "d", "extra": 1 });
    assert_eq!(queue(&h, &lease, &odd).await.status, 400);
    assert!(runs(&h, &lease).await.is_empty());
    let edge = "x".repeat(256);
    let reply = queue(&h, &lease, &keyed(&edge, &edge)).await;
    assert_eq!(reply.status, 202, "{}", reply.body);
}
