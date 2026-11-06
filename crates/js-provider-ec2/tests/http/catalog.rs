//! The EC2 reads of the catalog: the request each makes and the records each answer is
//! read into.

use js_provider_ec2::{
    ApiError, ArchName, BlockOffering, EC2_API_VERSION, Ec2Api, GpuInfo, InstanceType,
    InstanceTypeInfo, Zone, ZoneOffering, ZonePrice,
};

use crate::script::{
    KEY_ID, REGION, answer, form, param, region, rig, signed_headers, status, xml,
};

/// A GPU type, as `DescribeInstanceTypes` answers it.
const GPU_TYPE: &str = "<item><instanceType>p5.48xlarge</instanceType>\
    <vCpuInfo><defaultVCpus>192</defaultVCpus><defaultCores>96</defaultCores></vCpuInfo>\
    <memoryInfo><sizeInMiB>2097152</sizeInMiB></memoryInfo>\
    <processorInfo><supportedArchitectures><item>x86_64</item></supportedArchitectures></processorInfo>\
    <gpuInfo><gpus><item><name>H100</name><manufacturer>NVIDIA</manufacturer><count>8</count>\
    <memoryInfo><sizeInMiB>81920</sizeInMiB></memoryInfo></item></gpus></gpuInfo>\
    <instanceStorageInfo><totalSizeInGB>30400</totalSizeInGB></instanceStorageInfo>\
    <bareMetal>false</bareMetal></item>";

/// A CPU type with two architectures, no GPU, no local storage, bare metal.
const CPU_TYPE: &str = "<item><instanceType>m7g.metal</instanceType>\
    <vCpuInfo><defaultVCpus>64</defaultVCpus></vCpuInfo>\
    <memoryInfo><sizeInMiB>262144</sizeInMiB></memoryInfo>\
    <processorInfo><supportedArchitectures><item>arm64</item><item>x86_64</item>\
    </supportedArchitectures></processorInfo>\
    <gpuInfo/>\
    <bareMetal>true</bareMetal></item>";

fn types_page(items: &str, next: Option<&str>) -> String {
    let token = next.map_or(String::new(), |t| format!("<nextToken>{t}</nextToken>"));
    xml(
        "DescribeInstanceTypesResponse",
        &format!("<instanceTypeSet>{items}</instanceTypeSet>{token}"),
    )
}

/// Asserts that `instance_types` is one signed POST to the region's EC2 endpoint named
/// `DescribeInstanceTypes` with a page size, signed for `ec2` with the content type and
/// the session token among the signed headers, and that the answer is read into the model:
/// the GPU type with its GPU, memory, architecture and local storage, and the bare-metal
/// CPU type with two architectures, no GPU and no storage. It catches a request sent to
/// the wrong host or signed for the wrong service, and a field read from the wrong
/// element.
#[test]
fn instance_types_are_requested_signed_and_read_into_the_model() {
    let (client, script) = rig(vec![answer(&types_page(
        &format!("{GPU_TYPE}{CPU_TYPE}"),
        None,
    ))]);
    let types = client.instance_types(&region(REGION)).unwrap();
    assert_eq!(
        types,
        vec![
            InstanceTypeInfo {
                name: InstanceType("p5.48xlarge".to_owned()),
                vcpus: 192,
                memory_mib: 2_097_152,
                architectures: vec![ArchName("x86_64".to_owned())],
                gpus: vec![GpuInfo {
                    name: "H100".to_owned(),
                    manufacturer: "NVIDIA".to_owned(),
                    count: 8,
                    memory_mib: 81_920,
                }],
                instance_storage_gb: Some(30_400),
                bare_metal: false,
            },
            InstanceTypeInfo {
                name: InstanceType("m7g.metal".to_owned()),
                vcpus: 64,
                memory_mib: 262_144,
                architectures: vec![ArchName("arm64".to_owned()), ArchName("x86_64".to_owned())],
                gpus: Vec::new(),
                instance_storage_gb: None,
                bare_metal: true,
            },
        ]
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    let request = &sent[0];
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(request.url, "https://ec2.region-a.amazonaws.com/");
    assert_eq!(
        request.header("content-type"),
        Some("application/x-www-form-urlencoded; charset=utf-8")
    );
    assert_eq!(request.header("host"), Some("ec2.region-a.amazonaws.com"));
    assert_eq!(request.header("x-amz-date"), Some("20261105T010101Z"));
    assert!(request.header("x-amz-security-token").is_some());
    assert!(
        request
            .header("authorization")
            .unwrap()
            .starts_with(&format!(
                "AWS4-HMAC-SHA256 Credential={KEY_ID}/20261105/region-a/ec2/aws4_request"
            ))
    );
    assert_eq!(
        signed_headers(request),
        ["content-type", "host", "x-amz-date", "x-amz-security-token"]
    );
    assert_eq!(
        param(request, "Action").as_deref(),
        Some("DescribeInstanceTypes")
    );
    assert_eq!(param(request, "Version").as_deref(), Some(EC2_API_VERSION));
    assert_eq!(param(request, "MaxResults").as_deref(), Some("100"));
    assert_eq!(param(request, "NextToken"), None);
}

/// Asserts that a call goes to the region it is given, not the client's own: the host,
/// the credential scope and nothing of `region-a` appear in a request for `region-b`.
/// It catches a client that ignores the region argument.
#[test]
fn a_catalog_call_goes_to_the_region_it_is_given() {
    let (client, script) = rig(vec![answer(&types_page("", None))]);
    assert_eq!(client.instance_types(&region("region-b")).unwrap(), vec![]);
    let request = &script.sent()[0];
    assert_eq!(request.url, "https://ec2.region-b.amazonaws.com/");
    assert!(
        request
            .header("authorization")
            .unwrap()
            .contains("/region-b/ec2/aws4_request")
    );
}

/// Asserts that a list in two pages is read whole: the second request carries the token
/// of the first answer and the rows of both come back in order. It catches a walk that
/// stops at the first page.
#[test]
fn a_list_in_two_pages_is_read_whole() {
    let (client, script) = rig(vec![
        answer(&types_page(GPU_TYPE, Some("token+one/="))),
        answer(&types_page(CPU_TYPE, None)),
    ]);
    let names: Vec<String> = client
        .instance_types(&region(REGION))
        .unwrap()
        .into_iter()
        .map(|info| info.name.0)
        .collect();
    assert_eq!(names, ["p5.48xlarge", "m7g.metal"]);
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(param(&sent[0], "NextToken"), None);
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some("token+one/="));
}

