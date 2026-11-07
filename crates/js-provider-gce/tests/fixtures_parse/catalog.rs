//! The catalog side: machine types, accelerator types, region quotas, SKUs and the offers
//! the catalog states from them.

use super::*;

/// Asserts that `machineTypes.aggregatedList` over the two fixture pages is two requests
/// (the second carrying the first page's token), that the rows of both pages are merged,
/// and that each row is read field by field: the name, the zone, the vCPUs, the memory in
/// MiB (`memoryMb`), the built-in accelerators and the stated architecture (an A2 states
/// `X86_64`, a T2A `ARM64`, an N1 none). The scope of a zone nobody asked for is not read.
/// It catches a page token read from the wrong key, a page dropped from the merge, and a
/// unit changed on the way to the record.
#[test]
fn machine_types_in_two_pages_are_merged_and_read_field_by_field() {
    let (client, script) = over(machine_pages());
    let zones = [zone(ZONE_A), zone(ZONE_B)];
    let rows = client.machine_types(&project(), &zones).unwrap();
    let expected = vec![
        info(
            "a2-highgpu-1g",
            ZONE_A,
            12,
            87040,
            &[("nvidia-tesla-a100", 1)],
            Some("X86_64"),
        ),
        info(
            "a2-highgpu-1g",
            ZONE_B,
            12,
            87040,
            &[("nvidia-tesla-a100", 1)],
            Some("X86_64"),
        ),
        info("n1-standard-4", ZONE_A, 4, 15360, &[], None),
        info("n1-standard-4", ZONE_B, 4, 15360, &[], None),
        info("t2a-standard-4", ZONE_B, 4, 16384, &[], Some("ARM64")),
    ];
    assert_eq!(by_name_and_zone(rows), expected);
    let url = format!("{COMPUTE}/aggregated/machineTypes");
    let first = query(&[("returnPartialSuccess", "true"), ("maxResults", "500")]);
    let second = query(&[
        ("returnPartialSuccess", "true"),
        ("maxResults", "500"),
        ("pageToken", "EXAMPLE-PAGE-TOKEN-1"),
    ]);
    assert_eq!(calls(&script), [(url.clone(), first), (url, second)]);
    assert_eq!(script.left(), 0);
}

/// Asserts that a requested zone whose scope holds only a `NO_RESULTS_ON_PAGE` warning
/// has no rows and no error, on every page. It catches a warning scope read as a list or
/// as a failure.
#[test]
fn a_zone_whose_scope_holds_only_a_warning_has_no_machine_types() {
    let (client, script) = over(machine_pages());
    let rows = client.machine_types(&project(), &[zone(ZONE_F)]).unwrap();
    assert!(rows.is_empty());
    assert_eq!(script.left(), 0);
}

/// Asserts that `acceleratorTypes.aggregatedList` reads each type's name, its zone (a URL
/// in this answer) and `maximumCardsPerInstance`, and skips the warning-only scope of a
/// requested zone. It catches a zone read from the wrong form and a limit read as a name.
#[test]
fn accelerator_types_are_read_from_their_zone_urls() {
    let (client, script) = over(vec![answer(fixture!("accelerator_types_aggregated.json"))]);
    let zones = [zone(ZONE_A), zone(ZONE_B), zone(ZONE_F)];
    let rows = client.accelerator_types(&project(), &zones).unwrap();
    let row = |name: &str, zone_name: &str, max: u32| AcceleratorTypeInfo {
        name: AcceleratorTypeName(name.to_owned()),
        zone: zone(zone_name),
        max_per_instance: max,
    };
    assert_eq!(
        rows,
        [
            row("nvidia-tesla-a100", ZONE_A, 16),
            row("nvidia-tesla-t4", ZONE_A, 4),
            row("nvidia-tesla-t4", ZONE_B, 4),
        ]
    );
    assert_eq!(
        calls(&script),
        [(
            format!("{COMPUTE}/aggregated/acceleratorTypes"),
            query(&[("returnPartialSuccess", "true"), ("maxResults", "500")])
        )]
    );
}

