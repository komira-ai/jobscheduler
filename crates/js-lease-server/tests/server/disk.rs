//! Scratch disk over HTTP: `resources.disk_bytes` of `POST /v1/leases` is judged against
//! the disk of a host, and the disk a host books is shown in `GET /v1/hosts`. (The class
//! these routes serve books a whole host, so a lease holds all of its disk; the sum over
//! leases is the scheduler's, tested in `js-sched`.)

use serde_json::{Value, json};

use crate::support::{Harness, Scratch, base_args, replace};

const GIB: u64 = 1 << 30;

/// The disk the one listed host has.
const DISK: u64 = 100 * GIB;

/// A hosts file of one host with 8 cores, 64 GiB, 8 GPUs and `disk` bytes of scratch disk.
fn hosts_text(disk: u64) -> String {
    format!(
        "[[host]]\nid = \"big-01\"\ngpu_class = \"nvidia-a100\"\n\n[host.resources]\n\
         cpu_millis = 8000\nmemory_bytes = {}\ngpus = 8\ndisk_bytes = {disk}\n\n\
         [host.capabilities]\nisa = \"x86-64-v3\"\n",
        64 * GIB
    )
}

/// A server over a static hosts file whose one host has `disk` bytes of scratch disk.
async fn serve(name: &str, disk: u64) -> Harness {
    let scratch = Scratch::new(name);
    let path = scratch.write("big-hosts.toml", &hosts_text(disk), 0o644);
    let args = replace(base_args(&scratch), "--pool-config", path.into_os_string());
    Harness::with(scratch, args, None).await
}

/// A request for one GPU that books `disk` bytes of scratch disk, or none when `None`.
fn lease(disk: Option<Value>) -> Value {
    let mut resources = json!({
        "cpu_millis": 1000,
        "memory_bytes": GIB,
        "gpus": 1,
        "gpu_class": "nvidia-a100",
    });
    if let Some(disk) = disk {
        resources["disk_bytes"] = disk;
    }
    json!({
        "class": "gpu-host",
        "resources": resources,
        "capabilities": { "isa": "x86-64-v3" },
        "max_wall_seconds": 7200,
    })
}

/// Asks `h` for a lease booking `disk` bytes and answers the status and the error code, or
/// "granted".
async fn ask(h: &Harness, disk: u64) -> (u16, String) {
    let reply = h
        .call("POST", "/v1/leases", Some(&lease(Some(json!(disk)))))
        .await;
    let said = match reply.status {
        201 => "granted".to_owned(),
        _ => reply.json()["error"].as_str().unwrap_or("").to_owned(),
    };
    (reply.status, said)
}

/// Asserts that a request for exactly the disk the host has is granted end to end, and
/// one byte more is refused `no-fit` and books nothing. It catches a body field that is
/// read and dropped (the request would book no disk and be granted), and a booking that
/// is off by one at the edge.
#[tokio::test]
async fn a_request_for_the_hosts_disk_is_granted_and_one_byte_more_is_refused() {
    let h = serve("disk_edge", DISK).await;
    assert_eq!(ask(&h, DISK + 1).await, (409, "no-fit".to_owned()));
    assert_eq!(ask(&h, DISK).await, (201, "granted".to_owned()));
}

/// Asserts that a body without `disk_bytes` (a client that predates the field) is granted
/// on a host with no disk to book, and that a request for any disk is refused there. It
/// catches a field that is required, and a host with no disk that is read as one with
/// plenty.
#[tokio::test]
async fn a_request_without_disk_needs_none_and_a_host_without_disk_has_none() {
    let h = serve("disk_none", 0).await;
    let plain = h.call("POST", "/v1/leases", Some(&lease(None))).await;
    assert_eq!(plain.status, 201, "{}", plain.body);
    assert_eq!(ask(&h, 1).await, (409, "no-fit".to_owned()));
}

/// Asserts that a `disk_bytes` of zero, above 2^46 - 1 or not a number is a 400 that
/// names the field where it can, books nothing, and leaves the host able to serve the next
/// request. It catches a bound that is missing and an invalid request that reaches the
/// table.
#[tokio::test]
async fn a_bad_disk_bytes_is_a_400_and_books_nothing() {
    let h = serve("disk_bad", DISK).await;
    for bad in [json!(0), json!((1u64 << 46)), json!("lots"), json!(-1)] {
        let reply = h
            .call("POST", "/v1/leases", Some(&lease(Some(bad.clone()))))
            .await;
        assert_eq!(reply.status, 400, "{bad}: {}", reply.body);
    }
    assert_eq!(ask(&h, DISK).await, (201, "granted".to_owned()));
}

/// Asserts that `GET /v1/hosts` shows the disk a host books under `shape.disk_bytes`, and
/// that a host with none has no such key. It catches an operator view that leaves the disk
/// out.
#[tokio::test]
async fn the_hosts_list_shows_the_disk() {
    let h = serve("disk_listed", DISK).await;
    let listed = h.call("GET", "/v1/hosts", None).await.json();
    assert_eq!(listed["hosts"][0]["shape"]["disk_bytes"], json!(DISK));
    let h = serve("disk_listed_none", 0).await;
    let listed = h.call("GET", "/v1/hosts", None).await.json();
    assert!(listed["hosts"][0]["shape"].get("disk_bytes").is_none());
}