/// Asserts that a second page that fails fails the call with that page's error, and that
/// a page whose token repeats the one just given is incomplete: no short list is ever
/// returned. It catches a walk that returns the rows it has.
#[test]
fn a_page_that_fails_or_repeats_its_token_fails_the_list() {
    let throttle =
        "<Response><Errors><Error><Code>RequestLimitExceeded</Code></Error></Errors></Response>";
    let (client, script) = rig(vec![
        answer(&types_page(GPU_TYPE, Some("next"))),
        status(503, throttle),
    ]);
    assert_eq!(
        client.instance_types(&region(REGION)),
        Err(ApiError::answered("RequestLimitExceeded"))
    );
    assert_eq!(script.sent().len(), 2);

    let (client, _) = rig(vec![
        answer(&types_page(GPU_TYPE, Some("next"))),
        answer(&types_page(GPU_TYPE, Some("next"))),
    ]);
    assert_eq!(
        client.instance_types(&region(REGION)),
        Err(ApiError::Incomplete)
    );
}

/// Asserts that an instance type answer missing a field the documentation gives, or
/// holding a number that is not one, is unreadable and names the field: the type, its
/// vCPUs, its memory, the name, maker, count and memory of a GPU, and its local storage.
/// It catches a field that defaults to zero.
#[test]
fn an_instance_type_with_a_missing_or_bad_field_is_unreadable() {
    let cases = [
        (
            "<instanceType>p5.48xlarge</instanceType>",
            "",
            "instanceType is missing",
        ),
        (
            "<defaultVCpus>192</defaultVCpus>",
            "<defaultVCpus>many</defaultVCpus>",
            "vCpuInfo/defaultVCpus is not a number",
        ),
        (
            "<vCpuInfo><defaultVCpus>192</defaultVCpus><defaultCores>96</defaultCores></vCpuInfo>",
            "",
            "vCpuInfo/defaultVCpus is missing",
        ),
        (
            "<memoryInfo><sizeInMiB>2097152</sizeInMiB></memoryInfo>",
            "",
            "memoryInfo/sizeInMiB is missing",
        ),
        ("<name>H100</name>", "", "name is missing"),
        (
            "<manufacturer>NVIDIA</manufacturer>",
            "",
            "manufacturer is missing",
        ),
        (
            "<count>8</count>",
            "<count>-1</count>",
            "count is not a number",
        ),
        (
            "<memoryInfo><sizeInMiB>81920</sizeInMiB></memoryInfo></item></gpus>",
            "</item></gpus>",
            "memoryInfo/sizeInMiB is missing",
        ),
        (
            "<totalSizeInGB>30400</totalSizeInGB>",
            "<totalSizeInGB>lots</totalSizeInGB>",
            "instanceStorageInfo/totalSizeInGB is not a number",
        ),
    ];
    for (from, to, what) in cases {
        assert!(GPU_TYPE.contains(from), "{from}");
        let broken = GPU_TYPE.replacen(from, to, 1);
        let (client, _) = rig(vec![answer(&types_page(&broken, None))]);
        assert_eq!(
            client.instance_types(&region(REGION)),
            Err(ApiError::Unreadable(what.to_owned())),
            "{what}"
        );
    }
}

