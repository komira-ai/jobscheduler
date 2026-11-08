//! The accelerator of a job through `POST /v1/jobs`: the `gpus` and `gpuClass` needs of
//! today, resolved through the alias table, the optional `constraints.accelerator` message,
//! and a host whose hello predates the accelerator.
//!
//! The scheduler places the host by the shape the registry lists for it, the accelerator
//! facts of its hello included. The scheduler's mapping of the same need is covered in process, in
//! `crates/js-sched-placer/src/map.rs`. A job placed on a host with GPUs is not sent to its
//! agent, so it reads `JOB_STATE_PLACED` with the host it is placed on.

use serde_json::{Value, json};

use crate::agents_support::{entry, hosts};
use crate::jobs_support::{JobBody, job_agent, job_args, job_of, job_server, submit_body};
use crate::support::{Harness, Reply, Scratch};

/// The 80 GiB variant of the A100 SXM4 the alias table knows.
const A100_80: &str = "nvidia-a100-sxm4-80gb";

/// The 40 GiB variant of the same family.
const A100_40: &str = "nvidia-a100-sxm4-40gb";

/// Bytes in a GiB.
const GIB: u64 = 1 << 30;

/// The pool a host registers into: eight GPUs of `class` and a floor of the same, on
/// `x86_64` (the scheduler describes a host by its `arch`).
fn pool(class: &str) -> String {
    format!(
        r#"
name = "burst"
class = "gpu-host"
max_machines = 1

[shape]
cpu_millis = 8000
memory_bytes = 17179869184
gpus = 8
gpu_class = "{class}"

[capabilities]
isa = "x86-64-v3"
arch = "x86_64"
"#
    )
}

/// The hello of an agent that predates `shape.accelerator`: the class, the count and one
/// device record of `memory_gib` GiB per GPU, and no accelerator.
fn old_hello(class: &str, memory_gib: u64) -> Value {
    let devices: Vec<Value> = (0..8)
        .map(|index| {
            json!({
                "index": index,
                "uuid": format!("gpu-{index}"),
                "model": class,
                "memory_bytes": memory_gib * GIB,
            })
        })
        .collect();
    json!({
        "host_id": "burst-1",
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 8,
            "gpu_class": class,
        },
        "gpu_devices": devices,
        "capabilities": { "isa": "x86-64-v3" },
        "labels": {},
        "phase": "serving",
    })
}

/// A server whose only host is `burst-1`, registered with `hello` (with `arch` and the per-job messages), in a pool of
/// `pool_class`.
async fn serve(name: &str, pool_class: &str, hello: &Value) -> Harness {
    let scratch = Scratch::new(name);
    let args = job_args(&scratch, "127.0.0.1:0");
    let mut hello = hello.clone();
    hello["capabilities"]["arch"] = json!("x86_64");
    hello["capabilities"]["per-job"] = json!("true");
    scratch.write("agent-pool.toml", &pool(pool_class), 0o644);
    let h = Harness::with(scratch, args, None).await;
    job_agent(&h, &hello).await;
    h
}

/// The body of the job `id` for the pool's host with these needs (one core and a
/// kilobyte, plus the GPU needs `gpus` gives as `(count, class)`) and the constraints
/// `accelerator` gives,
fn job(id: &str, gpus: Option<(u32, &str)>, accelerator: Option<Value>) -> Value {
    let mut body = JobBody::new(id).needs(1000, 1024);
    if let Some((count, class)) = gpus {
        body = body.gpus(count, class);
    }
    if let Some(accelerator) = accelerator {
        body = body.constraints(json!({ "accelerator": accelerator }));
    }
    body.json()
}

/// Submits `body` and steps a tick.
async fn submit(h: &Harness, body: &Value) -> Reply {
    let reply = submit_body(h, body).await;
    h.ticks.step().await;
    reply
}

