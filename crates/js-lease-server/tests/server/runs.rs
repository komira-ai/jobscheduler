//! Container runs under a lease on a registered host, through the lease routes and a
//! scripted agent on the agent routes: how a run reaches the host, what the host's
//! answers do, and what a release, an expiry or a restart does to the runs.

use std::time::Duration;

use serde_json::json;

use crate::agents_support::{
    JOIN, entry, grant, grant_with, hello_body, hosts, join, post, report_ready, restarted,
    start_agents,
};
use crate::runs_support::{
    beat, held, idle, lease, queue, queued, result, run_body, runs, state, stop, verdicts, words,
};
use crate::support::{ids, lease_body};

/// Asserts the path of a run end to end: it is queued (202), listed as queued, carried on
/// the host's next heartbeat reply with the lease it runs under, the heartbeat it was
/// queued at and the time it may still start for, listed as running when the host lists it,
/// and when the host reports its result the result is accepted, shown in the listing, and
/// the run is ended once the host stops listing it. It catches a run that is queued but
/// never reaches the host, and a result that is accepted and then lost to the consumer.
#[tokio::test]
async fn run_started_on_next_heartbeat_and_result_returned() {
    let h = start_agents("runs_flow", false).await;
    let me = join(&h, "burst-1").await;
    beat(&h, &me, &[], &[]).await;
    let lease = lease(&h, &[&me]).await;

    let queued = queue(&h, &lease, &run_body(2000)).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    assert_eq!(
        queued.json(),
        json!({ "run_id": "run-1-1", "state": "queued" })
    );
    assert_eq!(state(&h, &lease, "run-1-1").await, "queued");

    let reply = beat(&h, &me, &[], &[]).await;
    assert_eq!(words(&reply), ["start run-1-1"]);
    let start = &reply["commands"][0];
    assert_eq!(start["lease_id"], lease.as_str());
    assert_eq!(start["image"], "busybox:1");
    assert_eq!(start["limits"]["cpu_millis"], 2000);
    assert_eq!(start["limits"]["wall_ms"], 600_000);
    assert_eq!(start["valid_for_ms"], 600_000);
    // The beat before the lease, the one that reported the host ready, and this one.
    assert_eq!(start["heartbeat_seq"], 2);

    let running = [held("run-1-1", "running")];
    let reply = beat(&h, &me, &running, &[]).await;
    assert_eq!(words(&reply), Vec::<String>::new());
    assert_eq!(state(&h, &lease, "run-1-1").await, "running");

    let exited = [held("run-1-1", "exited_unacked")];
    let done = [result("run-1-1", &lease)];
    let reply = beat(&h, &me, &exited, &done).await;
    assert_eq!(verdicts(&reply), ["run-1-1 ACCEPTED"]);
    let listing = runs(&h, &lease).await;
    assert_eq!(listing[0]["state"], "exited");
    assert_eq!(listing[0]["outcome"], "succeeded");
    assert_eq!(listing[0]["exit_code"], 0);

    let reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
    let listing = runs(&h, &lease).await;
    assert_eq!(listing[0]["state"], "ended");
    assert_eq!(listing[0]["outcome"], "succeeded");
}

