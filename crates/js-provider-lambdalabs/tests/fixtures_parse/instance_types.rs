//! The instance-type listing: every field, the catalog the provider makes of it, and its
//! failures.

use crate::*;

// ---------------------------------------------------------------- instance types

fn type_offer(
    (name, description, gpu_description): (&str, &str, &str),
    (price_cents_per_hour, vcpus, memory_gib, storage_gib, gpus): (u64, u32, u32, u64, u32),
    architecture: Architecture,
    regions: Vec<Region>,
) -> InstanceTypeOffer {
    InstanceTypeOffer {
        name: name.to_owned(),
        description: description.to_owned(),
        gpu_description: gpu_description.to_owned(),
        price_cents_per_hour,
        vcpus,
        memory_gib,
        storage_gib,
        gpus,
        architecture,
        regions,
    }
}

#[test]
fn instance_types_come_back_with_every_field_and_ordered_by_name() {
    let server = serving(TYPES, "instance_types.json");
    let offers = connect(&server).instance_types().unwrap();
    let h100 = (
        "gpu_1x_h100_sxm5",
        "1x H100 (80 GB SXM5)",
        "H100 (80 GB SXM5)",
    );
    let expected = vec![
        type_offer(
            ("gpu_1x_arm_sample", "1x Arm Sample GPU", "Arm Sample GPU"),
            (149, 64, 400, 4000, 1),
            Architecture::Arm64,
            vec![region("region-a", "Place A")],
        ),
        type_offer(
            h100,
            (75, 30, 200, 500, 1),
            Architecture::X86_64,
            vec![region("region-b", "Place B"), region("region-a", "Place A")],
        ),
        type_offer(
            (
                "gpu_8x_h100_sxm5",
                "8x H100 (80 GB SXM5)",
                "H100 (80 GB SXM5)",
            ),
            (1200, 128, 1800, 6000, 8),
            Architecture::X86_64,
            Vec::new(),
        ),
        // No description, no regions key, and a control character in the architecture.
        type_offer(
            ("gpu_odd_arch", "", ""),
            (10, 4, 16, 100, 1),
            Architecture::Other("riscv".to_owned()),
            Vec::new(),
        ),
    ];
    assert_eq!(offers, expected);
    assert!(offers[0].has_capacity() && offers[1].has_capacity());
    assert!(!offers[2].has_capacity() && !offers[3].has_capacity());
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, "/api/v1/instance-types");
}

#[test]
fn the_documented_instance_types_come_back_with_every_field() {
    let server = serving(TYPES, "instance_types_documented.json");
    let offers = connect(&server).instance_types().unwrap();
    let california = || region("us-west-1", "California, USA");
    let virginia = || region("us-east-1", "Virginia, USA");
    let expected = vec![
        type_offer(
            ("gpu_1x_a10", "1x A10 (24 GB PCIe)", "A10 (24 GB PCIe)"),
            (86, 30, 200, 1400, 1),
            Architecture::X86_64,
            vec![california(), virginia()],
        ),
        type_offer(
            ("gpu_1x_gh200", "1x GH200 (96 GB)", "GH200 (96 GB)"),
            (149, 64, 432, 4096, 1),
            Architecture::Arm64,
            vec![virginia()],
        ),
        type_offer(
            (
                "gpu_1x_h100_pcie",
                "1x H100 (80 GB PCIe)",
                "H100 (80 GB PCIe)",
            ),
            (249, 26, 200, 1024, 1),
            Architecture::X86_64,
            Vec::new(),
        ),
        type_offer(
            (
                "gpu_8x_a100_80gb_sxm4",
                "8x A100 (80 GB SXM4)",
                "A100 (80 GB SXM4)",
            ),
            (1432, 240, 1800, 20000, 8),
            Architecture::X86_64,
            vec![california()],
        ),
    ];
    assert_eq!(offers, expected);
    assert!(offers[0].available_in("us-east-1") && offers[0].available_in("us-west-1"));
    assert!(!offers[1].available_in("us-west-1"));
}

/// The provider's catalog, run on a runtime of its own.
fn catalog(provider: &LambdaProvider) -> Result<Vec<Offer>, ProviderError> {
    tokio::runtime::Builder::new_multi_thread()
        .build()
        .unwrap()
        .block_on(provider.catalog())
}

