//! A job's accelerator need as `Requirements` reads it from a request: the
//! `accelerator.*` keys over the GPUs the request books, the refusals, and the lenient
//! read.

use std::collections::BTreeMap;
use std::time::Duration;

use js_types::{
    AccelError, AcceleratorNeed, Capabilities, ClassId, Model, Request, Requirements,
    RequirementsError, Resources,
};

/// A request for one `nvidia-a10` naming these capabilities.
fn request(caps: &[(&str, &str)]) -> Request {
    let capabilities: Capabilities = caps
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Request {
        class: ClassId::new("whole-node").unwrap(),
        resources: Resources::new(4000, 8, 1, Some("nvidia-a10".into())).unwrap(),
        capabilities,
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    }
}

/// The need the test request's booked GPU states: one `nvidia-a10`.
fn a10() -> Option<AcceleratorNeed> {
    Some(AcceleratorNeed {
        model: Some(Model::parse("nvidia-a10").unwrap()),
        count: 1,
        ..AcceleratorNeed::default()
    })
}

/// A request booking `gpus` GPUs of `class` (none when `class` is `None`) and naming
/// these capabilities.
fn booking(gpus: u64, class: Option<&str>, caps: &[(&str, &str)]) -> Request {
    let mut asked = request(caps);
    asked.resources = Resources::new(4000, 8, gpus, class.map(str::to_owned)).unwrap();
    asked
}

/// Asserts that the `accelerator.*` keys are read as the need (memory in GiB, the model
/// as spelt, every field), that a vendor-only need books nothing and is valid, and that
/// a request with no keys reads its booked GPU count and class as the need. Catches a
/// requirement that drops a key, or one that loses today's GPU class when no key is
/// named.
#[test]
fn the_accelerator_comes_from_its_keys_or_the_booked_gpus() {
    use js_types::{FormFactor, Interconnect, Vendor};
    let full = Requirements::try_from_request(&booking(
        4,
        Some("nvidia-a100-sxm4-80gb"),
        &[
            ("accelerator.vendor", "nvidia"),
            ("accelerator.model", "nvidia-a100-sxm4-80gb"),
            ("accelerator.count", "4"),
            ("accelerator.memory-gib", "80"),
            ("accelerator.interconnect", "nvlink-domain>=72"),
            ("accelerator.form-factor", "sxm"),
        ],
    ))
    .unwrap();
    let want = AcceleratorNeed {
        vendor: Some(Vendor::parse("nvidia").unwrap()),
        model: Some(Model::parse("nvidia-a100-sxm4-80gb").unwrap()),
        count: 4,
        memory_per_device: Some(80 << 30),
        interconnect: Some(Interconnect::new("nvlink-domain", 72).unwrap()),
        form_factor: Some(FormFactor::Sxm),
    };
    assert_eq!(full.accelerator, Some(want));
    let cpu = |caps: &[(&str, &str)]| Requirements::try_from_request(&booking(0, None, caps));
    let vendor_only = cpu(&[("accelerator.vendor", "nvidia")]).unwrap();
    let want = AcceleratorNeed {
        vendor: Some(Vendor::parse("nvidia").unwrap()),
        ..AcceleratorNeed::default()
    };
    assert_eq!(vendor_only.accelerator, Some(want));
    let read = |caps: &[(&str, &str)]| Requirements::try_from_request(&request(caps));
    assert_eq!(read(&[]).unwrap().accelerator, a10());
    let mut cpu_only = request(&[]);
    cpu_only.resources = Resources::new(1000, 1, 0, None).unwrap();
    assert_eq!(Requirements::from_request(&cpu_only).accelerator, None);
    let mut odd_class = request(&[]);
    odd_class.resources = Resources::new(1000, 1, 2, Some("A100_X".into())).unwrap();
    assert_eq!(
        Requirements::from_request(&odd_class).accelerator,
        None,
        "a class that is not a model name is matched by the class alone"
    );
}

/// Asserts that keys a request names beside a booked pair narrow the booked need and
/// never drop it: with a booked full spelling, a vendor alone, a memory alone, a count
/// alone (equal to the booking) and a count of zero each keep the booked model (so its
/// variant pin) and count, and add what they name. Catches a vendor key that turns a
/// pinned request into one for any model, or a count key refused for lacking a model the
/// booking names.
#[test]
fn keys_beside_a_booking_narrow_its_need_and_keep_its_model() {
    use js_types::Vendor;
    let spelling = "nvidia-a100-sxm4-80gb";
    let need = |caps: &[(&str, &str)]| {
        Requirements::try_from_request(&booking(2, Some(spelling), caps))
            .unwrap()
            .accelerator
            .unwrap()
    };
    let booked = AcceleratorNeed {
        model: Some(Model::parse(spelling).unwrap()),
        count: 2,
        ..AcceleratorNeed::default()
    };
    assert_eq!(
        need(&[("accelerator.vendor", "nvidia")]),
        AcceleratorNeed {
            vendor: Some(Vendor::parse("nvidia").unwrap()),
            ..booked.clone()
        }
    );
    assert_eq!(
        need(&[("accelerator.memory-gib", "80")]),
        AcceleratorNeed {
            memory_per_device: Some(80 << 30),
            ..booked.clone()
        }
    );
    assert_eq!(need(&[("accelerator.count", "2")]), booked);
    assert_eq!(need(&[("accelerator.count", "0")]), booked);
    assert_eq!(need(&[]), booked);
}

