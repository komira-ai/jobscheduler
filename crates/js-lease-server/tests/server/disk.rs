//! Scratch disk over HTTP: `needs.diskBytes` of `POST /v1/jobs` is judged against the disk
//! of a registered host, and the disk a host books is shown in `GET /v1/hosts`. (The sum
//! over jobs is the scheduler's, tested in `js-sched`.)

use serde_json::{Value, json};

use crate::agents_support::POOL;
use crate::jobs_support::{JobBody, job_agent, job_args, job_of, per_job_hello, submit_body};
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

/// A server with the job service on and one registered host, `burst-1`, of 8 cores, 16 GiB
/// and one `nvidia-a100`, whose pool file and hello list `disk` bytes of scratch disk and
/// report it as the `disk-gib` fact when given.
async fn registered(name: &str, disk: Option<u64>) -> Harness {
    let scratch = Scratch::new(name);
    let args = job_args(&scratch, "127.0.0.1:0");
    let mut hello = per_job_hello("burst-1");
    if let Some(disk) = disk {
        let gib = (disk / GIB).to_string();
        let sized = POOL.replace(
            "gpu_class = \"nvidia-a100\"\n",
            &format!("gpu_class = \"nvidia-a100\"\ndisk_bytes = {disk}\n"),
        );
        let pool = format!("{sized}disk-gib = \"{gib}\"\n");
        scratch.write("agent-pool.toml", &pool, 0o644);
        hello["shape"]["disk_bytes"] = json!(disk);
        hello["capabilities"]["disk-gib"] = json!(gib);
    }
    let h = Harness::with(scratch, args, None).await;
    job_agent(&h, &hello).await;
    h
}

/// The body of a job for one GPU that needs `disk` (a `diskBytes` value) bytes of scratch
/// disk, or none when `None`.
fn job(id: &str, disk: Option<Value>) -> Value {
    let mut body = JobBody::new(id).gpus(1, "nvidia-a100").json();
    if let Some(disk) = disk {
        body["spec"]["needs"]["diskBytes"] = disk;
    }
    body
}

/// Submits a job needing `disk` bytes and answers the status and the error code, or
/// "queued".
async fn ask(h: &Harness, id: &str, disk: u64) -> (u16, String) {
    let reply = submit_body(h, &job(id, Some(json!(disk.to_string())))).await;
    let said = match reply.status {
        201 => "queued".to_owned(),
        _ => reply.json()["error"].as_str().unwrap_or("").to_owned(),
    };
    (reply.status, said)
}

/// Asserts that a job needing exactly the disk the registered host has is admitted, and one
/// byte more is refused `no-capable-host` (no host could ever hold it) and is not
/// recorded. It catches a body field that is read and dropped (the job would need no disk
/// and be admitted), and a bound that is off by one at the edge.
#[tokio::test]
async fn a_job_needing_the_hosts_disk_is_admitted_and_one_byte_more_is_refused() {
    let h = registered("disk_edge", Some(DISK)).await;
    assert_eq!(
        ask(&h, "disk-over", DISK + 1).await,
        (422, "no-capable-host".to_owned())
    );
    assert_eq!(job_of(&h, "disk-over").await.status, 404);
    assert_eq!(ask(&h, "disk-edge", DISK).await, (201, "queued".to_owned()));
}

/// Asserts that a job without `diskBytes` is admitted on a host with no disk to book, and
/// that a job needing any disk is refused there (`no-capable-host`). It catches a field
/// that is required, and a host with no disk that is read as one with plenty.
#[tokio::test]
async fn a_job_without_disk_needs_none_and_a_host_without_disk_has_none() {
    let h = registered("disk_none", None).await;
    let plain = submit_body(&h, &job("disk-plain", None)).await;
    assert_eq!(plain.status, 201, "{}", plain.body);
    assert_eq!(
        ask(&h, "disk-some", 1).await,
        (422, "no-capable-host".to_owned())
    );
}

/// Asserts that a `diskBytes` that is not a number is a 400 that books nothing, that one no
/// host could hold (2^46) is refused `no-capable-host`, that a `diskBytes` of zero is the
/// same as none (the job is admitted, needing no disk), and that the host stays able to
/// serve the next job. It catches an invalid job that reaches admission.
#[tokio::test]
async fn a_bad_disk_bytes_is_a_400_and_books_nothing() {
    let h = registered("disk_bad", Some(DISK)).await;
    for bad in [json!("lots"), json!("-1")] {
        let reply = submit_body(&h, &job("disk-bad", Some(bad.clone()))).await;
        assert_eq!(reply.status, 400, "{bad}: {}", reply.body);
        assert_eq!(job_of(&h, "disk-bad").await.status, 404);
    }
    let huge = job("disk-huge", Some(json!((1u64 << 46).to_string())));
    let reply = submit_body(&h, &huge).await;
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(job_of(&h, "disk-huge").await.status, 404);
    let zero = submit_body(&h, &job("disk-zero", Some(json!("0")))).await;
    assert_eq!(zero.status, 201, "{}", zero.body);
    assert_eq!(ask(&h, "disk-good", DISK).await, (201, "queued".to_owned()));
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