/// Asserts that `regions.get` is one request to the region and that every quota is read
/// with its metric, its limit and its usage as whole numbers, though the answer writes
/// them with a fraction of zero (`24.0`). It catches a float left as a float and a usage
/// dropped.
#[test]
fn region_quotas_are_read_as_whole_numbers() {
    let (client, script) = over(vec![answer(fixture!("regions_get_us_central1.json"))]);
    let quotas = client.region_quotas(&project(), &region()).unwrap();
    let quota = |metric: &str, limit: u64, usage: u64| QuotaMetric {
        metric: metric.to_owned(),
        limit,
        usage: Some(usage),
    };
    assert_eq!(
        quotas,
        [
            quota("CPUS", 24, 4),
            quota("PREEMPTIBLE_CPUS", 0, 0),
            quota("DISKS_TOTAL_GB", 4096, 100),
            quota("N2_CPUS", 24, 0),
            quota("NVIDIA_T4_GPUS", 4, 0),
            quota("PREEMPTIBLE_NVIDIA_T4_GPUS", 2, 0),
            quota("NVIDIA_A100_GPUS", 8, 1),
            quota("PREEMPTIBLE_NVIDIA_A100_GPUS", 16, 0),
        ]
    );
    assert_eq!(
        calls(&script),
        [(format!("{COMPUTE}/regions/us-central1"), Vec::new())]
    );
}

fn usd(units: &str, nanos: i32) -> Money {
    Money {
        currency: "USD".to_owned(),
        units: units.to_owned(),
        nanos,
    }
}

fn price(item: PricedItem, usage: UsageType, units: &str, nanos: i32) -> SkuPrice {
    SkuPrice {
        item,
        usage,
        unit_price: usd(units, nanos),
    }
}

fn cores(series: &str) -> PricedItem {
    PricedItem::Cores(SeriesName(series.to_owned()))
}

fn memory(series: &str) -> PricedItem {
    PricedItem::Memory(SeriesName(series.to_owned()))
}

fn gpu(name: &str) -> PricedItem {
    PricedItem::Accelerator(AcceleratorTypeName(name.to_owned()))
}

/// Asserts that `services.skus.list` over the two fixture pages is two requests in US
/// dollars at the largest page size (the second carrying the first page's token), and that
/// of the fifteen SKUs only the twelve the catalog prices are kept, each with its item, its
/// usage type and its unit price as text units and nanos. Left out are the same words
/// offered in another region, a one-year commitment and a storage SKU. A tiered SKU gives
/// the rate that starts at zero, and a rate with no `startUsageAmount` starts at zero. It
/// catches a SKU of another region kept, a commitment read as on-demand, the wrong tier
/// read, and a page lost.
#[test]
fn skus_in_two_pages_are_filtered_and_read_with_their_unit_prices() {
    let (client, script) = over(sku_pages());
    let prices = client.prices(&region()).unwrap();
    let od = UsageType::OnDemand;
    let spot = UsageType::Preemptible;
    assert_eq!(
        prices,
        [
            price(cores("n1"), od, "0", 31_611_000),
            price(memory("n1"), od, "0", 4_237_000),
            price(cores("n1"), spot, "0", 6_655_000),
            price(memory("n1"), spot, "0", 892_000),
            price(gpu("nvidia-tesla-t4"), od, "0", 350_000_000),
            price(gpu("nvidia-tesla-t4"), spot, "0", 110_000_000),
            price(cores("a2"), od, "0", 33_174_000),
            price(memory("a2"), od, "0", 4_446_000),
            price(cores("a2"), spot, "0", 9_952_000),
            price(memory("a2"), spot, "0", 1_334_000),
            price(gpu("nvidia-tesla-a100"), od, "2", 933_908_000),
            price(gpu("nvidia-tesla-a100"), spot, "1", 100_000_000),
        ]
    );
    let first = query(&[("currencyCode", "USD"), ("pageSize", "5000")]);
    let second = query(&[
        ("currencyCode", "USD"),
        ("pageSize", "5000"),
        ("pageToken", "EXAMPLE-PAGE-TOKEN-2"),
    ]);
    assert_eq!(
        calls(&script),
        [(SKUS_URL.to_owned(), first), (SKUS_URL.to_owned(), second)]
    );
    assert_eq!(script.left(), 0);
}

/// Asserts that the SKUs the catalog keeps state the unit it divides by: the memory SKUs
/// per gibibyte-hour (`GiBy.h`), which `price.rs` turns into a price per MiB, and every
/// core and accelerator SKU per hour (`h`). The client reads descriptions and does not
/// read `usageUnit`, so this pins the fixtures to the assumption the price arithmetic
/// makes. It catches a fixture edited to a unit the arithmetic does not handle.
#[test]
fn the_priced_skus_state_the_units_the_arithmetic_assumes() {
    let mut checked = 0;
    for text in [
        fixture!("billing_skus_page1.json"),
        fixture!("billing_skus_page2.json"),
    ] {
        let page: serde_json::Value = serde_json::from_str(text).unwrap();
        for sku in page["skus"].as_array().unwrap() {
            let description = sku["description"].as_str().unwrap();
            let unit = sku["pricingInfo"][0]["pricingExpression"]["usageUnit"]
                .as_str()
                .unwrap();
            let expected = if description.contains("Instance Ram") {
                "GiBy.h"
            } else if description.contains("Instance Core") || description.contains("GPU") {
                "h"
            } else {
                continue;
            };
            assert_eq!(unit, expected, "{description}");
            checked += 1;
        }
    }
    // Fifteen SKUs less the storage SKU and the commitment: thirteen, one of them the
    // other region's core SKU.
    assert_eq!(checked, 13);
}