/// Asserts that submitting `body` queued the job `id` and the tick placed it on the one
/// registered host.
async fn assert_placed(h: &Harness, id: &str, body: &Value, why: &str) {
    let reply = submit(h, body).await;
    assert_eq!(reply.status, 201, "{why}: {}", reply.body);
    let read = job_of(h, id).await.json();
    assert_eq!(read["job"]["state"], "JOB_STATE_PLACED", "{why}: {read}");
    assert_eq!(read["job"]["hostId"], "burst-1", "{why}");
}

/// Asserts that submitting `body` is refused 422 `no-capable-host` and the job `id` is not
/// recorded.
async fn assert_refused(h: &Harness, id: &str, body: &Value, why: &str) {
    let refused = submit(h, body).await;
    assert_eq!(refused.status, 422, "{why}: {}", refused.body);
    assert_eq!(
        refused.json(),
        json!({ "error": "no-capable-host" }),
        "{why}"
    );
    assert_eq!(job_of(h, id).await.status, 404, "{why}");
}

/// Asserts that submitting `body` is admitted (queued) and that the tick leaves the job `id`
/// queued, placed on no host.
async fn assert_unplaced(h: &Harness, id: &str, body: &Value, why: &str) {
    let reply = submit(h, body).await;
    assert_eq!(reply.status, 201, "{why}: {}", reply.body);
    let read = job_of(h, id).await.json();
    assert_eq!(read["job"]["state"], "JOB_STATE_QUEUED", "{why}: {read}");
    assert_eq!(read["job"].get("hostId"), None, "{why}");
}

/// Asserts that a job asking for `gpus` and `gpuClass` only (the body of today) is placed
/// on a host of eight `nvidia-a100-sxm4-80gb` when its class is that full spelling and when
/// it is the family `nvidia-a100`, and is refused when it names another family, the 40 GiB
/// variant on that host of 80 GiB devices, or the 80 GiB variant on a host of 40 GiB
/// devices. Catches a class compared as text, which would leave a family request
/// unplaceable, and one compared as a family only, or by memory at least, which would
/// place a full spelling on another variant.
#[tokio::test]
async fn todays_full_spelling_requests_still_match() {
    let hello = old_hello(A100_80, 80);
    for (name, class) in [("accel_full", A100_80), ("accel_family", "nvidia-a100")] {
        let h = serve(name, A100_80, &hello).await;
        let body = job("accel-today", Some((4, class)), None);
        assert_placed(&h, "accel-today", &body, class).await;
        let listing = hosts(&h).await;
        let host = entry(&listing, "burst-1");
        assert_eq!(host["shape"]["gpus"], 8, "{class}");
        assert_eq!(host["shape"]["gpu_class"], A100_80, "{class}");
    }
    let h = serve("accel_other_family", A100_80, &hello).await;
    for class in ["nvidia-h100", A100_40] {
        let body = job("accel-other", Some((4, class)), None);
        assert_refused(&h, "accel-other", &body, class).await;
    }

    let small = old_hello(A100_40, 40);
    let h = serve("accel_smaller_variant", A100_40, &small).await;
    let body = job("accel-smaller", Some((4, A100_80)), None);
    assert_refused(&h, "accel-smaller", &body, A100_80).await;
}

/// Asserts that a hello with `gpus`, `gpu_class` and `gpu_devices` and no accelerator
/// registers the host, that the host is listed with the shape it sent, and that a job
/// whose `constraints.accelerator` names the family `nvidia-a100`, 80 GiB per device and a
/// count is placed on it. Catches a hello that is refused for lacking the new field, and
/// a host whose accelerator is not derived from its class.
#[tokio::test]
async fn old_hello_with_gpu_class_still_registers() {
    let h = serve("accel_old_hello", A100_80, &old_hello(A100_80, 80)).await;
    let listing = hosts(&h).await;
    let host = entry(&listing, "burst-1");
    assert_eq!(host["source"], "registered");
    assert_eq!(host["state"], "live");
    assert_eq!(
        host["shape"],
        json!({
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 8,
            "gpu_class": A100_80,
        })
    );
    let accelerator = json!({ "model": "nvidia-a100", "memoryPerDeviceGib": 80, "count": 4 });
    let body = job("accel-object", None, Some(accelerator));
    assert_placed(&h, "accel-object", &body, "accelerator object").await;
}

