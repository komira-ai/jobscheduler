//! A restart over the same lease file.

use serde_json::json;

use crate::support::{
    Harness, Scratch, SharedPool, args_with, base_args, ids, lease_body, replace,
};

/// Asserts that a server started again with `--boot resume` over the lease file of one
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