/// The catalog over the client and the fixtures: the machine types (two pages), the
/// accelerator types, the prices (two pages) and the region's quotas, with the zones, the
/// provisioning models and the allow list the offers below are checked against.
fn catalog_over_the_fixtures() -> (CatalogAnswer, Arc<script::Script>) {
    let mut answers = machine_pages();
    answers.push(answer(fixture!("accelerator_types_aggregated.json")));
    answers.extend(sku_pages());
    answers.push(answer(fixture!("regions_get_us_central1.json")));
    let (client, script) = over(answers);
    let settings = GceCatalogSettings::new(
        PROJECT,
        &[ZONE_A, ZONE_B],
        &["on-demand", "spot"],
        Some(vec![
            "n1-standard-4".to_owned(),
            "a2-highgpu-1g".to_owned(),
            "t2a-standard-4".to_owned(),
        ]),
    )
    .unwrap();
    let catalog = GceCatalog::new(Arc::new(client), settings);
    (catalog.catalog().unwrap(), script)
}

fn the_offer<'a>(offers: &'a [Offer], id: &str, zone_name: &str) -> &'a Offer {
    let mut found = offers
        .iter()
        .filter(|offer| offer.id == id && offer.zone.as_deref() == Some(zone_name));
    let offer = found.next().unwrap();
    assert!(found.next().is_none(), "{id} in {zone_name} twice");
    offer
}

