//! The catalog side: instance types, offerings, prices, quotas and the offers the catalog states.

use super::*;

/// Asserts that `DescribeInstanceTypes` is read over two pages: the first request has no
/// token, the second carries the token of the first answer, the rows of both pages come
/// back in order, and each row's vCPUs, memory in MiB, architectures, GPUs, instance store
/// in GB and bare-metal flag are the ones the answer states (an absent instance store is
/// `None`). Catches a page token read from the wrong element and a field read from the
/// wrong path.
#[test]
fn describe_instance_types_pages_are_merged_with_every_field() {
    let (api, script) = client(vec![
        ok("describe_instance_types_page1.xml"),
        ok("describe_instance_types_page2.xml"),
    ]);
    let types = api.instance_types(&region()).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(script.left(), 0);
    assert_eq!(sent[0].url, "https://ec2.us-east-1.amazonaws.com/");
    for request in &sent {
        assert_eq!(
            param(request, "Action").as_deref(),
            Some("DescribeInstanceTypes")
        );
        assert_eq!(param(request, "Version").as_deref(), Some("2016-11-15"));
        assert_eq!(param(request, "MaxResults").as_deref(), Some("100"));
    }
    assert_eq!(param(&sent[0], "NextToken"), None);
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some(TOKEN));
    let plain = |name: &str, arch: &str, vcpus: u32, mib: u64| InstanceTypeInfo {
        name: typed(name),
        vcpus,
        memory_mib: mib,
        architectures: names(&[arch]),
        gpus: Vec::new(),
        instance_storage_gb: None,
        bare_metal: false,
    };
    assert_eq!(
        types,
        vec![
            plain("m7i.large", "x86_64", 2, 8192),
            InstanceTypeInfo {
                bare_metal: true,
                ..plain("c7g.metal", "arm64", 64, 131_072)
            },
            InstanceTypeInfo {
                gpus: vec![GpuInfo {
                    name: "A10G".to_owned(),
                    manufacturer: "NVIDIA".to_owned(),
                    count: 1,
                    memory_mib: 24_576,
                }],
                instance_storage_gb: Some(250),
                ..plain("g5.xlarge", "x86_64", 4, 16_384)
            },
            InstanceTypeInfo {
                gpus: vec![GpuInfo {
                    name: "H100".to_owned(),
                    manufacturer: "NVIDIA".to_owned(),
                    count: 8,
                    memory_mib: 81_920,
                }],
                instance_storage_gb: Some(30_720),
                ..plain("p5.48xlarge", "x86_64", 192, 2_097_152)
            },
        ]
    );
}

/// Asserts that `DescribeInstanceTypeOfferings` is asked by availability zone and read
/// into one (type, zone) pair per item, in order. Catches a location read from the wrong
/// element.
#[test]
fn describe_instance_type_offerings_are_type_and_zone_pairs() {
    let (api, script) = client(vec![ok("describe_instance_type_offerings.xml")]);
    let pairs = api.zone_offerings(&region()).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("DescribeInstanceTypeOfferings")
    );
    assert_eq!(
        param(&sent[0], "LocationType").as_deref(),
        Some("availability-zone")
    );
    let pair = |kind: &str, at: &str| ZoneOffering {
        instance_type: typed(kind),
        zone: zone(at),
    };
    assert_eq!(
        pairs,
        vec![
            pair("m7i.large", "us-east-1a"),
            pair("m7i.large", "us-east-1b"),
            pair("c7g.metal", "us-east-1a"),
            pair("g5.xlarge", "us-east-1a"),
            pair("g5.xlarge", "us-east-1b"),
            pair("p5.48xlarge", "us-east-1b"),
        ]
    );
}

