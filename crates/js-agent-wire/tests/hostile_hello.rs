//! Hostile input to a hello: every rule has a test, and each says the defect it catches.

mod common;

use common::{broken, expect, hello, long, map};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{
    GpuDevice, HeldRun, HeldState, Hello, InstanceIdentity, ProtocolRange, Rule, WireAccelerator,
};

fn check(edit: impl FnOnce(&mut Hello)) -> js_agent_wire::ValidationError {
    broken(&hello(), edit, Hello::validate)
}

fn held(id: &str) -> HeldRun {
    HeldRun {
        run_id: id.into(),
        state: HeldState::Running,
    }
}

/// A host id that is empty, upper case, a path, holds NUL or a space, is 64 bytes or a
/// megabyte is refused with the generic hello code. It catches an id that is not a
/// valid `js-types` host id reaching the registry, or used as a path or log key.
#[test]
fn host_id_is_bounded_and_plain() {
    for id in ["", "Upper", "../x", "a/b", "a\0", "a b", ".x"] {
        let err = check(|h| h.host_id = id.into());
        assert_eq!(
            (err.field(), err.code()),
            ("host_id", refusal::INVALID_HELLO),
            "{id:?}"
        );
    }
    expect(
        check(|h| h.host_id = long(64)),
        "host_id",
        Rule::BadChars,
        refusal::INVALID_HELLO,
    );
    expect(
        check(|h| h.host_id = long(MAX_ID_LEN + 1)),
        "host_id",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    expect(
        check(|h| h.host_id = long(1 << 20)),
        "host_id",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    let mut ok = hello();
    ok.host_id = long(63);
    assert_eq!(ok.validate(), Ok(()));
}

/// A pool and an agent version that are empty, too long or hold control characters are
/// refused. It catches unbounded text the registry would store and log.
#[test]
fn pool_and_agent_version_are_bounded() {
    for pool in ["", "a/b", "a\0", "-p"] {
        let err = check(|h| h.pool = pool.into());
        assert_eq!(err.field(), "pool", "{pool:?}");
    }
    expect(
        check(|h| h.pool = long(MAX_ID_LEN + 1)),
        "pool",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    expect(
        check(|h| h.agent_version.clear()),
        "agent_version",
        Rule::Empty,
        refusal::INVALID_HELLO,
    );
    expect(
        check(|h| h.agent_version = long(MAX_VERSION_LEN + 1)),
        "agent_version",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    for version in ["1\n2", "1\u{7f}", "a\0"] {
        let err = check(|h| h.agent_version = version.into());
        expect(err, "agent_version", Rule::BadChars, refusal::INVALID_HELLO);
    }
    let mut ok = hello();
    ok.agent_version = long(MAX_VERSION_LEN);
    ok.pool = long(MAX_ID_LEN);
    assert_eq!(ok.validate(), Ok(()));
}

/// Shape numbers above their caps, a GPU class that is not an id, GPUs without a class
/// or a class without GPUs, and a lease cap of zero or above its cap are refused; the
/// caps pass. It catches a shape that makes later sums overflow and a host booked as GPU
/// capacity of no kind.
#[test]
fn shape_is_bounded_and_consistent() {
    let err = check(|h| h.shape.cpu_millis = MAX_CPU_MILLIS + 1);
    expect(
        err,
        "shape.cpu_millis",
        Rule::OutOfRange,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| h.shape.memory_bytes = u64::MAX);
    expect(
        err,
        "shape.memory_bytes",
        Rule::OutOfRange,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| h.shape.gpus = MAX_GPUS + 1);
    expect(err, "shape.gpus", Rule::OutOfRange, refusal::INVALID_HELLO);
    let err = check(|h| h.shape.gpu_class = Some("a b".into()));
    expect(
        err,
        "shape.gpu_class",
        Rule::BadChars,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| h.shape.gpu_class = None);
    expect(
        err,
        "shape.gpu_class",
        Rule::Inconsistent,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| {
        h.shape.gpus = 0;
        h.gpu_devices.clear();
    });
    expect(
        err,
        "shape.gpu_class",
        Rule::Inconsistent,
        refusal::INVALID_HELLO,
    );
    for cap in [0, MAX_LEASE_CAP + 1] {
        let err = check(|h| h.shape.max_leases = Some(cap));
        expect(
            err,
            "shape.max_leases",
            Rule::OutOfRange,
            refusal::INVALID_HELLO,
        );
    }
    let mut ok = hello();
    ok.shape.cpu_millis = MAX_CPU_MILLIS;
    ok.shape.memory_bytes = MAX_MEMORY_BYTES;
    ok.shape.max_leases = Some(MAX_LEASE_CAP);
    assert_eq!(ok.validate(), Ok(()));
    ok.shape.max_leases = None;
    ok.shape.cpu_millis = 0;
    ok.shape.memory_bytes = 0;
    assert_eq!(ok.validate(), Ok(()));
}

/// Listed GPU devices must number `shape.gpus`; an index twice, too many devices, and
/// a uuid, model or memory out of bounds are refused. It catches a host that reports
/// devices contradicting its shape and a device list that grows without bound.
#[test]
fn gpu_devices_match_the_shape() {
    let device = |index| GpuDevice {
        index,
        uuid: format!("GPU-{index}"),
        model: "A100".into(),
        memory_bytes: 1 << 36,
    };
    let err = check(|h| h.gpu_devices.push(device(1)));
    expect(
        err,
        "gpu_devices",
        Rule::Inconsistent,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| {
        h.shape.gpus = 2;
        h.gpu_devices = vec![device(3), device(3)];
    });
    expect(err, "gpu_devices", Rule::Duplicate, refusal::INVALID_HELLO);
    let err = check(|h| {
        h.shape.gpus = MAX_GPUS;
        h.gpu_devices = (0..=MAX_GPUS as u32).map(device).collect();
    });
    expect(err, "gpu_devices", Rule::TooMany, refusal::INVALID_HELLO);
    let err = check(|h| h.gpu_devices[0].index = MAX_GPUS as u32);
    expect(
        err,
        "gpu_devices.index",
        Rule::OutOfRange,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| h.gpu_devices[0].uuid.clear());
    expect(err, "gpu_devices.uuid", Rule::Empty, refusal::INVALID_HELLO);
    let err = check(|h| h.gpu_devices[0].model = long(MAX_GPU_TEXT_LEN + 1));
    expect(
        err,
        "gpu_devices.model",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| h.gpu_devices[0].memory_bytes = MAX_MEMORY_BYTES + 1);
    expect(
        err,
        "gpu_devices.memory_bytes",
        Rule::OutOfRange,
        refusal::INVALID_HELLO,
    );
    let mut ok = hello();
    ok.shape.gpus = MAX_GPUS;
    ok.gpu_devices = (0..MAX_GPUS as u32).map(device).collect();
    assert_eq!(ok.validate(), Ok(()));
    ok.gpu_devices.clear();
    assert_eq!(ok.validate(), Ok(()), "devices may be left unlisted");
}

/// Capabilities: more than the cap, a key that is not an id, an empty, long or
/// control-character value are refused; the caps pass. It catches an unbounded fact map.
#[test]
fn capabilities_are_bounded() {
    let many = (0..=MAX_CAPABILITIES)
        .map(|i| (format!("k{i}"), "v".to_string()))
        .collect();
    expect(
        check(|h| h.capabilities = many),
        "capabilities",
        Rule::TooMany,
        refusal::INVALID_HELLO,
    );
    for key in ["", "a b", "a\0", "a/b"] {
        let err = check(|h| h.capabilities = map(&[(key, "v")]));
        assert_eq!(err.field(), "capabilities", "{key:?}");
    }
    let key = long(MAX_CAPABILITY_KEY_LEN + 1);
    let err = check(|h| h.capabilities = map(&[(key.as_str(), "v")]));
    expect(err, "capabilities", Rule::TooLong, refusal::INVALID_HELLO);
    let value = long(MAX_CAPABILITY_VALUE_LEN + 1);
    for bad in ["", "a\nb", value.as_str()] {
        let err = check(|h| h.capabilities = map(&[("os", bad)]));
        assert_eq!(err.field(), "capabilities", "{bad:?}");
    }
    let mut ok = hello();
    ok.capabilities = map(&[(
        &long(MAX_CAPABILITY_KEY_LEN),
        &long(MAX_CAPABILITY_VALUE_LEN),
    )]);
    assert_eq!(ok.validate(), Ok(()));
}

/// Labels on a hello: the reserved prefix is refused with its own code, and count, key
/// and value bounds apply. It catches a host forging a scheduler-written label.
#[test]
fn hello_labels_are_bounded_and_not_reserved() {
    let err = check(|h| h.labels = map(&[("jobscheduler.dev/tenant", "x")]));
    expect(err, "labels", Rule::Reserved, refusal::RESERVED_LABEL);
    let err = check(|h| h.labels = map(&[("", "x")]));
    expect(err, "labels", Rule::Empty, refusal::INVALID_HELLO);
    let err = check(|h| h.labels = map(&[("rack", "UPPER")]));
    expect(err, "labels", Rule::BadChars, refusal::INVALID_HELLO);
    let many = (0..=MAX_LABELS)
        .map(|i| (format!("k{i}"), "v".to_string()))
        .collect();
    expect(
        check(|h| h.labels = many),
        "labels",
        Rule::TooMany,
        refusal::INVALID_HELLO,
    );
}

/// The held list: more than the cap, a bad run id and a run id twice are refused; the cap
/// passes. It catches a hello that lists 100k runs, a path as a run id, and one run held
/// twice (which would double-count it).
#[test]
fn held_runs_are_bounded_and_distinct() {
    let err = check(|h| h.held = (0..=MAX_HELD).map(|i| held(&format!("r{i}"))).collect());
    expect(err, "held", Rule::TooMany, refusal::INVALID_HELLO);
    for id in [
        "",
        "../../etc/cron.d/x",
        "a\0",
        long(MAX_ID_LEN + 1).as_str(),
    ] {
        let err = check(|h| h.held = vec![held(id)]);
        assert_eq!(
            (err.field(), err.code()),
            ("held.run_id", refusal::INVALID_HELLO)
        );
    }
    let err = check(|h| h.held = vec![held("a"), held("b"), held("a")]);
    expect(err, "held", Rule::Duplicate, refusal::INVALID_HELLO);
    let mut ok = hello();
    ok.held = (0..MAX_HELD).map(|i| held(&format!("r{i}"))).collect();
    assert_eq!(ok.validate(), Ok(()));
}

/// The provider identity: a provider that is not a provider id and an instance id that is
/// not an id are refused; a good one passes. It catches an instance id used as a lookup
/// key without bounds.
#[test]
fn instance_identity_is_bounded() {
    let identity = |provider: &str, instance: &str| {
        Some(InstanceIdentity {
            provider: provider.into(),
            instance_id: instance.into(),
        })
    };
    for provider in ["", "AWS", "a/b"] {
        let err = check(|h| h.instance = identity(provider, "i-0abc"));
        assert_eq!(err.field(), "instance.provider", "{provider:?}");
    }
    for instance in ["", "i 0", "../i", long(MAX_ID_LEN + 1).as_str()] {
        let err = check(|h| h.instance = identity("aws", instance));
        assert_eq!(err.field(), "instance.instance_id", "{instance:?}");
    }
    let mut ok = hello();
    ok.instance = identity("aws", "i-0abc123");
    assert_eq!(ok.validate(), Ok(()));
}

/// An identity checked on its own gives the verdict a hello carrying it would get: the
/// same field names and the same pass. It catches a standalone check that is looser or
/// stricter than the hello's, which would let a server bind a token to an identity no
/// agent could send.
#[test]
fn an_instance_identity_validates_alone_as_in_a_hello() {
    let identity = |provider: &str, instance: &str| InstanceIdentity {
        provider: provider.into(),
        instance_id: instance.into(),
    };
    assert_eq!(identity("aws", "i-0abc123").validate(), Ok(()));
    assert_eq!(
        identity("AWS", "i-0abc").validate().unwrap_err().field(),
        "instance.provider"
    );
    assert_eq!(
        identity("aws", "i 0").validate().unwrap_err().field(),
        "instance.instance_id"
    );
}

/// A protocol range that is empty or starts at zero is refused as a bad hello; one with
/// no major in common with this crate is refused with the protocol code. It catches an
/// agent of an unknown future being admitted and a malformed range being trusted.
#[test]
fn protocol_range_is_checked() {
    for range in [
        ProtocolRange { min: 0, max: 1 },
        ProtocolRange { min: 3, max: 2 },
    ] {
        let err = check(|h| h.protocol = range);
        expect(err, "protocol", Rule::OutOfRange, refusal::INVALID_HELLO);
    }
    let err = check(|h| h.protocol = ProtocolRange { min: 2, max: 3 });
    expect(
        err,
        "protocol",
        Rule::UnsupportedProtocol,
        refusal::UNSUPPORTED_PROTOCOL,
    );
    let mut ok = hello();
    ok.protocol = ProtocolRange { min: 1, max: 9 };
    assert_eq!(ok.validate(), Ok(()));
    assert_eq!(ok.negotiate(), Ok(1));
}

/// The accelerator the fixture hello (one GPU of class `nvidia-a100`, 64 GiB) states about
/// itself, as the agent would send it.
fn stated() -> WireAccelerator {
    WireAccelerator {
        vendor: "nvidia".into(),
        model: "nvidia-a100".into(),
        variant_spelling: None,
        count: 1,
        memory_per_device_bytes: Some(1 << 36),
    }
}

/// Asserts that a `shape.accelerator` that agrees with the class, the count and the
/// devices is accepted, memory included (the least of the devices, and absent when none is
/// listed), and that one whose model, variant, count or vendor disagrees, or that has no `gpu_class` to
/// derive from, is refused naming `shape.accelerator`; and that an over-long or
/// bad-character string is refused at its own field. It catches a server that believes a
/// host's claim of another model or more devices than its shape says, and a hostile
/// string that reaches the registry.
#[test]
fn a_hello_whose_accelerator_disagrees_with_its_devices_is_refused() {
    let mut ok = hello();
    ok.shape.accelerator = Some(stated());
    assert_eq!(ok.validate(), Ok(()));
    ok.gpu_devices.clear();
    ok.shape.accelerator = Some(WireAccelerator {
        memory_per_device_bytes: None,
        ..stated()
    });
    assert_eq!(ok.validate(), Ok(()));
    let mut two = hello();
    two.gpu_devices[0].memory_bytes = 1 << 37;
    two.shape.gpus = 2;
    two.gpu_devices.push(GpuDevice {
        index: 1,
        uuid: "GPU-2".into(),
        model: "A100".into(),
        memory_bytes: 1 << 36,
    });
    two.shape.accelerator = Some(WireAccelerator {
        count: 2,
        ..stated()
    });
    assert_eq!(two.validate(), Ok(()), "the least of the devices is stated");
    two.shape.accelerator = Some(WireAccelerator {
        count: 2,
        memory_per_device_bytes: Some(1 << 37),
        ..stated()
    });
    assert!(
        two.validate().is_err(),
        "the largest device is not the least"
    );

    let lies = [
        WireAccelerator {
            model: "nvidia-h100".into(),
            ..stated()
        },
        WireAccelerator {
            variant_spelling: Some("nvidia-a100-sxm4-80gb".into()),
            ..stated()
        },
        WireAccelerator {
            count: 2,
            ..stated()
        },
        WireAccelerator {
            memory_per_device_bytes: Some(1),
            ..stated()
        },
        WireAccelerator {
            memory_per_device_bytes: Some(1 << 37),
            ..stated()
        },
        WireAccelerator {
            memory_per_device_bytes: None,
            ..stated()
        },
        WireAccelerator {
            vendor: "apple".into(),
            ..stated()
        },
    ];
    for lie in lies {
        let err = check(move |h| h.shape.accelerator = Some(lie));
        expect(
            err,
            "shape.accelerator",
            Rule::Inconsistent,
            refusal::INVALID_HELLO,
        );
    }

    let err = check(|h| {
        h.gpu_devices.clear();
        h.shape.accelerator = Some(stated());
    });
    expect(
        err,
        "shape.accelerator",
        Rule::Inconsistent,
        refusal::INVALID_HELLO,
    );

    let err = check(|h| {
        h.shape.gpu_class = Some("nvidia-a100-sxm4-80gb".into());
        h.shape.accelerator = Some(stated());
    });
    expect(
        err,
        "shape.accelerator",
        Rule::Inconsistent,
        refusal::INVALID_HELLO,
    );

    let err = check(|h| {
        h.shape.gpus = 0;
        h.shape.gpu_class = None;
        h.gpu_devices.clear();
        h.shape.accelerator = Some(stated());
    });
    expect(
        err,
        "shape.accelerator",
        Rule::Inconsistent,
        refusal::INVALID_HELLO,
    );

    let err = check(|h| {
        h.shape.accelerator = Some(WireAccelerator {
            model: long(MAX_ID_LEN + 1),
            ..stated()
        });
    });
    expect(
        err,
        "shape.accelerator.model",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| {
        h.shape.accelerator = Some(WireAccelerator {
            vendor: "nv idia".into(),
            ..stated()
        });
    });
    expect(
        err,
        "shape.accelerator.vendor",
        Rule::BadChars,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| {
        h.shape.accelerator = Some(WireAccelerator {
            variant_spelling: Some("a\nb".into()),
            ..stated()
        });
    });
    expect(
        err,
        "shape.accelerator.variant_spelling",
        Rule::BadChars,
        refusal::INVALID_HELLO,
    );
    let err = check(|h| {
        h.shape.accelerator = Some(WireAccelerator {
            memory_per_device_bytes: Some(MAX_MEMORY_BYTES + 1),
            ..stated()
        });
    });
    expect(
        err,
        "shape.accelerator.memory_per_device_bytes",
        Rule::OutOfRange,
        refusal::INVALID_HELLO,
    );
}

/// Asserts that a hello is not refused for the alias table the agent was built with: an
/// agent of a newer release that names a class this server's table does not know, with
/// the family its own table gives and the class as the variant spelling, is accepted, and
/// so is an agent of an older release that passes a class this server knows as a variant
/// through as its own model; while a vendor or family that contradicts this server's
/// table for a class it knows is still refused. Catches a cross-check that refuses every
/// agent whose table differs from the server's, which would refuse new data on an older
/// server (or old data on a newer one) after a release adds a row.
#[test]
fn a_hello_from_an_agent_with_another_alias_table_is_accepted() {
    let with = |class: &str, model: &str, variant: Option<&str>, vendor: &str| {
        let mut h = hello();
        h.shape.gpu_class = Some(class.into());
        h.shape.accelerator = Some(WireAccelerator {
            vendor: vendor.into(),
            model: model.into(),
            variant_spelling: variant.map(str::to_owned),
            ..stated()
        });
        h.validate()
    };
    let newer = "nvidia-b300-sxm6-288gb";
    assert_eq!(with(newer, "nvidia-b300", Some(newer), "nvidia"), Ok(()));
    let known = "nvidia-a100-sxm4-80gb";
    assert_eq!(with(known, known, None, "nvidia"), Ok(()));
    assert_eq!(with(known, "nvidia-a100", Some(known), "nvidia"), Ok(()));
    for refused in [
        with(known, "nvidia-h100", Some(known), "nvidia"),
        with(known, "nvidia-a100", Some(known), "amd"),
        with(known, "nvidia-a100", None, "nvidia"),
        with(newer, "nvidia-b300", Some("nvidia-b300-other"), "nvidia"),
    ] {
        expect(
            refused.unwrap_err(),
            "shape.accelerator",
            Rule::Inconsistent,
            refusal::INVALID_HELLO,
        );
    }
}

/// Asserts that a hello of one A100 of 64 GiB that also reports `accelerator.model`,
/// `accelerator.count`, `accelerator.memory-gib` and a form factor of its own as plain
/// capabilities is shaped with the facts its class, count and devices state: the shape's
/// accelerator is the A100, one device of 64 GiB, no reported form factor, and every
/// other capability is kept. Catches a host that reports an H100 or more devices through
/// raw capabilities and is placed as that.
#[test]
fn reported_accelerator_capabilities_are_replaced_by_the_validated_facts() {
    let mut h = hello();
    for (key, value) in [
        ("accelerator.vendor", "amd"),
        ("accelerator.model", "nvidia-h100"),
        ("accelerator.count", "8"),
        ("accelerator.memory-gib", "999"),
        ("accelerator.form-factor", "sxm"),
        ("accelerator.interconnect", "nvlink>=900"),
    ] {
        h.capabilities.insert(key.into(), value.into());
    }
    assert_eq!(h.validate(), Ok(()));
    let shape = h.to_shape().unwrap();
    let found = shape.accelerator().unwrap();
    assert_eq!(found.model.as_str(), "nvidia-a100");
    assert_eq!(found.vendor.as_str(), "nvidia");
    assert_eq!((found.count, found.memory_per_device), (1, Some(1 << 36)));
    assert_eq!((found.form_factor, found.interconnect), (None, None));
    let mut wanted = map(&[("os", "macos")]);
    wanted.insert("accelerator.vendor".into(), "nvidia".into());
    wanted.insert("accelerator.model".into(), "nvidia-a100".into());
    wanted.insert("accelerator.count".into(), "1".into());
    wanted.insert("accelerator.memory-gib".into(), "64".into());
    assert_eq!(shape.capabilities(), &wanted);
}