#[test]
fn the_catalog_converts_vcpus_gib_and_cents_into_the_offers_units() {
    let server = serving(TYPES, "instance_types_documented.json");
    // The allow list names a type the catalog table has no row for: not offered.
    let table = rows(&[
        "gpu_1x_a10",
        "gpu_1x_gh200",
        "gpu_1x_h100_pcie",
        "gpu_8x_a100_80gb_sxm4",
    ]);
    let offers = catalog(&provider(&server, &table, 2)).unwrap();
    let shown: Vec<(&str, &str)> = offers
        .iter()
        .map(|o| (o.id.as_str(), o.zone.as_deref().unwrap()))
        .collect();
    // One offer per region with capacity now, regions in name order; a type with no
    // region (gpu_1x_h100_pcie) and a type the table lacks (gpu_1x_gh200) have none.
    assert_eq!(
        shown,
        [
            ("gpu_1x_a10", "us-east-1"),
            ("gpu_1x_a10", "us-west-1"),
            ("gpu_8x_a100_80gb_sxm4", "us-west-1"),
        ]
    );
    for offer in &offers[..2] {
        assert_eq!(offer.cpu_millis, 30_000);
        assert_eq!(offer.memory_bytes, 200 << 30);
        assert_eq!(offer.disk_bytes, 1400 << 30);
        assert_eq!(offer.price_cents_per_hour, Some(86));
        let accelerator = offer.accelerator.as_ref().unwrap();
        assert_eq!(accelerator.count, 1);
        assert_eq!(accelerator.memory_per_device, Some(24 << 30));
        assert_eq!(accelerator.form_factor, Some(FormFactor::Pcie));
    }
    let board = &offers[2];
    assert_eq!(board.cpu_millis, 240_000);
    assert_eq!(board.memory_bytes, 1800 << 30);
    assert_eq!(board.disk_bytes, 20_000 << 30);
    assert_eq!(board.price_cents_per_hour, Some(1432));
    let accelerator = board.accelerator.as_ref().unwrap();
    assert_eq!(accelerator.count, 8);
    assert_eq!(accelerator.memory_per_device, Some(80 << 30));
    assert_eq!(accelerator.form_factor, Some(FormFactor::Sxm));
    for offer in &offers {
        assert_eq!(offer.price_unit, PriceUnit::UsCents);
        assert_eq!(
            (offer.os.as_str(), offer.arch.as_str()),
            ("linux", "x86_64")
        );
        assert_eq!(offer.exec, ExecMode::Container);
        assert_eq!(offer.purchase, Some(Purchase::OnDemand));
        assert!(offer.available);
        assert!(offer.isa_level.is_none() && offer.quota.is_none() && offer.block.is_none());
    }
}

#[test]
fn the_catalog_offers_a_type_in_each_region_that_has_capacity_and_none_otherwise() {
    let server = serving(TYPES, "instance_types.json");
    let table = rows(&["gpu_1x_h100_sxm5", "gpu_8x_h100_sxm5", "gpu_1x_arm_sample"]);
    let offers = catalog(&provider(&server, &table, 2)).unwrap();
    let shown: Vec<(&str, &str)> = offers
        .iter()
        .map(|o| (o.id.as_str(), o.zone.as_deref().unwrap()))
        .collect();
    // gpu_8x_h100_sxm5 lists no region with capacity; gpu_1x_arm_sample has no row in the
    // catalog table.
    assert_eq!(
        shown,
        [
            ("gpu_1x_h100_sxm5", "region-a"),
            ("gpu_1x_h100_sxm5", "region-b")
        ]
    );
    for offer in &offers {
        assert_eq!(offer.cpu_millis, 30_000);
        assert_eq!(offer.memory_bytes, 200 << 30);
        assert_eq!(offer.disk_bytes, 500 << 30);
        assert_eq!(offer.price_cents_per_hour, Some(75));
        assert_eq!(offer.price_unit, PriceUnit::UsCents);
    }
}

#[test]
fn a_catalog_that_cannot_be_read_is_an_error_not_an_empty_catalog() {
    let cases = [
        (
            401,
            "error_invalid_api_key.json",
            "the API answered 401 global/invalid-api-key",
        ),
        (
            500,
            "error_internal.json",
            "the API answered 500 global/internal-error",
        ),
        (
            429,
            "error_rate_limited.json",
            "the API is rate limiting this client",
        ),
    ];
    for (status, file, message) in cases {
        let server = Server::plain();
        server.on(TYPES, Reply::json(status, &fixture(file)));
        let err = catalog(&provider(&server, &rows(&["gpu_1x_a10"]), 2)).unwrap_err();
        assert_eq!(err.to_string(), message, "{file}");
    }
}
