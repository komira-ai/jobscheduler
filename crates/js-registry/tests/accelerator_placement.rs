//! The accelerator a registered host is placed on: the facts its hello states, validated,
//! and not the pool floor's class or what the host reports as plain capabilities.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{GpuDevice, Hello};
use js_provider::{Pool, RefusalCode, StartAck, StartSpec};
use js_registry::PoolConfig;
use js_types::{Capabilities, ClassId, ProviderId, Request, Resources, Shape};
use support::*;

const GIB: u64 = 1 << 30;

/// A pool whose floor is one GPU of `floor_class`, on macOS hosts.
fn gpu_fixture(floor_class: &str) -> Fixture {
    let resources = Resources::new(4000, 8 << 30, 1, Some(floor_class.into())).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos")]));
    let name = ProviderId::new("builders").unwrap();
    fixture_of(PoolConfig::new(name, class(), shape, 4))
}

/// The hello of an agent that predates `shape.accelerator`: one GPU of `class` with one
/// device of `memory_gib` GiB.
fn old_gpu_hello(class: &str, memory_gib: u64) -> Hello {
    let mut hello = hello("gpu-1");
    hello.shape.gpus = 1;
    hello.shape.gpu_class = Some(class.into());
    hello.gpu_devices = vec![GpuDevice {
        index: 0,
        uuid: "GPU-1".into(),
        model: class.into(),
        memory_bytes: memory_gib * GIB,
    }];
    hello
}

/// A request for one GPU of `gpu_class` with these extra `capabilities`.
fn gpu_request(gpu_class: &str, extra: &[(&str, &str)]) -> Request {
    let mut capabilities: Capabilities = map(&[("os", "macos")]);
    capabilities.extend(map(extra));
    Request {
        class: ClassId::new("mac-host").unwrap(),
        resources: Resources::new(1000, 1, 1, Some(gpu_class.into())).unwrap(),
        capabilities,
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    }
}

/// Whether `request` is placed on the one registered host of `fx`.
fn placed(fx: &Fixture, request: Request) -> bool {
    let spec = StartSpec::new(lease(1), request, &[class()]).unwrap();
    started_on(&fx.provider.clone().start(spec)) == Some("gpu-1")
}

/// Asserts that a host of an old agent (no `shape.accelerator`) with a class the alias
/// table does not know and one device of 40 GiB is placed for a request of that model with
/// 40 GiB per device, and not for 41 GiB. Catches a placement shape that has lost the
/// measured memory of a model the table does not know.
#[test]
fn a_measured_memory_of_an_unknown_model_reaches_placement() {
    let ask = |gib: &str| {
        let caps = [
            ("accelerator.model", "nvidia-zz9000"),
            ("accelerator.memory-gib", gib),
        ];
        gpu_request("nvidia-zz9000", &caps)
    };
    for (gib, expected) in [("40", true), ("41", false)] {
        let fx = gpu_fixture("nvidia-zz9000");
        fx.hello(&old_gpu_hello("nvidia-zz9000", 40), &cred("t"))
            .unwrap();
        assert_eq!(placed(&fx, ask(gib)), expected, "{gib} GiB");
    }
}

/// Asserts that a pool whose floor names the family `nvidia-a100` admits a host of
/// `nvidia-a100-sxm4-80gb`, and places a request for exactly that spelling on it but not
/// one for `nvidia-a100-sxm4-40gb`. Catches a host placed as the pool floor's class, which
/// would take the 40 GiB variant.
#[test]
fn the_hosts_variant_and_not_the_pool_floors_class_is_placed_on() {
    let hello = old_gpu_hello("nvidia-a100-sxm4-80gb", 80);
    for (class, expected) in [
        ("nvidia-a100-sxm4-80gb", true),
        ("nvidia-a100-sxm4-40gb", false),
    ] {
        let fx = gpu_fixture("nvidia-a100");
        fx.hello(&hello, &cred("t")).unwrap();
        assert_eq!(placed(&fx, gpu_request(class, &[])), expected, "{class}");
    }
}

/// Asserts that a hello of one A100 that also reports `accelerator.model=nvidia-h100`,
/// a count of 8 and 999 GiB as plain capabilities is placed as an A100: a request for the
/// H100 or for 80 GiB is not placed, and one for the A100 is. Catches raw capabilities
/// overriding the facts the class and devices state.
#[test]
fn reported_accelerator_capabilities_do_not_change_where_a_host_is_placed() {
    let mut hello = old_gpu_hello("nvidia-a100", 40);
    hello.capabilities.extend(map(&[
        ("accelerator.model", "nvidia-h100"),
        ("accelerator.count", "8"),
        ("accelerator.memory-gib", "999"),
    ]));
    let asks = [
        ("nvidia-h100", "40", false),
        ("nvidia-a100", "80", false),
        ("nvidia-a100", "40", true),
    ];
    for (model, gib, expected) in asks {
        let fx = gpu_fixture("nvidia-a100");
        fx.hello(&hello, &cred("t")).unwrap();
        let caps = [
            ("accelerator.model", model),
            ("accelerator.memory-gib", gib),
        ];
        // Each request books the model it names, so the H100 one is a valid request that
        // the host's validated A100 cannot serve, not one refused for contradicting itself.
        assert_eq!(
            placed(&fx, gpu_request(model, &caps)),
            expected,
            "{model} {gib}"
        );
    }
}

