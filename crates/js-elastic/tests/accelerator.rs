//! `Offer::fit` on the accelerator: offers built the one way an offer's accelerator is
//! built (`js_types::accelerator_of`), against needs as a request states them.

use js_elastic::{Demand, Dimension, Offer, PriceUnit};
use js_types::{
    Accelerator, AcceleratorNeed, ExecMode, FormFactor, Interconnect, Model, Resources, Vendor,
    accelerator_of,
};

const GIB: u64 = 1 << 30;

fn model(name: &str) -> Model {
    Model::parse(name).unwrap()
}

/// A Linux offer with ample CPU, memory and disk and these accelerators.
fn offer(accelerator: Option<Accelerator>) -> Offer {
    Offer {
        id: "gpu_8x".to_owned(),
        zone: Some("zone-a".to_owned()),
        cpu_millis: 64_000,
        memory_bytes: 512 * GIB,
        disk_bytes: 1024 * GIB,
        accelerator,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(100),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// An offer of `count` devices spelt `spelling`, with `measured` memory per device.
fn offer_of(spelling: &str, count: u32, measured: Option<u64>) -> Offer {
    offer(Some(accelerator_of(&model(spelling), count, measured)))
}

/// A demand for one core and `need`, booking no GPU (the booked count is the request
/// path's business; fit judges the need).
fn demand(need: Option<AcceleratorNeed>) -> Demand {
    Demand {
        resources: Resources::new(1000, GIB, 0, None).unwrap(),
        disk_bytes: None,
        os: None,
        arch: None,
        isa_level: None,
        exec: None,
        accelerator: need,
        network: Default::default(),
        host_ports: Vec::new(),
        layered_rootfs: false,
    }
}

/// A need for `count` devices of the model `name`.
fn asking(name: &str, count: u32) -> Demand {
    demand(Some(AcceleratorNeed {
        model: Some(model(name)),
        count,
        ..AcceleratorNeed::default()
    }))
}

/// The three A100 offers: 80 GB SXM4 and 40 GB SXM4 by their agent spellings, and a 40 GB
/// PCIe one built by hand, as a provider states it when it gives the family and the facts
/// without a variant spelling.
fn a100s() -> [Offer; 3] {
    let pcie = Accelerator {
        vendor: Vendor::parse("nvidia").unwrap(),
        model: model("nvidia-a100"),
        variant: None,
        count: 8,
        memory_per_device: Some(40 * GIB),
        form_factor: Some(FormFactor::Pcie),
        interconnect: None,
    };
    [
        offer_of("nvidia-a100-sxm4-80gb", 8, None),
        offer_of("nvidia-a100-sxm4-40gb", 8, None),
        offer(Some(pcie)),
    ]
}

/// Asserts that offers for the 80 GB SXM4, the 40 GB SXM4 and the 40 GB PCIe A100 all
/// fit a request for the family `nvidia-a100`. Catches a fit that compares the lower-case
/// spelling for equality, or a table row that lacks its family link.
#[test]
fn a_family_request_matches_every_variant() {
    let family = asking("nvidia-a100", 8);
    for offer in a100s() {
        assert_eq!(offer.fit(&family), Ok(()), "{:?}", offer.accelerator);
    }
}

/// Asserts that a request for `nvidia-a100-sxm4-80gb` fits that offer and fails the 40 GB
/// SXM4 and the 40 GB PCIe offers by memory (the variant's 80 GiB is pinned), and that a
/// PCIe offer with 80 GB fails it by form factor. Catches a full spelling widened to its
/// family.
#[test]
fn a_full_spelling_matches_only_that_variant() {
    let full = asking("nvidia-a100-sxm4-80gb", 8);
    let [sxm80, sxm40, pcie40] = a100s();
    assert_eq!(sxm80.fit(&full), Ok(()));
    assert_eq!(sxm40.fit(&full), Err(Dimension::AcceleratorMemory));
    assert_eq!(pcie40.fit(&full), Err(Dimension::AcceleratorMemory));
    let mut pcie80 = pcie40.clone();
    if let Some(accel) = pcie80.accelerator.as_mut() {
        accel.memory_per_device = Some(80 * GIB);
    }
    assert_eq!(pcie80.fit(&full), Err(Dimension::AcceleratorFormFactor));
}

/// Asserts that a request for `nvidia-gb300` (not in the table) fits an offer of the
/// identical unknown name and fails `nvidia-a100` by model; that an unknown name never
/// fits a known family and a known family never fits an unknown name that starts like it.
/// Catches unknown names dropped, rewritten or matched by prefix.
#[test]
fn an_unknown_name_matches_only_an_identical_one() {
    let gb300 = asking("nvidia-gb300", 4);
    assert_eq!(offer_of("nvidia-gb300", 4, None).fit(&gb300), Ok(()));
    assert_eq!(
        offer_of("nvidia-a100-sxm4-80gb", 4, None).fit(&gb300),
        Err(Dimension::AcceleratorModel)
    );
    assert_eq!(
        offer_of("nvidia-gb300-x", 4, None).fit(&gb300),
        Err(Dimension::AcceleratorModel)
    );
    assert_eq!(
        offer_of("nvidia-a100x", 4, None).fit(&asking("nvidia-a100", 4)),
        Err(Dimension::AcceleratorModel)
    );
}

/// Asserts that count and memory per device are at least, at the exact boundary: count 4
/// fits offers of 4 and 8 and fails 2; 80 GiB fits offers of 80 and 141 GiB and fails 40;
/// an offer whose memory is not known fails a request that fixes memory. Catches `==`, a
/// reversed `>=`, or an off-by-one at the boundary.
#[test]
fn count_and_memory_are_at_least() {
    let four = asking("nvidia-gb300", 4);
    assert_eq!(offer_of("nvidia-gb300", 4, None).fit(&four), Ok(()));
    assert_eq!(offer_of("nvidia-gb300", 8, None).fit(&four), Ok(()));
    assert_eq!(
        offer_of("nvidia-gb300", 3, None).fit(&four),
        Err(Dimension::AcceleratorCount)
    );
    assert_eq!(
        offer_of("nvidia-gb300", 2, None).fit(&four),
        Err(Dimension::AcceleratorCount)
    );
    let eighty = demand(Some(AcceleratorNeed {
        memory_per_device: Some(80 * GIB),
        ..AcceleratorNeed::default()
    }));
    for fits in [80 * GIB, 141 * GIB] {
        assert_eq!(offer_of("nvidia-gb300", 1, Some(fits)).fit(&eighty), Ok(()));
    }
    for short in [40 * GIB, 80 * GIB - 1] {
        assert_eq!(
            offer_of("nvidia-gb300", 1, Some(short)).fit(&eighty),
            Err(Dimension::AcceleratorMemory)
        );
    }
    assert_eq!(
        offer_of("nvidia-gb300", 1, None).fit(&eighty),
        Err(Dimension::AcceleratorMemory)
    );
}

/// Asserts the memory rule of the one builder on an offer: an H100 80GB HBM3 whose devices
/// measure 81559 MiB (below 80 GiB) fits `memory >= 80 GiB` by its variant's nominal
/// memory, while an unknown name is held to the bytes measured. Catches a measured
/// compare that refuses the device a request names, or nominal memory invented for a name
/// the table does not know.
#[test]
fn a_known_variant_is_compared_by_its_nominal_memory() {
    let measured = 81_559 << 20;
    let eighty = demand(Some(AcceleratorNeed {
        memory_per_device: Some(80 * GIB),
        ..AcceleratorNeed::default()
    }));
    assert_eq!(
        offer_of("nvidia-h100-80gb-hbm3", 8, Some(measured)).fit(&eighty),
        Ok(())
    );
    assert_eq!(
        offer_of("nvidia-gb300", 8, Some(measured)).fit(&eighty),
        Err(Dimension::AcceleratorMemory)
    );
}

/// Asserts that an omitted field matches anything: a request with only a count fits any
/// model, vendor and memory; one with only `vendor=nvidia` fits any NVIDIA model and fails
/// an Apple offer; a request with no accelerator fits a CPU-only offer and a GPU offer.
/// Catches an omitted field read as "must be absent" or as a default value.
#[test]
fn an_omitted_field_matches_anything() {
    let count_only = demand(Some(AcceleratorNeed {
        count: 2,
        ..AcceleratorNeed::default()
    }));
    for spelling in [
        "nvidia-a100-sxm4-80gb",
        "nvidia-gb300",
        "apple-m",
        "amd-mi300x",
    ] {
        assert_eq!(
            offer_of(spelling, 2, None).fit(&count_only),
            Ok(()),
            "{spelling}"
        );
    }
    let nvidia = demand(Some(AcceleratorNeed {
        vendor: Some(Vendor::parse("nvidia").unwrap()),
        ..AcceleratorNeed::default()
    }));
    assert_eq!(
        offer_of("nvidia-h100-80gb-hbm3", 1, None).fit(&nvidia),
        Ok(())
    );
    assert_eq!(offer_of("nvidia-gb300", 1, None).fit(&nvidia), Ok(()));
    assert_eq!(
        offer_of("apple-m", 1, None).fit(&nvidia),
        Err(Dimension::AcceleratorVendor)
    );
    let none = demand(None);
    assert_eq!(offer(None).fit(&none), Ok(()));
    assert_eq!(
        offer_of("nvidia-a100-sxm4-80gb", 8, None).fit(&none),
        Ok(())
    );
}

/// Asserts that a count-only need, which the request path refuses but `Offer::fit`
/// accepts, is judged on the count alone: met by an offer with enough devices of any
/// model, missed by count on one with fewer and on a CPU-only offer. Catches a fit that
/// requires a model whenever a count is asked.
#[test]
fn a_count_only_need_is_judged_on_the_count() {
    let count_only = demand(Some(AcceleratorNeed {
        count: 4,
        ..AcceleratorNeed::default()
    }));
    assert_eq!(offer_of("nvidia-gb300", 4, None).fit(&count_only), Ok(()));
    assert_eq!(
        offer_of("nvidia-gb300", 3, None).fit(&count_only),
        Err(Dimension::AcceleratorCount)
    );
    assert_eq!(
        offer(None).fit(&count_only),
        Err(Dimension::AcceleratorCount)
    );
}

/// Asserts the order of the accelerator dimensions, which is reported to clients: an offer
/// wrong in model, count, memory and form factor fails by model; right in model, by
/// count; right in count too, by memory; right in memory too, by form factor; and a vendor
/// miss comes before them all. Catches a changed order.
#[test]
fn fit_checks_model_then_count_then_memory_then_form_factor() {
    let want = demand(Some(AcceleratorNeed {
        vendor: None,
        model: Some(model("nvidia-a100")),
        count: 8,
        memory_per_device: Some(80 * GIB),
        interconnect: None,
        form_factor: Some(FormFactor::Sxm),
    }));
    let with = |name: &str, count, memory, form_factor| {
        offer(Some(Accelerator {
            vendor: Vendor::parse("nvidia").unwrap(),
            model: model(name),
            variant: None,
            count,
            memory_per_device: Some(memory),
            form_factor: Some(form_factor),
            interconnect: None,
        }))
    };
    let pcie = FormFactor::Pcie;
    let sxm = FormFactor::Sxm;
    assert_eq!(
        with("nvidia-h100", 1, 40 * GIB, pcie).fit(&want),
        Err(Dimension::AcceleratorModel)
    );
    assert_eq!(
        with("nvidia-a100", 1, 40 * GIB, pcie).fit(&want),
        Err(Dimension::AcceleratorCount)
    );
    assert_eq!(
        with("nvidia-a100", 8, 40 * GIB, pcie).fit(&want),
        Err(Dimension::AcceleratorMemory)
    );
    assert_eq!(
        with("nvidia-a100", 8, 80 * GIB, pcie).fit(&want),
        Err(Dimension::AcceleratorFormFactor)
    );
    assert_eq!(with("nvidia-a100", 8, 80 * GIB, sxm).fit(&want), Ok(()));
    let mut by_vendor = want.clone();
    if let Some(need) = by_vendor.accelerator.as_mut() {
        need.vendor = Some(Vendor::parse("amd").unwrap());
    }
    assert_eq!(
        with("nvidia-h100", 1, 40 * GIB, pcie).fit(&by_vendor),
        Err(Dimension::AcceleratorVendor)
    );
}

/// Asserts the GPUs a demand books are judged beside its need: more booked than the offer
/// has fails by count even when the need asks for fewer, and a booked class that is not a
/// model name (no need is derived from it) fits no offer, by model. Catches a fit that
/// books more devices than the machine has.
#[test]
fn the_booked_gpus_are_held_to_the_offers_count() {
    let mut booked = asking("nvidia-gb300", 1);
    booked.resources = Resources::new(1000, GIB, 8, Some("nvidia-gb300".into())).unwrap();
    assert_eq!(offer_of("nvidia-gb300", 8, None).fit(&booked), Ok(()));
    assert_eq!(
        offer_of("nvidia-gb300", 4, None).fit(&booked),
        Err(Dimension::AcceleratorCount)
    );
    let mut odd = demand(None);
    odd.resources = Resources::new(1000, GIB, 1, Some("A100_X".into())).unwrap();
    assert_eq!(
        offer_of("nvidia-gb300", 8, None).fit(&odd),
        Err(Dimension::AcceleratorModel)
    );
}

/// Asserts the interconnect on an offer: the same kind with a domain of exactly the size
/// asked or larger fits, a smaller domain, none stated and another kind each fail by the
/// interconnect dimension, and an offer wrong in both interconnect and form factor is
/// named by the interconnect, which is checked first. Catches an interconnect miss mapped
/// to another dimension, or form factor checked before it.
#[test]
fn an_interconnect_is_at_least_and_named_before_the_form_factor() {
    let want = demand(Some(AcceleratorNeed {
        interconnect: Some(Interconnect::new("nvlink-domain", 72).unwrap()),
        form_factor: Some(FormFactor::Sxm),
        ..AcceleratorNeed::default()
    }));
    let with = |link: Option<(&str, u32)>, form_factor| {
        offer(Some(Accelerator {
            vendor: Vendor::parse("nvidia").unwrap(),
            model: model("nvidia-gb300"),
            variant: None,
            count: 72,
            memory_per_device: None,
            form_factor: Some(form_factor),
            interconnect: link.map(|(kind, size)| Interconnect::new(kind, size).unwrap()),
        }))
    };
    let sxm = FormFactor::Sxm;
    assert_eq!(with(Some(("nvlink-domain", 72)), sxm).fit(&want), Ok(()));
    assert_eq!(with(Some(("nvlink-domain", 144)), sxm).fit(&want), Ok(()));
    for link in [Some(("nvlink-domain", 71)), None, Some(("infiniband", 72))] {
        assert_eq!(
            with(link, sxm).fit(&want),
            Err(Dimension::AcceleratorInterconnect),
            "{link:?}"
        );
    }
    assert_eq!(
        with(Some(("nvlink-domain", 8)), FormFactor::Pcie).fit(&want),
        Err(Dimension::AcceleratorInterconnect)
    );
    assert_eq!(
        with(Some(("nvlink-domain", 72)), FormFactor::Pcie).fit(&want),
        Err(Dimension::AcceleratorFormFactor)
    );
}