/// Asserts that the catalog over the fixtures calls the API in its order (machine types
/// with the second page's token, accelerator types, SKUs with the second page's token, the
/// region once), leaves nothing unread and no note, and states 22 offers whose price in
/// whole US cents per hour is the sum of the parts rounded up: an N1 four-vCPU type at
/// 0.189999 dollars (19 cents), with a T4 at 0.539999 (54), with two at 0.889999 (89) and
/// four at 1.589999 (159); Spot at exactly 4, 15, 26 and 48 cents; an A2 with one A100 at
/// 3.705998 (371) and as Spot at 1.332814 (134); and a T2A, whose series the price table
/// does not list, with no price. It catches a memory price taken per MiB instead of per
/// GiB, truncation in place of rounding up, the wrong tier, and a price lost on the way
/// to the offer.
#[test]
fn the_catalog_over_the_fixtures_prices_each_offer_in_cents_per_hour() {
    let (answer, script) = catalog_over_the_fixtures();
    assert!(answer.notes.is_empty());
    assert_eq!(script.left(), 0);
    let bases: Vec<String> = calls(&script).into_iter().map(|(base, _)| base).collect();
    assert_eq!(
        bases,
        [
            format!("{COMPUTE}/aggregated/machineTypes"),
            format!("{COMPUTE}/aggregated/machineTypes"),
            format!("{COMPUTE}/aggregated/acceleratorTypes"),
            SKUS_URL.to_owned(),
            SKUS_URL.to_owned(),
            format!("{COMPUTE}/regions/us-central1"),
        ]
    );
    let tokens: Vec<Option<String>> = calls(&script)
        .into_iter()
        .map(|(_, pairs)| {
            pairs
                .into_iter()
                .find(|(name, _)| name == "pageToken")
                .map(|(_, token)| token)
        })
        .collect();
    assert_eq!(
        tokens,
        [
            None,
            Some("EXAMPLE-PAGE-TOKEN-1".to_owned()),
            None,
            None,
            Some("EXAMPLE-PAGE-TOKEN-2".to_owned()),
            None,
        ]
    );
    type Row = (&'static str, Option<u64>, Option<u64>);
    let a2: [Row; 1] = [("a2-highgpu-1g", Some(371), Some(134))];
    let n1: [Row; 4] = [
        ("n1-standard-4", Some(19), Some(4)),
        ("n1-standard-4+nvidia-tesla-t4x1", Some(54), Some(15)),
        ("n1-standard-4+nvidia-tesla-t4x2", Some(89), Some(26)),
        ("n1-standard-4+nvidia-tesla-t4x4", Some(159), Some(48)),
    ];
    let t2a: [Row; 1] = [("t2a-standard-4", None, None)];
    let mut expected = Vec::new();
    for (zones, rows) in [
        (vec![ZONE_A, ZONE_B], a2.to_vec()),
        (vec![ZONE_A, ZONE_B], n1.to_vec()),
        (vec![ZONE_B], t2a.to_vec()),
    ] {
        for zone_name in zones {
            for (id, on_demand, spot) in &rows {
                expected.push((format!("{id}/on-demand"), zone_name, *on_demand));
                expected.push((format!("{id}/spot"), zone_name, *spot));
            }
        }
    }
    let got: Vec<(String, &str, Option<u64>)> = answer
        .offers
        .iter()
        .map(|offer| {
            (
                offer.id.clone(),
                offer.zone.as_deref().unwrap(),
                offer.price_cents_per_hour,
            )
        })
        .collect();
    assert_eq!(got, expected);
    assert_eq!(got.len(), 22);
    assert!(
        answer
            .offers
            .iter()
            .all(|offer| offer.price_unit == PriceUnit::UsCents)
    );
}

/// Asserts, for five offers, what the offer states beyond its price: the vCPUs in
/// thousandths, the memory in bytes from `memoryMb` as MiB, no disk, the architecture
/// (stated by the A2 and the T2A, from the series table for the N1), the purchase option,
/// the accelerator with its count and, for the A100, its memory per GPU, and the quota the
/// region lists. A Spot N1 counts against the standard vCPU quota because the region's
/// preemptible limit is zero; a Spot T4 counts against the preemptible GPU limit; an A2 has
/// no vCPU quota row; a T2A has no quota. It catches a MiB read as bytes, an architecture
/// lost, and the quota of the wrong metric.
#[test]
fn offers_state_their_size_architecture_accelerator_and_quota() {
    let (answer, _) = catalog_over_the_fixtures();
    let limit = |limit, in_use| QuotaLimit { limit, in_use };

    let n1 = the_offer(&answer.offers, "n1-standard-4/on-demand", ZONE_A);
    assert_eq!(n1.cpu_millis, 4000);
    assert_eq!(n1.memory_bytes, 15360 << 20);
    assert_eq!(n1.disk_bytes, 0);
    assert_eq!(n1.arch, "x86_64");
    assert_eq!(n1.os, "linux");
    assert_eq!(n1.accelerator, None);
    assert_eq!(n1.purchase, Some(Purchase::OnDemand));
    assert!(n1.available);
    assert_eq!(
        n1.quota,
        Some(Quota {
            vcpus: Some(limit(24, Some(4))),
            accelerators: None,
        })
    );

    let n1_spot = the_offer(&answer.offers, "n1-standard-4/spot", ZONE_B);
    assert_eq!(n1_spot.purchase, Some(Purchase::Spot));
    assert_eq!(
        n1_spot.quota,
        Some(Quota {
            vcpus: Some(limit(24, Some(4))),
            accelerators: None,
        })
    );

    let t4 = the_offer(
        &answer.offers,
        "n1-standard-4+nvidia-tesla-t4x2/spot",
        ZONE_A,
    );
    let accelerator = t4.accelerator.as_ref().unwrap();
    assert_eq!(accelerator.model.as_str(), "nvidia-tesla-t4");
    assert_eq!(accelerator.count, 2);
    assert_eq!(accelerator.memory_per_device, None);
    assert_eq!(
        t4.quota,
        Some(Quota {
            vcpus: Some(limit(24, Some(4))),
            accelerators: Some(limit(2, Some(0))),
        })
    );

    let a2 = the_offer(&answer.offers, "a2-highgpu-1g/on-demand", ZONE_B);
    assert_eq!(a2.cpu_millis, 12_000);
    assert_eq!(a2.memory_bytes, 87040 << 20);
    assert_eq!(a2.arch, "x86_64");
    let accelerator = a2.accelerator.as_ref().unwrap();
    assert_eq!(accelerator.model.as_str(), "nvidia-a100");
    assert_eq!(accelerator.count, 1);
    assert_eq!(accelerator.memory_per_device, Some(40 << 30));
    assert_eq!(
        a2.quota,
        Some(Quota {
            vcpus: None,
            accelerators: Some(limit(8, Some(1))),
        })
    );

    let t2a = the_offer(&answer.offers, "t2a-standard-4/on-demand", ZONE_B);
    assert_eq!(t2a.arch, "arm64");
    assert_eq!(t2a.cpu_millis, 4000);
    assert_eq!(t2a.memory_bytes, 16384 << 20);
    assert_eq!(t2a.price_cents_per_hour, None);
    assert_eq!(t2a.quota, None);
}