/// Asserts that `DescribeSpotPriceHistory` is asked for `Linux/UNIX` from the client's
/// clock, that an empty `nextToken` element ends the walk after one page, and that each
/// row keeps its price as the decimal text the answer states. Catches an empty token
/// taken for a next page.
#[test]
fn describe_spot_price_history_keeps_the_price_text_and_stops_at_an_empty_token() {
    let (api, script) = client(vec![ok("describe_spot_price_history.xml")]);
    let prices = api.spot_prices(&region()).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("DescribeSpotPriceHistory")
    );
    assert_eq!(
        param(&sent[0], "ProductDescription.1").as_deref(),
        Some("Linux/UNIX")
    );
    assert_eq!(
        param(&sent[0], "StartTime").as_deref(),
        Some("2026-11-05T01:01:01Z")
    );
    let price = |kind: &str, at: &str, usd: &str| ZonePrice {
        instance_type: typed(kind),
        zone: zone(at),
        usd_per_hour: usd.to_owned(),
    };
    assert_eq!(
        prices,
        vec![
            price("m7i.large", "us-east-1a", "0.041200"),
            price("m7i.large", "us-east-1b", "0.039800"),
            price("c7g.metal", "us-east-1a", "0.927500"),
            price("g5.xlarge", "us-east-1a", "0.401200"),
            price("p5.48xlarge", "us-east-1b", "21.736200"),
        ]
    );
}

/// Asserts that `DescribeCapacityBlockOfferings` is asked for the hours given and read
/// into the offering id, type, zone, instance count, start as seconds since the epoch,
/// length in minutes, upfront fee text and currency. Catches a start time or a duration
/// read in the wrong unit.
#[test]
fn describe_capacity_block_offerings_carry_start_length_fee_and_currency() {
    let (api, script) = client(vec![ok("describe_capacity_block_offerings.xml")]);
    let blocks = api.capacity_block_offerings(&region(), 24).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("DescribeCapacityBlockOfferings")
    );
    assert_eq!(
        param(&sent[0], "CapacityDurationHours").as_deref(),
        Some("24")
    );
    let block = |id: &str, at: &str, count: u32, fee: &str| BlockOffering {
        id: id.to_owned(),
        instance_type: typed("p5.48xlarge"),
        zone: zone(at),
        instance_count: count,
        start_unix_seconds: 1_794_483_000,
        duration_minutes: 24 * 60,
        upfront_fee: fee.to_owned(),
        currency: "USD".to_owned(),
    };
    assert_eq!(
        blocks,
        vec![
            block("cb-0123456789abcdef0", "us-east-1b", 1, "1000.0000"),
            block("cb-0fedcba9876543210", "us-east-1a", 2, "2400.0000"),
        ]
    );
}

/// Asserts that Pricing `GetProducts` is read over two pages: the first request has no
/// `NextToken`, the second carries the token of the first answer, the entries (JSON
/// documents inside strings) of both pages come back in order with the instance type and
/// the hourly USD price text of their on-demand term, and the request goes to the Pricing
/// endpoint with its target header. Catches a price taken from the wrong dimension and a
/// page token key that is not `NextToken`.
#[test]
fn get_products_pages_are_merged_with_the_hourly_price() {
    let (api, script) = client(vec![
        ok("get_products_page1.json"),
        ok("get_products_page2.json"),
    ]);
    let prices = api.on_demand_prices(&region()).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].url, "https://api.pricing.us-east-1.amazonaws.com/");
    for request in &sent {
        assert_eq!(
            request.header("x-amz-target"),
            Some("AWSPriceListService.GetProducts")
        );
        let body = script::json(request);
        assert_eq!(body["ServiceCode"], "AmazonEC2");
        assert_eq!(body["FormatVersion"], "aws_v1");
    }
    assert!(script::json(&sent[0]).get("NextToken").is_none());
    assert_eq!(script::json(&sent[1])["NextToken"], TOKEN);
    let price = |kind: &str, usd: &str| TypePrice {
        instance_type: typed(kind),
        usd_per_hour: usd.to_owned(),
    };
    assert_eq!(
        prices,
        vec![
            price("m7i.large", "0.1008000000"),
            price("c7g.metal", "2.3200000000"),
            price("g5.xlarge", "1.0060000000"),
            price("p5.48xlarge", "55.0400000000"),
        ]
    );
}