/// Asserts that `zone_offerings` asks for `availability-zone` locations with a page size
/// and reads each row into the type and the zone, over two pages, and that a row with no
/// location is unreadable. It catches a read of the wrong location type.
#[test]
fn zone_offerings_are_read_by_availability_zone() {
    let page = |rows: &str, next: &str| {
        answer(&xml(
            "DescribeInstanceTypeOfferingsResponse",
            &format!("<instanceTypeOfferingSet>{rows}</instanceTypeOfferingSet>{next}"),
        ))
    };
    let row = |kind: &str, zone: &str| {
        format!(
            "<item><instanceType>{kind}</instanceType><locationType>availability-zone</locationType><location>{zone}</location></item>"
        )
    };
    let (client, script) = rig(vec![
        page(
            &row("p5.48xlarge", "zone-a1"),
            "<nextToken>more</nextToken>",
        ),
        page(&row("m7g.metal", "zone-b1"), ""),
    ]);
    assert_eq!(
        client.zone_offerings(&region(REGION)).unwrap(),
        vec![
            ZoneOffering {
                instance_type: InstanceType("p5.48xlarge".to_owned()),
                zone: Zone("zone-a1".to_owned()),
            },
            ZoneOffering {
                instance_type: InstanceType("m7g.metal".to_owned()),
                zone: Zone("zone-b1".to_owned()),
            },
        ]
    );
    let sent = script.sent();
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("DescribeInstanceTypeOfferings")
    );
    assert_eq!(
        param(&sent[0], "LocationType").as_deref(),
        Some("availability-zone")
    );
    assert_eq!(param(&sent[0], "MaxResults").as_deref(), Some("1000"));
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some("more"));

    let (client, _) = rig(vec![page(
        "<item><instanceType>m7g.metal</instanceType></item>",
        "",
    )]);
    assert_eq!(
        client.zone_offerings(&region(REGION)),
        Err(ApiError::Unreadable("location is missing".to_owned()))
    );
}

/// Asserts that `spot_prices` asks for Linux/UNIX prices from the clock's time with a
/// page size, reads each row into the type, the zone and the price text exactly as
/// written, and that a row with no price is unreadable. It catches a price parsed into a
/// float and a request with no start time, which returns the whole history.
#[test]
fn spot_prices_are_requested_from_now_and_read_as_written() {
    let body = "<spotPriceHistorySet><item><instanceType>p5.48xlarge</instanceType>\
        <productDescription>Linux/UNIX</productDescription><spotPrice>12.3400000000</spotPrice>\
        <timestamp>2026-11-05T01:00:00.000Z</timestamp><availabilityZone>zone-a1</availabilityZone></item>\
        </spotPriceHistorySet>";
    let (client, script) = rig(vec![answer(&xml("DescribeSpotPriceHistoryResponse", body))]);
    assert_eq!(
        client.spot_prices(&region(REGION)).unwrap(),
        vec![ZonePrice {
            instance_type: InstanceType("p5.48xlarge".to_owned()),
            zone: Zone("zone-a1".to_owned()),
            usd_per_hour: "12.3400000000".to_owned(),
        }]
    );
    let request = &script.sent()[0];
    assert_eq!(
        param(request, "Action").as_deref(),
        Some("DescribeSpotPriceHistory")
    );
    assert_eq!(
        param(request, "ProductDescription.1").as_deref(),
        Some("Linux/UNIX")
    );
    assert_eq!(
        param(request, "StartTime").as_deref(),
        Some("2026-11-05T01:01:01Z")
    );
    assert_eq!(param(request, "MaxResults").as_deref(), Some("1000"));

    let broken = body.replace("<spotPrice>12.3400000000</spotPrice>", "");
    let (client, _) = rig(vec![answer(&xml(
        "DescribeSpotPriceHistoryResponse",
        &broken,
    ))]);
    assert_eq!(
        client.spot_prices(&region(REGION)),
        Err(ApiError::Unreadable("spotPrice is missing".to_owned()))
    );
}