/// Asserts that an `accelerator.model` key beside a booked class keeps the narrower of
/// the two: a booked full spelling stays pinned beside a key naming its family (so a
/// validated request for the 80 GiB A100 does not become one for any A100, and does not
/// fit a host of the 40 GiB one), a key naming a variant pins a booked family, and a key
/// naming another family or another variant is refused by `Request::validate` and
/// `try_from_request`. Catches a key that widens the booked model.
#[test]
fn a_model_key_never_widens_the_booked_model() {
    use js_types::{RequestError, Shape};
    let (a80, a40) = ("nvidia-a100-sxm4-80gb", "nvidia-a100-sxm4-40gb");
    let family = [("accelerator.model", "nvidia-a100")];
    let pinned = booking(2, Some(a80), &family);
    assert!(pinned.validate().is_ok());
    let need = Requirements::try_from_request(&pinned)
        .unwrap()
        .accelerator
        .unwrap();
    assert_eq!(need.model.as_ref().map(Model::as_str), Some(a80));
    let host = |class: &str| {
        let resources = Resources::new(64_000, 1 << 40, 8, Some(class.into())).unwrap();
        Shape::new(resources, Capabilities::new())
    };
    assert!(host(a80).satisfies(&pinned.resources, &pinned.capabilities));
    assert!(
        !host(a40).satisfies(&pinned.resources, &pinned.capabilities),
        "the family key does not widen the booked 80 GiB variant"
    );
    let narrowed = booking(2, Some("nvidia-a100"), &[("accelerator.model", a80)]);
    let need = Requirements::try_from_request(&narrowed)
        .unwrap()
        .accelerator
        .unwrap();
    assert_eq!(need.model.as_ref().map(Model::as_str), Some(a80));
    assert!(!host(a40).satisfies(&narrowed.resources, &narrowed.capabilities));
    for (booked, key) in [(a80, a40), ("nvidia-a100", "nvidia-h100")] {
        let contradicted = booking(2, Some(booked), &[("accelerator.model", key)]);
        let conflict = AccelError::ModelConflict {
            stated: key.to_owned(),
            booked: booked.to_owned(),
        };
        assert_eq!(
            Requirements::try_from_request(&contradicted),
            Err(RequirementsError::BadAccelerator(conflict.clone()))
        );
        assert_eq!(
            contradicted.validate(),
            Err(RequestError::Requirements(
                RequirementsError::BadAccelerator(conflict.clone())
            ))
        );
        assert!(!host(booked).satisfies(&contradicted.resources, &contradicted.capabilities));
        assert_eq!(
            conflict.to_string(),
            format!("accelerator.model {key:?} contradicts the booked gpu_class {booked:?}")
        );
    }
    let same = booking(2, Some(a80), &[("accelerator.model", a80)]);
    let need = Requirements::try_from_request(&same)
        .unwrap()
        .accelerator
        .unwrap();
    assert_eq!(need.model.as_ref().map(Model::as_str), Some(a80));
}

/// Asserts that a count without a model (neither a key nor a booked class names one),
/// a count above the GPUs the request books, and each key holding a value that is not
/// valid, is refused naming what is wrong, and that the lenient read then falls back to
/// the booked GPU count and class. Catches a count-only need reaching a lease, which
/// would be stored as GPUs of no class, and a need for devices the request does not
/// book, which would let two leases share one device.
#[test]
fn a_count_without_a_model_or_a_bad_key_is_refused() {
    let read = |caps: &[(&str, &str)]| Requirements::try_from_request(&request(caps));
    let count_only = [("accelerator.count", "4")];
    assert_eq!(
        Requirements::try_from_request(&booking(0, None, &count_only)),
        Err(RequirementsError::CountWithoutModel)
    );
    assert_eq!(
        RequirementsError::CountWithoutModel.to_string(),
        "accelerator.model is required when accelerator.count is above zero"
    );
    let unbooked = [
        ("accelerator.model", "nvidia-a100"),
        ("accelerator.count", "4"),
    ];
    assert_eq!(
        Requirements::try_from_request(&booking(0, None, &unbooked)),
        Err(RequirementsError::CountNotBooked { count: 4, gpus: 0 })
    );
    assert_eq!(
        read(&count_only),
        Err(RequirementsError::CountNotBooked { count: 4, gpus: 1 })
    );
    assert_eq!(
        RequirementsError::CountNotBooked { count: 4, gpus: 1 }.to_string(),
        "accelerator.count 4 is more than the 1 gpus the request books"
    );
    assert!(Requirements::try_from_request(&booking(4, Some("nvidia-a100"), &unbooked)).is_ok());
    assert!(read(&[("accelerator.count", "0")]).is_ok());
    let bad = [
        (
            "accelerator.vendor",
            "NVIDIA",
            AccelError::BadVendor("NVIDIA".into()),
        ),
        (
            "accelerator.model",
            "a--b",
            AccelError::BadModel("a--b".into()),
        ),
        (
            "accelerator.count",
            "four",
            AccelError::BadNumber {
                key: "accelerator.count",
                value: "four".into(),
            },
        ),
        (
            "accelerator.memory-gib",
            "-1",
            AccelError::BadNumber {
                key: "accelerator.memory-gib",
                value: "-1".into(),
            },
        ),
        (
            "accelerator.interconnect",
            "nvlink",
            AccelError::BadInterconnect("nvlink".into()),
        ),
        (
            "accelerator.form-factor",
            "oam",
            AccelError::BadFormFactor("oam".into()),
        ),
    ];
    for (key, value, why) in bad {
        assert_eq!(
            read(&[(key, value)]),
            Err(RequirementsError::BadAccelerator(why.clone())),
            "{key}"
        );
        assert_eq!(
            RequirementsError::BadAccelerator(why.clone()).to_string(),
            format!("accelerator: {why}")
        );
        let lenient = Requirements::from_request(&request(&[(key, value)]));
        assert_eq!(lenient.accelerator, a10(), "{key}");
    }
    let lenient = Requirements::from_request(&request(&count_only));
    assert_eq!(lenient.accelerator, a10());
}