/// Asserts that Service Quotas `GetServiceQuota` is asked for the quota code of each
/// family and purchase option, at the Service Quotas endpoint of the region with the
/// documented target, and that `Quota.Value` becomes the vCPU limit with no usage figure.
/// Catches a limit read from the wrong key and a quota code that is not the family's.
#[test]
fn get_service_quota_reads_the_limit_of_each_family_and_purchase() {
    let cases = [
        (
            "get_service_quota_standard_on_demand.json",
            QuotaFamily::Standard,
            PurchaseKind::OnDemand,
            "L-1216C47A",
            640,
        ),
        (
            "get_service_quota_standard_spot.json",
            QuotaFamily::Standard,
            PurchaseKind::Spot,
            "L-34B43A08",
            320,
        ),
        (
            "get_service_quota_p_on_demand.json",
            QuotaFamily::P,
            PurchaseKind::OnDemand,
            "L-417A185B",
            384,
        ),
        (
            "get_service_quota_p_spot.json",
            QuotaFamily::P,
            PurchaseKind::Spot,
            "L-7212CCBC",
            192,
        ),
    ];
    for (file, family, purchase, code, limit) in cases {
        let (api, script) = client(vec![ok(file)]);
        let quota = api.vcpu_quota(&region(), family, purchase).unwrap();
        assert_eq!(
            quota,
            Some(VcpuQuota {
                limit,
                in_use: None
            }),
            "{file}"
        );
        let sent = script.sent();
        assert_eq!(sent.len(), 1, "{file}");
        assert_eq!(
            sent[0].url,
            "https://servicequotas.us-east-1.amazonaws.com/"
        );
        assert_eq!(sent[0].header("x-amz-target"), Some(QUOTAS_TARGET));
        let body = script::json(&sent[0]);
        assert_eq!(body["ServiceCode"], "ec2", "{file}");
        assert_eq!(body["QuotaCode"], code, "{file}");
    }
}

/// The settings of the catalog tests: the two types the fixtures price and offer, on
/// demand, on spot and as 24-hour Capacity Blocks.
fn all_purchases() -> Ec2CatalogSettings {
    Ec2CatalogSettings::new(
        REGION,
        &["on-demand", "spot", "capacity-block"],
        Some(vec!["m7i.large".to_owned(), "p5.48xlarge".to_owned()]),
        Some(24),
    )
    .unwrap()
}