fn block(duration: &str, start: &str) -> String {
    format!(
        "<item><capacityBlockOfferingId>cb-0123456789abcdef0</capacityBlockOfferingId>\
        <instanceType>p5.48xlarge</instanceType><availabilityZone>zone-a1</availabilityZone>\
        <instanceCount>2</instanceCount><startDate>{start}</startDate>\
        <endDate>2026-11-09T11:30:00.000Z</endDate>{duration}\
        <upfrontFee>1234.5600</upfrontFee><currencyCode>USD</currencyCode><tenancy>default</tenancy></item>"
    )
}

/// Asserts that `capacity_block_offerings` asks for the length in hours with a page size
/// and reads each offering: the id, type, zone, count, start in seconds since the epoch,
/// the length as hours and minutes together in minutes (minutes may be absent), the fee
/// text and the currency. It catches a duration that drops the minutes or counts the hours
/// as minutes.
#[test]
fn capacity_blocks_are_read_with_their_terms() {
    let long = block(
        "<capacityBlockDurationHours>24</capacityBlockDurationHours>\
         <capacityBlockDurationMinutes>30</capacityBlockDurationMinutes>",
        "2026-11-05T12:00:00.000Z",
    );
    let short = block(
        "<capacityBlockDurationHours>48</capacityBlockDurationHours>",
        "2026-11-06T00:00:00Z",
    );
    let (client, script) = rig(vec![answer(&xml(
        "DescribeCapacityBlockOfferingsResponse",
        &format!("<capacityBlockOfferingSet>{long}{short}</capacityBlockOfferingSet>"),
    ))]);
    let blocks = client
        .capacity_block_offerings(&region(REGION), 24)
        .unwrap();
    let expected = |start: u64, minutes: u64| BlockOffering {
        id: "cb-0123456789abcdef0".to_owned(),
        instance_type: InstanceType("p5.48xlarge".to_owned()),
        zone: Zone("zone-a1".to_owned()),
        instance_count: 2,
        start_unix_seconds: start,
        duration_minutes: minutes,
        upfront_fee: "1234.5600".to_owned(),
        currency: "USD".to_owned(),
    };
    assert_eq!(
        blocks,
        vec![
            expected(1_793_880_000, 24 * 60 + 30),
            expected(1_793_923_200, 48 * 60)
        ]
    );
    let request = &script.sent()[0];
    assert_eq!(
        form(request)
            .iter()
            .filter(|(name, _)| name != "Version")
            .cloned()
            .collect::<Vec<_>>(),
        [
            ("Action", "DescribeCapacityBlockOfferings"),
            ("CapacityDurationHours", "24"),
            ("MaxResults", "100"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
    );
}

/// Asserts that a Capacity Block offering with no hours, minutes that are not a number, a
/// start that is not a time, a start before the epoch or a missing id is unreadable. It
/// catches a start that wraps around into a far future.
#[test]
fn a_capacity_block_with_a_missing_or_bad_field_is_unreadable() {
    let hours = "<capacityBlockDurationHours>24</capacityBlockDurationHours>";
    let cases = [
        (
            block("", "2026-11-05T12:00:00Z"),
            "capacityBlockDurationHours is missing",
        ),
        (
            block(
                &format!("{hours}<capacityBlockDurationMinutes>x</capacityBlockDurationMinutes>"),
                "2026-11-05T12:00:00Z",
            ),
            "capacityBlockDurationMinutes is not a number",
        ),
        (
            block(hours, "soon"),
            "startDate is not a time since the epoch",
        ),
        (
            block(hours, "2026-11-05T12:00:00Z").replace("cb-0123456789abcdef0", ""),
            "capacityBlockOfferingId is missing",
        ),
    ];
    for (item, what) in cases {
        let (client, _) = rig(vec![answer(&xml(
            "DescribeCapacityBlockOfferingsResponse",
            &format!("<capacityBlockOfferingSet>{item}</capacityBlockOfferingSet>"),
        ))]);
        assert_eq!(
            client.capacity_block_offerings(&region(REGION), 24),
            Err(ApiError::Unreadable(what.to_owned())),
            "{what}"
        );
    }
}
