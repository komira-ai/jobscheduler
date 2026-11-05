//! The accelerator of a lease request through `POST /v1/leases`: the `gpus` and
//! `gpu_class` bodies of today, resolved through the alias table, the optional
//! `resources.accelerator` object, and a host whose hello predates the accelerator.
//!
//! These run on the default placement (the registry). The scheduler's mapping of the same
//! need is covered in process, in `src/sched/map.rs`.

use serde_json::{Value, json};

use crate::agents_support::{JOIN, agent_args, entry, hosts, post, session};
use crate::support::{Harness, Reply, Scratch};

/// The 80 GiB variant of the A100 SXM4 the alias table knows.
const A100_80: &str = "nvidia-a100-sxm4-80gb";

/// The 40 GiB variant of the same family.
const A100_40: &str = "nvidia-a100-sxm4-40gb";

/// Bytes in a GiB.
const GIB: u64 = 1 << 30;

/// The pool a host registers into: eight GPUs of `class` and a floor of the same.
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

/// A server whose only host is `burst-1`, registered with `hello`, in a pool of
/// `pool_class`.
async fn serve(name: &str, pool_class: &str, hello: &Value) -> Harness {
    let scratch = Scratch::new(name);
    let args = agent_args(&scratch, false);
    scratch.write("agent-pool.toml", &pool(pool_class), 0o644);
    let h = Harness::with(scratch, args, None).await;
    let reply = post(&h, "/v1/agent/hello", Some(JOIN), hello).await;
    session(&reply, "burst-1");
    h
}

/// The body of a lease for the pool's class with these `resources` (one core and a
/// kilobyte, plus what `extra` adds).
fn lease(extra: Value) -> Value {
    let mut resources = json!({ "cpu_millis": 1000, "memory_bytes": 1024 });
    for (key, value) in extra.as_object().unwrap() {
        resources[key] = value.clone();
    }
    json!({
        "class": "gpu-host",
        "resources": resources,
        "capabilities": { "isa": "x86-64-v3" },
        "max_wall_seconds": 7200,
    })
}

/// Asks `h` for `body`.
async fn acquire(h: &Harness, body: &Value) -> Reply {
    h.call("POST", "/v1/leases", Some(body)).await
}

/// Asserts that `reply` placed the lease on the one registered host.
fn assert_placed(reply: &Reply, why: &str) {
    assert_eq!(reply.status, 201, "{why}: {}", reply.body);
    assert_eq!(reply.json()["host"], "burst-1", "{why}");
}

/// Asserts that a lease asking for `gpus` and `gpu_class` only (the body of today) is
/// placed on a host of eight `nvidia-a100-sxm4-80gb` when its class is that full
/// spelling and when it is the family `nvidia-a100`, and is not placed when it names
/// another family, the 40 GiB variant on that host of 80 GiB devices, or the 80 GiB
/// variant on a host of 40 GiB devices. Catches a class compared as text, which would
/// leave a family request unplaceable, and one compared as a family only, or by memory at
/// least, which would place a full spelling on another variant.
#[tokio::test]
async fn todays_full_spelling_requests_still_match() {
    let hello = old_hello(A100_80, 80);
    for (name, class) in [("accel_full", A100_80), ("accel_family", "nvidia-a100")] {
        let h = serve(name, A100_80, &hello).await;
        let body = lease(json!({ "gpus": 4, "gpu_class": class }));
        assert_placed(&acquire(&h, &body).await, class);
        let listing = hosts(&h).await;
        let host = entry(&listing, "burst-1");
        assert_eq!(host["shape"]["gpus"], 8, "{class}");
        assert_eq!(host["shape"]["gpu_class"], A100_80, "{class}");
    }
    let h = serve("accel_other_family", A100_80, &hello).await;
    for class in ["nvidia-h100", A100_40] {
        let body = lease(json!({ "gpus": 4, "gpu_class": class }));
        let refused = acquire(&h, &body).await;
        assert_eq!(refused.status, 409, "{class}: {}", refused.body);
    }
    let held = h.call("GET", "/v1/leases", None).await;
    assert_eq!(held.json()["leases"], json!([]));

    let small = old_hello(A100_40, 40);
    let h = serve("accel_smaller_variant", A100_40, &small).await;
    let body = lease(json!({ "gpus": 4, "gpu_class": A100_80 }));
    let refused = acquire(&h, &body).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    let held = h.call("GET", "/v1/leases", None).await;
    assert_eq!(held.json()["leases"], json!([]));
}