/// The offer of a machine shape: the fields every offer of a type shares.
pub(crate) fn machine(kind: &str, vcpus: u64, mib: u64, disk_gb: u64) -> Offer {
    Offer {
        id: kind.to_owned(),
        zone: None,
        cpu_millis: vcpus * 1000,
        memory_bytes: mib << 20,
        disk_bytes: disk_gb * 1_000_000_000,
        accelerator: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// `machine` sold as `purchase` in `at` for `cents` an hour, under `limit` vCPUs.
fn sold(base: &Offer, purchase: Purchase, at: &str, cents: u64, limit: Option<u32>) -> Offer {
    let option = match purchase {
        Purchase::OnDemand => "on-demand",
        _ => "spot",
    };
    Offer {
        id: format!("{}/{option}", base.id),
        zone: Some(at.to_owned()),
        price_cents_per_hour: Some(cents),
        purchase: Some(purchase),
        quota: limit.map(|limit| Quota {
            vcpus: Some(QuotaLimit {
                limit,
                in_use: None,
            }),
            accelerators: None,
        }),
        ..base.clone()
    }
}

/// Asserts the whole catalog read over the documented answers, call by call in the order
/// the catalog makes them (types over two pages, zone offerings, prices over two pages,
/// spot prices, Capacity Blocks, then the four quotas): the requests carry the page token
/// of the first page and the quota codes of the two families; the offers of the two
/// allowed types carry vCPUs as thousandths of a core, memory in bytes from MiB, the
/// instance store in bytes from GB, the architecture and the H100 accelerator; on-demand
/// and spot prices are in US cents with a fraction of a cent rounded up (0.1008 dollars is
/// 11 cents, 0.0412 is 5, 0.0398 is 4, 21.7362 is 2174); a Capacity Block is priced by
/// its upfront fee over its instances and hours, rounded up, and carries its terms and no
/// quota; and the quotas are the Service Quotas values. Catches a unit or rounding error
/// in any conversion and an offer built from the wrong row.
#[test]
fn the_catalog_over_the_documented_answers_becomes_offers() {
    let (api, script) = client(vec![
        ok("describe_instance_types_page1.xml"),
        ok("describe_instance_types_page2.xml"),
        ok("describe_instance_type_offerings.xml"),
        ok("get_products_page1.json"),
        ok("get_products_page2.json"),
        ok("describe_spot_price_history.xml"),
        ok("describe_capacity_block_offerings.xml"),
        ok("get_service_quota_standard_on_demand.json"),
        ok("get_service_quota_standard_spot.json"),
        ok("get_service_quota_p_on_demand.json"),
        ok("get_service_quota_p_spot.json"),
    ]);
    let answer = Ec2Catalog::new(Arc::new(api), all_purchases())
        .catalog()
        .unwrap();
    assert_eq!(answer.notes, []);
    assert_eq!(script.left(), 0);
    let sent = script.sent();
    assert_eq!(sent.len(), 11);
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some(TOKEN));
    assert_eq!(script::json(&sent[4])["NextToken"], TOKEN);
    let quota_codes: Vec<String> = sent[7..]
        .iter()
        .map(|request| {
            script::json(request)["QuotaCode"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        quota_codes,
        ["L-1216C47A", "L-34B43A08", "L-417A185B", "L-7212CCBC"]
    );

    let small = machine("m7i.large", 2, 8192, 0);
    let big = Offer {
        accelerator: Some(accelerator_of(
            &Model::parse("nvidia-h100-80gb-hbm3").unwrap(),
            8,
            Some(81_920 << 20),
        )),
        ..machine("p5.48xlarge", 192, 2_097_152, 30_720)
    };
    let block = |id: &str, at: &str, instances: u32, upfront_cents: u64, cents: u64| Offer {
        id: format!("p5.48xlarge/capacity-block/{id}"),
        zone: Some(at.to_owned()),
        price_cents_per_hour: Some(cents),
        purchase: Some(Purchase::CapacityBlock),
        block: Some(BlockTerms {
            offering: id.to_owned(),
            start_unix_seconds: 1_794_483_000,
            duration_minutes: 1440,
            upfront_cents,
            instances,
        }),
        ..big.clone()
    };
    assert_eq!(
        answer.offers,
        vec![
            sold(&small, Purchase::OnDemand, "us-east-1a", 11, Some(640)),
            sold(&small, Purchase::Spot, "us-east-1a", 5, Some(320)),
            sold(&small, Purchase::OnDemand, "us-east-1b", 11, Some(640)),
            sold(&small, Purchase::Spot, "us-east-1b", 4, Some(320)),
            block("cb-0fedcba9876543210", "us-east-1a", 2, 240_000, 5000),
            sold(&big, Purchase::OnDemand, "us-east-1b", 5504, Some(384)),
            sold(&big, Purchase::Spot, "us-east-1b", 2174, Some(192)),
            block("cb-0123456789abcdef0", "us-east-1b", 1, 100_000, 4167),
        ]
    );
}

/// Asserts that a quota the account may not read (the denial fixture) leaves the offers
/// standing without a quota and adds one note naming the family, the purchase option and
/// the denial; and that a price list the cloud throttles fails the whole catalog with the
/// action and the code in its words, never a partial catalog. Catches a quota failure that
/// drops the offers and a price failure that yields offers without prices.
#[test]
fn a_denied_quota_is_a_note_and_a_throttled_price_list_fails_the_catalog() {
    let settings = Ec2CatalogSettings::new(
        REGION,
        &["on-demand"],
        Some(vec!["m7i.large".to_owned()]),
        None,
    )
    .unwrap();
    let (api, _) = client(vec![
        ok("describe_instance_types_page1.xml"),
        ok("describe_instance_types_page2.xml"),
        ok("describe_instance_type_offerings.xml"),
        ok("get_products_page1.json"),
        ok("get_products_page2.json"),
        failing(400, "error_quotas_access_denied_exception.json"),
    ]);
    let answer = Ec2Catalog::new(Arc::new(api), settings.clone())
        .catalog()
        .unwrap();
    assert_eq!(answer.notes.len(), 1);
    assert_eq!(answer.notes[0].family, QuotaFamily::Standard);
    assert_eq!(answer.notes[0].purchase, PurchaseKind::OnDemand);
    assert_eq!(answer.notes[0].error, denied("AccessDeniedException"));
    let small = machine("m7i.large", 2, 8192, 0);
    assert_eq!(
        answer.offers,
        vec![
            sold(&small, Purchase::OnDemand, "us-east-1a", 11, None),
            sold(&small, Purchase::OnDemand, "us-east-1b", 11, None),
        ]
    );

    let (api, _) = client(vec![
        ok("describe_instance_types_page1.xml"),
        ok("describe_instance_types_page2.xml"),
        ok("describe_instance_type_offerings.xml"),
        failing(400, "error_pricing_throttling_exception.json"),
    ]);
    let failure = Ec2Catalog::new(Arc::new(api), settings)
        .catalog()
        .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "GetProducts failed: throttled (ThrottlingException)"
    );
}
