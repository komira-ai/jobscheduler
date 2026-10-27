//! A restart over the same lease file.

use js_pool_static::EXAMPLE_CONFIG;
use serde_json::json;

use crate::support::{
    Harness, LogCapture, Scratch, SharedPool, args_with, base_args, ids, lease_body, lease_file,
    replace,
};

/// Asserts that, over a provider that outlives both servers (a service of its own), a
/// server started again with `--boot resume` over the lease file of one
/// that stopped keeps the leases (the provider, which outlives both, still has them
/// booked), never reissues an id of the first run, and places a new lease on the host
/// left free.
#[tokio::test]
async fn resume_keeps_leases_and_never_reuses_ids() {
    let pool = SharedPool::new();
    let scratch = Scratch::new("resume");
    let args = base_args(&scratch);
    let first = Harness::with(scratch, args, Some(Box::new(pool.clone()))).await;
    let granted = first.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["id"], "1-1");
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();

    let args = replace(base_args(&scratch), "--boot", "resume");
    let second = Harness::with(scratch, args, Some(Box::new(pool))).await;
    let listed = second.call("GET", "/v1/leases", None).await;
    assert_eq!(ids(&listed), vec!["1-1"]);
    assert_eq!(listed.json()["leases"][0]["state"], "held");
    assert_eq!(listed.json()["leases"][0]["host"], "gpu-01");

    let next = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.status, 201, "{}", next.body);
    assert_eq!(next.json()["id"], "2-2");
    assert_eq!(next.json()["host"], "gpu-02");

    let renewed = second.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(renewed.status, 200);
    assert_eq!(renewed.json().as_object().unwrap().len(), 1);
    assert_eq!(
        second.call("DELETE", "/v1/leases/1-1", None).await.json(),
        json!({ "state": "freed" })
    );
}

/// Asserts that a first boot over an existing lease file loads it like a resume.
#[tokio::test]
async fn initialise_over_an_existing_file_loads_it() {
    let pool = SharedPool::new();
    let scratch = Scratch::new("initialise_existing");
    let args = args_with(&scratch, &[]);
    let first = Harness::with(scratch, args.clone(), Some(Box::new(pool.clone()))).await;
    first.call("POST", "/v1/leases", Some(&lease_body())).await;
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();

    let second = Harness::with(scratch, args, Some(Box::new(pool))).await;
    assert_eq!(
        ids(&second.call("GET", "/v1/leases", None).await),
        vec!["1-1"]
    );
}

/// Asserts that a server whose lease file has used its last sequence answers an acquire
/// 503 `ids-exhausted`, books nothing and leaves the file as it was: the table can issue
/// no lease until the operator restores the state file.
#[tokio::test]
async fn an_acquire_after_the_last_lease_id_is_503() {
    let scratch = Scratch::new("ids_exhausted");
    let head = format!(
        "version = 1\nterm = 1\nnext_sequence = {}\nlease_count = 0\n",
        i64::MAX
    );
    std::fs::write(scratch.state(), &head).unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    let refused = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "ids-exhausted" }));
    assert_eq!(
        ids(&h.call("GET", "/v1/leases", None).await),
        Vec::<String>::new()
    );
}

/// Stops `first`, a server on the base flags, and starts another over its scratch
/// directory with `--boot resume` and the pool file in its place, exactly as the program
/// does: the second server builds a fresh pool from the file, and no provider outlives
/// the first.
async fn restarted(first: Harness) -> Harness {
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    Harness::with(scratch, args, None).await
}

/// Asserts that a restart with the shipped provider, a pool built again from the pool
/// file, keeps a held lease held on the same host (the lease file is the record of the
/// pool's bookings), that the host is not leased to anyone else (the second host is
/// taken by the next acquire and the third acquire has no capacity), that the lease can
/// be renewed and released, and that its host is free after the release.
#[tokio::test]
async fn a_restart_keeps_a_held_lease_on_the_shipped_pool() {
    let first = Harness::new("resume_shipped").await;
    let granted = first.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["host"], "gpu-01");
    let second = restarted(first).await;

    let listed = second.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["id"], "1-1");
    assert_eq!(listed["leases"][0]["state"], "held");
    assert_eq!(listed["leases"][0]["host"], "gpu-01");

    let next = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-02");
    let full = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.status, 409, "{}", full.body);
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));

    let renewed = second.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    let released = second.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
    let again = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(again.json()["host"], "gpu-01");
}

/// Asserts that a lease file with a held, a releasing and a pending lease without a
/// host restores both hosts as booked (an acquire has no capacity), and that a tick then
/// settles the other two as the table always does: the releasing lease is stopped and
/// frees its host, the pending lease is not known to the pool and is dropped.
#[tokio::test]
async fn every_booking_of_the_file_is_seated_before_the_table_looks() {
    let scratch = Scratch::new("resume_states");
    let leases = [
        (1, "held", Some("gpu-01")),
        (2, "releasing", Some("gpu-02")),
        (3, "pending", None),
    ];
    std::fs::write(scratch.state(), lease_file(&leases)).unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await).len(), 3);
    let full = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));

    h.ticks.step().await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), vec!["1-1"]);
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-02");
}

/// Asserts that a lease whose host the pool file no longer lists is dropped at the
/// restart (the table finds the pool ignorant of it), that the drop is logged with the
/// lease, the host and the reason, and that the hosts left are leasable.
#[tokio::test]
async fn a_host_the_pool_file_dropped_loses_its_lease_and_says_so() {
    let first = Harness::new("resume_unlisted").await;
    first.call("POST", "/v1/leases", Some(&lease_body())).await;
    let only_gpu_02 = format!(
        "[[host]]{}",
        EXAMPLE_CONFIG.rsplit("[[host]]").next().unwrap()
    );
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();

    let logs = LogCapture::start();
    let args = replace(base_args(&scratch), "--boot", "resume");
    std::fs::write(scratch.path("pool.toml"), &only_gpu_02).unwrap();
    let h = Harness::with(scratch, args, None).await;
    let text = logs.text();
    assert!(text.contains("1-1"), "{text}");
    assert!(text.contains("gpu-01"), "{text}");
    assert!(text.contains("not in the pool"), "{text}");

    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-02");
}

/// Asserts that when a lease file names one host for two leases the lower id keeps it,
/// the other is dropped, and the drop is logged with the reason.
#[tokio::test]
async fn two_leases_on_one_host_leave_the_lower_id_seated() {
    let scratch = Scratch::new("resume_double");
    let leases = [(1, "held", Some("gpu-01")), (2, "held", Some("gpu-01"))];
    std::fs::write(scratch.state(), lease_file(&leases)).unwrap();
    let logs = LogCapture::start();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    let text = logs.text();
    assert!(text.contains("1-2"), "{text}");
    assert!(text.contains("held by lease 1-1"), "{text}");
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), vec!["1-1"]);
}