/// Asserts that a hello with `gpus`, `gpu_class` and `gpu_devices` and no accelerator
/// registers the host, that the host is listed with the shape it sent, and that a lease
/// whose `resources.accelerator` names the family `nvidia-a100`, 80 GiB per device and a
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
    let body = lease(json!({
        "accelerator": { "model": "nvidia-a100", "memory_per_device_gib": 80, "count": 4 },
    }));
    assert_placed(&acquire(&h, &body).await, "accelerator object");
}

/// Asserts that a count with no model is refused through the route as a 400 whose reason
/// names `accelerator.model`, whether it is in the `resources.accelerator` object or an
/// `accelerator.count` capability, that nothing is booked by either, and that an unknown
/// key of the object is a 400 as well. Catches GPUs booked with no class, which a previous
/// release would read back from the lease file as a whole host.
#[tokio::test]
async fn a_count_only_accelerator_is_a_400_naming_accelerator_model() {
    let h = Harness::new("accel_count_only").await;
    let said = "accelerator.model is required when accelerator.count is above zero";
    let object = lease(json!({ "accelerator": { "count": 4 } }));
    let mut capability = lease(json!({}));
    capability["capabilities"] = json!({ "accelerator.count": "4" });
    for body in [object, capability] {
        let refused = acquire(&h, &body).await;
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert_eq!(
            refused.json(),
            json!({ "error": "bad-request", "reason": said })
        );
    }
    let unknown = lease(json!({ "accelerator": { "cores": 4 } }));
    assert_eq!(acquire(&h, &unknown).await.status, 400);
    let held = h.call("GET", "/v1/leases", None).await;
    assert_eq!(held.json()["leases"], json!([]));
}

/// Asserts that a host of an old agent whose class the alias table does not know
/// (`nvidia-zz9000`, eight devices of 40 GiB) is placed for a `resources.accelerator` of
/// that model with 40 GiB per device and refused for 41 GiB with a 409, and that a
/// `nvidia-zz9000` request through `gpus` and `gpu_class` is placed too. Catches the
/// measured memory of the devices being lost between the hello and placement.
#[tokio::test]
async fn the_measured_memory_of_an_unknown_model_decides_placement() {
    let hello = old_hello("nvidia-zz9000", 40);
    let ask = |gib: u64| {
        lease(json!({
            "accelerator": { "model": "nvidia-zz9000", "memory_per_device_gib": gib, "count": 4 },
        }))
    };
    let h = serve("accel_zz_fits", "nvidia-zz9000", &hello).await;
    assert_placed(&acquire(&h, &ask(40)).await, "40 GiB");
    let h = serve("accel_zz_over", "nvidia-zz9000", &hello).await;
    let refused = acquire(&h, &ask(41)).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    let h = serve("accel_zz_class", "nvidia-zz9000", &hello).await;
    let body = lease(json!({ "gpus": 4, "gpu_class": "nvidia-zz9000" }));
    assert_placed(&acquire(&h, &body).await, "class only");
}

/// Asserts that a pool whose floor names the family `nvidia-a100` registers a host of
/// `nvidia-a100-sxm4-80gb`, and that a `gpus` and `gpu_class` request for exactly that
/// spelling is placed on it while one for `nvidia-a100-sxm4-40gb` is a 409. Catches a
/// host placed as the pool floor's family, which would take either variant.
#[tokio::test]
async fn a_host_is_placed_as_its_variant_and_not_as_the_pool_floors_family() {
    let hello = old_hello(A100_80, 80);
    let h = serve("accel_floor_family_80", "nvidia-a100", &hello).await;
    let body = lease(json!({ "gpus": 4, "gpu_class": A100_80 }));
    assert_placed(&acquire(&h, &body).await, A100_80);
    let h = serve("accel_floor_family_40", "nvidia-a100", &hello).await;
    let body = lease(json!({ "gpus": 4, "gpu_class": A100_40 }));
    let refused = acquire(&h, &body).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
}