/// A fixture of [`gpu_fixture`]'s pool under `placement`, whose floor and host also name
/// `arch` (js-sched describes a host by it), with one host of `hello` registered.
fn gpu_fixture_in(floor_class: &str, hello: &Hello, placement: Placement) -> Fixture {
    let resources = Resources::new(4000, 8 << 30, 1, Some(floor_class.into())).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos"), ("arch", "x86_64")]));
    let name = ProviderId::new("builders").unwrap();
    let fx = fixture_in(PoolConfig::new(name, class(), shape, 4), placement);
    let mut said = hello.clone();
    said.capabilities.insert("arch".into(), "x86_64".into());
    fx.hello(&said, &cred("t")).unwrap();
    fx
}

/// What a start of `request` on `fx` answers.
fn start(fx: &Fixture, request: Request) -> StartAck {
    let spec = StartSpec::new(lease(1), request, &[class()]).unwrap();
    fx.provider.clone().start(spec)
}

/// Asserts, under each placement, that a pool whose floor names the family `nvidia-a100`
/// places a request for `nvidia-a100-sxm4-80gb` on its host of that variant, and refuses
/// one for `nvidia-a100-sxm4-40gb` `no-capacity`: the floor answers for the family, not
/// the variant, so a host of the 40 GiB variant could still join. Catches a pool-fit gate
/// that holds a request to the floor's family alone (the floor fits no variant spelling,
/// so under js-sched the placer was never asked and the start was `no-fit`), and a host
/// placed as the floor's family.
#[test]
fn a_variant_request_is_placed_on_its_variant_under_each_placement() {
    let hello = old_gpu_hello("nvidia-a100-sxm4-80gb", 80);
    for placement in [Placement::Registry, Placement::JsSched] {
        let fx = gpu_fixture_in("nvidia-a100", &hello, placement);
        let ack = start(&fx, gpu_request("nvidia-a100-sxm4-80gb", &[]));
        assert_eq!(started_on(&ack), Some("gpu-1"), "{placement:?}");
        let fx = gpu_fixture_in("nvidia-a100", &hello, placement);
        let ack = start(&fx, gpu_request("nvidia-a100-sxm4-40gb", &[]));
        assert_eq!(
            ack,
            StartAck::Refused(RefusalCode::NoCapacity),
            "{placement:?}"
        );
    }
}

/// Asserts, under each placement, that a host of a model the alias table does not know,
/// whose devices measure 40 GiB, takes a request for that model with 40 GiB per device,
/// and that one for 41 GiB is refused `no-capacity` (the floor knows no memory, so a
/// larger host could join). Catches the measured memory of the host lost before js-sched
/// places, and a gate that holds the request to the floor, which states no memory.
#[test]
fn a_measured_memory_decides_placement_under_each_placement() {
    let hello = old_gpu_hello("nvidia-zz9000", 40);
    let ask = |gib: &str| {
        let caps = [
            ("accelerator.model", "nvidia-zz9000"),
            ("accelerator.memory-gib", gib),
        ];
        gpu_request("nvidia-zz9000", &caps)
    };
    for placement in [Placement::Registry, Placement::JsSched] {
        let fx = gpu_fixture_in("nvidia-zz9000", &hello, placement);
        assert_eq!(
            started_on(&start(&fx, ask("40"))),
            Some("gpu-1"),
            "{placement:?}"
        );
        let fx = gpu_fixture_in("nvidia-zz9000", &hello, placement);
        let over = start(&fx, ask("41"));
        assert_eq!(
            over,
            StartAck::Refused(RefusalCode::NoCapacity),
            "{placement:?}"
        );
    }
}

/// Asserts, under each placement, that a pool whose floor pins the variant
/// `nvidia-a100-sxm4-80gb` refuses a request for `nvidia-a100-sxm4-40gb` `no-fit`: every
/// host is held to the floor's variant, so none ever serves another, and a client must not
/// be told to retry. A request for 81 GiB per device is refused `no-fit` too, and the
/// floor's own variant is placed. Catches a gate that leaves a floor's variant to the
/// hosts, as it leaves a family's.
#[test]
fn a_floor_that_pins_a_variant_answers_for_it_under_each_placement() {
    let a100_80 = "nvidia-a100-sxm4-80gb";
    let hello = old_gpu_hello(a100_80, 80);
    for placement in [Placement::Registry, Placement::JsSched] {
        let fx = gpu_fixture_in(a100_80, &hello, placement);
        let other = start(&fx, gpu_request("nvidia-a100-sxm4-40gb", &[]));
        assert_eq!(
            other,
            StartAck::Refused(RefusalCode::NoFit),
            "{placement:?}"
        );
        let more = start(
            &fx,
            gpu_request(a100_80, &[("accelerator.memory-gib", "81")]),
        );
        assert_eq!(more, StartAck::Refused(RefusalCode::NoFit), "{placement:?}");
        let own = start(&fx, gpu_request(a100_80, &[]));
        assert_eq!(started_on(&own), Some("gpu-1"), "{placement:?}");
    }
}