/// Asserts that a stop is resent: the run is asked to stop (202, stopping), the first reply
/// carrying the stop never reaches the agent (the test drops it), and the next reply
/// carries it again because the host still lists the run; once the host stops listing the
/// run no stop is sent and the run is ended. It catches a stop sent once, which a lost
/// reply would turn into a job that cannot be stopped.
#[tokio::test]
async fn stop_resent_until_run_leaves_held() {
    let h = start_agents("runs_stop", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    let run = queued(&h, &lease, 1000).await;
    let running = [held(&run, "running")];
    beat(&h, &me, &running, &[]).await;

    let asked = stop(&h, &lease, &run).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    assert_eq!(asked.json(), json!({ "run_id": run, "state": "stopping" }));
    let dropped = beat(&h, &me, &running, &[]).await;
    assert_eq!(words(&dropped), [format!("stop {run}")]);
    let resent = beat(&h, &me, &running, &[]).await;
    assert_eq!(words(&resent), [format!("stop {run}")]);
    assert_eq!(state(&h, &lease, &run).await, "stopping");

    let gone = beat(&h, &me, &[], &[]).await;
    assert!(words(&gone).is_empty());
    assert_eq!(state(&h, &lease, &run).await, "ended");
}

/// Asserts that a run queued for a host that then goes silent is never started on it: a new
/// run for the lost host is refused 409 `host-lost`, and when the host is heard again the
/// reply carries no start for the queued run, which is listed as never started and has
/// given its limits back. It catches a start delivered to a host long after the lease
/// holder stopped waiting for it.
#[tokio::test]
async fn start_to_lost_host_never_started() {
    let h = start_agents("runs_lost", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    let run = queued(&h, &lease, 8000).await;

    h.clock.advance(Duration::from_secs(20));
    let refused = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "host-lost" }));

    let reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
    assert_eq!(state(&h, &lease, &run).await, "never-started");
    queued(&h, &lease, 8000).await;
}