/// Asserts that GPUs counted with no class are refused through the route as a 400 that
/// names the field, whether the count stands alone in `needs` or beside an accelerator
/// message that names no model, that nothing is recorded by either, and that an unknown key
/// of the accelerator message is a 400 as well. Catches GPUs booked with no class.
#[tokio::test]
async fn gpus_counted_with_no_class_are_a_400_naming_the_field() {
    let h = job_server("accel_count_only").await;
    let mut alone = job("accel-count", None, None);
    alone["spec"]["needs"]["gpus"] = json!(4);
    let mut beside = job("accel-beside", None, Some(json!({ "count": 4 })));
    beside["spec"]["needs"]["gpus"] = json!(4);
    for (id, body) in [("accel-count", alone), ("accel-beside", beside)] {
        let refused = submit_body(&h, &body).await;
        assert_eq!(refused.status, 400, "{id}: {}", refused.body);
        assert_eq!(job_of(&h, id).await.status, 404);
    }
    let unknown = job("accel-cores", None, Some(json!({ "cores": 4 })));
    let refused = submit_body(&h, &unknown).await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert_eq!(job_of(&h, "accel-cores").await.status, 404);
}

/// Asserts that a host of an old agent whose class the alias table does not know
/// (`nvidia-zz9000`, eight devices of 40 GiB) is placed for a `constraints.accelerator` of
/// that model with 40 GiB per device and left queued, placed on no host, for 41 GiB, and that a
/// `nvidia-zz9000` job through `gpus` and `gpuClass` is placed too. Catches the measured
/// memory of the devices being lost between the hello and placement.
#[tokio::test]
async fn the_measured_memory_of_an_unknown_model_decides_placement() {
    let hello = old_hello("nvidia-zz9000", 40);
    let ask = |id: &str, gib: u64| {
        let accelerator =
            json!({ "model": "nvidia-zz9000", "memoryPerDeviceGib": gib, "count": 4 });
        job(id, None, Some(accelerator))
    };
    let h = serve("accel_zz_fits", "nvidia-zz9000", &hello).await;
    assert_placed(&h, "zz-40", &ask("zz-40", 40), "40 GiB").await;
    let h = serve("accel_zz_over", "nvidia-zz9000", &hello).await;
    assert_unplaced(&h, "zz-41", &ask("zz-41", 41), "41 GiB").await;
    let h = serve("accel_zz_class", "nvidia-zz9000", &hello).await;
    let body = job("zz-class", Some((4, "nvidia-zz9000")), None);
    assert_placed(&h, "zz-class", &body, "class only").await;
}

/// Asserts that a pool whose floor names the family `nvidia-a100` registers a host of
/// `nvidia-a100-sxm4-80gb`, and that a `gpus` and `gpuClass` job for exactly that spelling
/// is placed on it while one for `nvidia-a100-sxm4-40gb` is refused 422. Catches a host
/// placed as the pool floor's family, which would take either variant: a scheduler told
/// the pool's floor at the hello in place of the shape the registry lists for the host.
#[tokio::test]
async fn a_host_is_placed_as_its_variant_and_not_as_the_pool_floors_family() {
    let hello = old_hello(A100_80, 80);
    let h = serve("accel_floor_family_80", "nvidia-a100", &hello).await;
    let body = job("floor-80", Some((4, A100_80)), None);
    assert_placed(&h, "floor-80", &body, A100_80).await;
    let h = serve("accel_floor_family_40", "nvidia-a100", &hello).await;
    let body = job("floor-40", Some((4, A100_40)), None);
    assert_refused(&h, "floor-40", &body, A100_40).await;
}