/// Asserts the refusals of a job that cannot run on the lease, each 422 with its code and
/// each leaving nothing queued: limits over the booked shape, a GPU index the shape does
/// not have, an image with a transport prefix, a mount that climbs, and a job that asks
/// for host isolation. Unknown fields and a missing limit are a 400. It catches a job that
/// reaches the agent only to be refused there, and a refusal with the wrong status.
#[tokio::test]
async fn limits_over_shape_422() {
    let h = start_agents("runs_422", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    let refuse = |body| {
        let (h, lease) = (&h, &lease);
        async move {
            let reply = queue(h, lease, &body).await;
            (
                reply.status,
                reply.json()["error"].as_str().unwrap().to_string(),
            )
        }
    };
    let mut cpu = run_body(8001);
    assert_eq!(
        refuse(cpu.clone()).await,
        (422, "limits-exceed-shape".into())
    );
    cpu["limits"]["cpu_millis"] = json!(1000);
    cpu["limits"]["memory_bytes"] = json!(17179869185u64);
    assert_eq!(refuse(cpu).await, (422, "limits-exceed-shape".into()));
    let mut gpu = run_body(1000);
    gpu["gpu_indices"] = json!([1]);
    assert_eq!(refuse(gpu).await, (422, "limits-exceed-shape".into()));
    let mut image = run_body(1000);
    image["image"] = json!("docker://busybox");
    assert_eq!(refuse(image).await, (422, "invalid-image".into()));
    let mut mount = run_body(1000);
    mount["mounts"] = json!([{ "root_name": "cache", "rel": "../x", "ro": true }]);
    assert_eq!(refuse(mount).await, (422, "invalid-mount".into()));
    let mut host = run_body(1000);
    host["isolation"] = json!("host");
    assert_eq!(refuse(host).await, (422, "host-jobs-not-allowed".into()));
    let mut wall = run_body(1000);
    wall["limits"]["wall_ms"] = json!(0);
    assert_eq!(refuse(wall).await, (422, "wall-time-not-positive".into()));
    assert!(runs(&h, &lease).await.is_empty());

    let mut extra = run_body(1000);
    extra["priority"] = json!(1);
    assert_eq!(queue(&h, &lease, &extra).await.status, 400);
    let nothing = json!({ "image": "busybox:1" });
    assert_eq!(queue(&h, &lease, &nothing).await.status, 400);
    let mut unknown = run_body(1000);
    unknown["isolation"] = json!("quantum");
    assert_eq!(queue(&h, &lease, &unknown).await.status, 400);
}

/// Asserts the budget of a lease: with a booked shape of 8000, two runs of 4000 fit, a
/// third is refused 409 `no-capacity`, and the refused run is not queued. It catches a
/// budget that is not summed over the runs of a lease.
#[tokio::test]
async fn second_run_fits_third_409() {
    let h = start_agents("runs_budget", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    queued(&h, &lease, 4000).await;
    queued(&h, &lease, 4000).await;
    let third = queue(&h, &lease, &run_body(4000)).await;
    assert_eq!(third.status, 409, "{}", third.body);
    assert_eq!(third.json(), json!({ "error": "no-capacity" }));
    assert_eq!(runs(&h, &lease).await.len(), 2);
}

/// Asserts when a run's limits come back: not when it is asked to stop, not when its result
/// is accepted, only when the host stops listing it. A run that would fit once the first is
/// gone is refused while the host still lists the first, and accepted in the very next
/// request after the list drops it. It catches a budget returned too early, which would let
/// a second job start while the first still holds the machine's cores and scratch.
#[tokio::test]
async fn slot_returns_only_after_run_leaves_held() {
    let h = start_agents("runs_slot", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    let first = queued(&h, &lease, 8000).await;
    let running = [held(&first, "running")];
    beat(&h, &me, &running, &[]).await;
    stop(&h, &lease, &first).await;

    let exited = [held(&first, "exited_unacked")];
    let done = [result(&first, &lease)];
    let reply = beat(&h, &me, &exited, &done).await;
    assert_eq!(verdicts(&reply), [format!("{first} ACCEPTED")]);
    let refused = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(refused.json(), json!({ "error": "no-capacity" }));

    beat(&h, &me, &[], &[]).await;
    queued(&h, &lease, 8000).await;
}

/// Asserts the release of a lease with a live run: the release is answered (the booking is
/// free) but the host takes no new lease and reads as releasing; the stop for the run goes
/// out on every reply while the host lists it; and the host is bookable again only after
/// the first heartbeat that lists nothing. The runs of the released lease cannot be read.
/// It catches a host handed to the next client while the last client's job still runs.
#[tokio::test]
async fn release_with_live_runs_host_rebookable_only_after_empty_heartbeat() {
    let h = start_agents("runs_release", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    let run = queued(&h, &lease, 1000).await;
    let running = [held(&run, "running")];
    beat(&h, &me, &running, &[]).await;

    let released = h.call("DELETE", &format!("/v1/leases/{lease}"), None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
    let listing = entry(&hosts(&h).await, "burst-1");
    assert_eq!(listing["releasing"], true);
    assert!(listing.get("lease").is_none());
    let taken = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(taken.json(), json!({ "error": "no-capacity" }));
    let gone = h
        .call("GET", &format!("/v1/leases/{lease}/runs"), None)
        .await;
    assert_eq!(gone.status, 404, "{}", gone.body);

    for _ in 0..2 {
        let reply = beat(&h, &me, &running, &[]).await;
        assert_eq!(words(&reply), [format!("stop {run}")]);
        let taken = h.call("POST", "/v1/leases", Some(&lease_body())).await;
        assert_eq!(taken.status, 409, "{}", taken.body);
    }
    let reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
    assert_eq!(entry(&hosts(&h).await, "burst-1")["releasing"], false);
    let again = grant(&h, &[&me]).await;
    assert_eq!(again["host"], "burst-1");
}

/// Asserts that a run the server does not know is cancelled and never adopted: a hello that
/// lists one is admitted, the host takes no lease and reads as releasing, the next
/// heartbeat reply cancels the run for as long as it is listed, and when it is gone the
/// host takes a lease whose run list is empty. It catches an agent that restarted with
/// work the server forgot being left to run on a machine leased to someone else.
#[tokio::test]
async fn unknown_run_in_hello_cancelled_not_adopted() {
    let h = start_agents("runs_unknown_hello", false).await;
    let mut body = hello_body("burst-1");
    body["held"] = json!([held("ghost", "running")]);
    let welcome = post(&h, "/v1/agent/hello", Some(JOIN), &body).await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let me = crate::agents_support::session(&welcome, "burst-1");
    assert_eq!(entry(&hosts(&h).await, "burst-1")["releasing"], true);
    let none = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(none.json(), json!({ "error": "no-capacity" }));

    let ghost = [held("ghost", "running")];
    let reply = beat(&h, &me, &ghost, &[]).await;
    assert_eq!(words(&reply), ["cancel ghost"]);
    let reply = beat(&h, &me, &ghost, &[]).await;
    assert_eq!(words(&reply), ["cancel ghost"]);
    let reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
    let lease = lease(&h, &[&me]).await;
    assert!(runs(&h, &lease).await.is_empty());
}

/// Asserts what a restart does to runs: the lease survives and the run records do not. The
/// agent's old session is refused, its hello that lists the run is admitted, the lease
/// shows no runs, the next reply cancels the run, and the host is quiet after it is gone.
/// It catches a server that adopts work it has no record of, and one that keeps records a
/// restart should have dropped.
#[tokio::test]
async fn server_restart_then_runs_cancelled() {
    let first = start_agents("runs_restart", false).await;
    let old = join(&first, "burst-1").await;
    let lease = lease(&first, &[&old]).await;
    let run = queued(&first, &lease, 1000).await;
    beat(&first, &old, &[held(&run, "running")], &[]).await;

    let h = restarted(first, false).await;
    let mut body = hello_body("burst-1");
    body["held"] = json!([held(&run, "running")]);
    let welcome = post(&h, "/v1/agent/hello", Some(JOIN), &body).await;
    let me = crate::agents_support::session(&welcome, "burst-1");
    assert!(runs(&h, &lease).await.is_empty());

    let reply = beat(&h, &me, &[held(&run, "running")], &[]).await;
    assert_eq!(words(&reply), [format!("cancel {run}")]);
    let late = [result(&run, &lease)];
    let reply = beat(&h, &me, &[held(&run, "exited_unacked")], &late).await;
    assert_eq!(verdicts(&reply), [format!("{run} STALE")]);
    beat(&h, &me, &[], &[]).await;
    assert_eq!(entry(&hosts(&h).await, "burst-1")["releasing"], false);
    // The restored lease is committed again: the agent must report it ready first.
    let refused = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(refused.json(), json!({ "error": "not-ready" }));
    report_ready(&h, &me, &lease).await;
    let next = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(next.status, 202, "{}", next.body);
}

/// Asserts that run ids are never reused across a restart: the first boot mints `run-1-1`,
/// and the same lease queues its next run after a restart as `run-2-1`, not `run-1-1`
/// again, although the server remembers nothing of the first. It catches an id counter that
/// starts over, which would let a result of the old run be taken for the new one.
#[tokio::test]
async fn run_ids_are_not_reused_across_a_restart() {
    let first = start_agents("runs_ids_restart", false).await;
    let me = join(&first, "burst-1").await;
    let lease = lease(&first, &[&me]).await;
    assert_eq!(queued(&first, &lease, 1000).await, "run-1-1");
    assert_eq!(queued(&first, &lease, 1000).await, "run-1-2");

    let h = restarted(first, false).await;
    let me = join(&h, "burst-1").await;
    report_ready(&h, &me, &lease).await;
    assert_eq!(queued(&h, &lease, 1000).await, "run-2-1");
    assert_eq!(queued(&h, &lease, 1000).await, "run-2-2");
}

/// Asserts the verdict for each kind of result: accepted the first time, duplicate when it
/// is resent, stale once the lease has moved on, and unknown lease for a lease the server
/// holds nowhere; each result of a heartbeat gets exactly one acknowledgement. It catches a
/// verdict that is missing or mapped to the wrong word, which is what tells an agent to
/// keep or drop a result.
#[tokio::test]
async fn result_ack_each_value() {
    let h = start_agents("runs_acks", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h, &[&me]).await;
    let first = queued(&h, &lease, 1000).await;
    let second = queued(&h, &lease, 1000).await;
    let both = [held(&first, "running"), held(&second, "running")];

    let reply = beat(&h, &me, &both, &[result(&first, &lease)]).await;
    assert_eq!(verdicts(&reply), [format!("{first} ACCEPTED")]);
    let reply = beat(&h, &me, &both, &[result(&first, &lease)]).await;
    assert_eq!(verdicts(&reply), [format!("{first} DUPLICATE")]);
    let ghost = result("ghost", "9-9");
    let reply = beat(&h, &me, &both, &[ghost]).await;
    assert_eq!(verdicts(&reply), ["ghost UNKNOWN_LEASE"]);

    h.call("DELETE", &format!("/v1/leases/{lease}"), None).await;
    let mixed = [result(&second, &lease), result("late", &lease)];
    let reply = beat(&h, &me, &both, &mixed).await;
    assert_eq!(
        verdicts(&reply),
        [format!("{second} STALE"), "late UNKNOWN_LEASE".to_string()]
    );
}

/// Asserts that a result for a run of another host is a MISMATCH and changes nothing: the
/// second host reports the first host's run under the right lease, is told MISMATCH, and
/// the run still has no result for the lease holder. It catches one host being able to
/// write another host's outcome.
#[tokio::test]
async fn result_for_other_host_is_mismatch() {
    let h = start_agents("runs_mismatch", false).await;
    let a = join(&h, "burst-1").await;
    let b = join(&h, "burst-2").await;
    let lease = lease(&h, &[&a, &b]).await;
    let run = queued(&h, &lease, 1000).await;
    beat(&h, &a, &[held(&run, "running")], &[]).await;

    let forged = [result(&run, &lease)];
    let reply = beat(&h, &b, &[], &forged).await;
    assert_eq!(verdicts(&reply), [format!("{run} MISMATCH")]);
    let listing = runs(&h, &lease).await;
    assert!(listing[0].get("outcome").is_none());
    assert_eq!(listing[0]["state"], "running");
    let own = beat(&h, &a, &[held(&run, "running")], &forged).await;
    assert_eq!(verdicts(&own), [format!("{run} ACCEPTED")]);
}

/// Asserts that a run never outlives its lease. The wall limit is the one asked for, cut
/// to the time the lease has left, and the time the agent may still start it for is that
/// too; a run asked for after the lease has lapsed (before a tick expires it) is refused
/// 409 `not-held`; and when the tick expires the lease the host is told to stop both runs
/// and is not bookable until it lists none. It catches a run with a bound longer than the
/// lease and a lease that ends with its work left running.
#[tokio::test]
async fn a_run_never_outlives_its_lease() {
    let h = start_agents("runs_lease_bound", false).await;
    let me = join(&h, "burst-1").await;
    let mut ask = lease_body();
    ask["ttl_seconds"] = json!(100);
    let granted = grant_with(&h, &[&me], &ask).await;
    let lease = granted["id"].as_str().unwrap().to_string();

    let mut long = run_body(1000);
    long["limits"]["wall_ms"] = json!(10_000_000);
    assert_eq!(queue(&h, &lease, &long).await.status, 202);
    let reply = beat(&h, &me, &[], &[]).await;
    let sent = &reply["commands"][0];
    assert_eq!(sent["limits"]["wall_ms"], 100_000);
    assert_eq!(sent["valid_for_ms"], 100_000);

    let first = [held("run-1-1", "running")];
    idle(&h, &me, 50, &first).await;
    let mut short = run_body(1000);
    short["limits"]["wall_ms"] = json!(5_000);
    assert_eq!(queue(&h, &lease, &short).await.status, 202);
    assert_eq!(queue(&h, &lease, &run_body(1000)).await.status, 202);
    let reply = beat(&h, &me, &first, &[]).await;
    assert_eq!(words(&reply), ["start run-1-2", "start run-1-3"]);
    assert_eq!(reply["commands"][0]["limits"]["wall_ms"], 5_000);
    assert_eq!(reply["commands"][1]["limits"]["wall_ms"], 50_000);
    assert_eq!(reply["commands"][1]["valid_for_ms"], 50_000);

    let all = [
        held("run-1-1", "running"),
        held("run-1-2", "running"),
        held("run-1-3", "running"),
    ];
    idle(&h, &me, 50, &all).await;
    h.clock.advance(Duration::from_secs(1));
    let lapsed = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(lapsed.json(), json!({ "error": "not-held" }));

    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    let reply = beat(&h, &me, &all, &[]).await;
    assert_eq!(
        words(&reply),
        ["stop run-1-1", "stop run-1-2", "stop run-1-3"]
    );
    let none = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(none.json(), json!({ "error": "no-capacity" }));
    beat(&h, &me, &[], &[]).await;
    let again = grant(&h, &[&me]).await;
    assert_eq!(again["host"], "burst-1");
}
