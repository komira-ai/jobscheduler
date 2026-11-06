//! The catalog reads of the client: the request each makes (URL, query, pages) and the
//! records each answer is read into, the SKU table, and the catalog over the client. Every
//! answer is written by hand from the documented response shapes; the project, regions and
//! zones are made up.

use std::sync::Arc;

use js_logstore::HttpRequest;
use js_provider_gce::{
    AcceleratorTypeInfo, AcceleratorTypeName, ApiError, ArchName, AttachedAccelerator, GceApi,
    GceCatalog, GceCatalogSettings, MachineTypeInfo, MachineTypeName, Money, PricedItem, Project,
    QuotaMetric, Region, SeriesName, SkuPrice, UsageType, Zone,
};
use serde_json::{Map, Value, json};

use crate::script::{Answer, answer, api_calls, error_body, over, status};

const PROJECT: &str = "sample-project";
const COMPUTE: &str = "https://compute.googleapis.com/compute/v1/projects/sample-project";
/// The description of an N1 core SKU.
const CORE: &str = "N1 Predefined Instance Core running in Americas";

/// The description of an N1 memory SKU.
const RAM: &str = "N1 Predefined Instance Ram running in Americas";
const SKUS_URL: &str = "https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus";

fn project() -> Project {
    Project::new(PROJECT).unwrap()
}

fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap()
}

/// The zones the tests ask for.
fn zones() -> Vec<Zone> {
    vec![zone("region-one-a"), zone("region-one-b")]
}

fn region() -> Region {
    zone("region-one-a").region()
}

/// An answer with status 200 and the JSON `value`.
fn json_answer(value: &Value) -> Answer {
    answer(&value.to_string())
}

/// The URL without its query, and the query as sorted decoded pairs.
fn split(request: &HttpRequest) -> (&str, Vec<(String, String)>) {
    let (base, query) = request
        .url
        .split_once('?')
        .unwrap_or((request.url.as_str(), ""));
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap();
            (name.to_owned(), value.to_owned())
        })
        .collect();
    pairs.sort();
    (base, pairs)
}

/// `pairs` as sorted owned pairs, to compare with a request's query.
fn query(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    owned.sort();
    owned
}

/// A scope of an aggregated list holding `list` under `kind`.
fn scope(kind: &str, list: Vec<Value>) -> Value {
    let mut map = Map::new();
    map.insert(kind.to_owned(), Value::Array(list));
    Value::Object(map)
}

/// The scope of a zone with nothing on this page: a warning in place of the list.
fn no_results(name: &str) -> Value {
    json!({"warning": {
        "code": "NO_RESULTS_ON_PAGE",
        "message": format!("There are no results for scope 'zones/{name}' on this page."),
        "data": [{"key": "scope", "value": format!("zones/{name}")}],
    }})
}

/// A page of an aggregated list with the `items` object and, when given, a next token.
fn listing(items: Value, next: Option<&str>) -> Value {
    let mut page = json!({"id": "projects/sample-project/aggregated/list", "items": items});
    if let Some(token) = next {
        page["nextPageToken"] = json!(token);
    }
    page
}

/// A machine type of 8 vCPUs in `zone` (a name or a URL), with no accelerator.
fn machine_json(name: &str, zone: &str) -> Value {
    json!({
        "name": name,
        "zone": zone,
        "guestCpus": 8,
        "memoryMb": 30720,
        "isSharedCpu": false,
    })
}

/// The record of `machine_json(name, zone)`.
fn machine_info(name: &str, zone_name: &str) -> MachineTypeInfo {
    MachineTypeInfo {
        name: MachineTypeName(name.to_owned()),
        zone: zone(zone_name),
        vcpus: 8,
        memory_mib: 30720,
        accelerators: Vec::new(),
        arch: None,
    }
}

/// An A2 machine type with one built-in A100 and a stated architecture.
fn a2_json(zone_name: &str) -> Value {
    json!({
        "name": "a2-highgpu-1g",
        "zone": zone_name,
        "guestCpus": 12,
        "memoryMb": 87040,
        "accelerators": [{"guestAcceleratorType": "nvidia-tesla-a100", "guestAcceleratorCount": 1}],
        "architecture": "X86_64",
    })
}

/// An accelerator type in `zone`.
fn accelerator_json(name: &str, zone_name: &str, max: u32) -> Value {
    json!({
        "name": name,
        "zone": zone_name,
        "description": "made-up",
        "maximumCardsPerInstance": max,
    })
}

fn machine_types_of(answers: Vec<Answer>) -> Result<Vec<MachineTypeInfo>, ApiError> {
    let (client, _) = over(answers);
    client.machine_types(&project(), &zones())
}

fn accelerator_types_of(answers: Vec<Answer>) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
    let (client, _) = over(answers);
    client.accelerator_types(&project(), &zones())
}

fn quotas_of(body: &Value) -> Result<Vec<QuotaMetric>, ApiError> {
    let (client, _) = over(vec![json_answer(body)]);
    client.region_quotas(&project(), &region())
}

fn prices_of(answers: Vec<Answer>) -> Result<Vec<SkuPrice>, ApiError> {
    let (client, _) = over(answers);
    client.prices(&region())
}

/// `value` with the member `key` removed.
fn without(mut value: Value, key: &str) -> Value {
    value.as_object_mut().unwrap().remove(key);
    value
}

/// `value` with the member `key` set.
fn with(mut value: Value, key: &str, member: Value) -> Value {
    value[key] = member;
    value
}

/// The one-scope answer of a machine type list holding `element` in `region-one-a`.
fn machine_page(element: Value) -> Answer {
    json_answer(&listing(
        json!({"zones/region-one-a": scope("machineTypes", vec![element])}),
        None,
    ))
}

/// The one-scope answer of an accelerator type list holding `element` in `region-one-a`.
fn accelerator_page(element: Value) -> Answer {
    json_answer(&listing(
        json!({"zones/region-one-a": scope("acceleratorTypes", vec![element])}),
        None,
    ))
}

/// Asserts that `machine_types` is one GET of the project's aggregated machine types with
/// `returnPartialSuccess` and the largest page size, and that the answer is read into the
/// records: a type with a built-in accelerator and a stated architecture, a type whose
/// zone is a URL, and nothing from a zone that has a warning in place of a list. Scopes of
/// zones not asked for, a region scope and a global scope are not read (the dropped ones
/// hold elements that would not parse). It catches a request sent to the wrong path or
/// without the partial-success flag, a zone URL kept whole, and a scope of another zone
/// read.
#[test]
fn machine_types_are_requested_and_read_into_the_model() {
    let items = json!({
        "zones/region-one-a": scope("machineTypes", vec![
            a2_json("region-one-a"),
            machine_json(
                "n1-standard-8",
                "https://www.googleapis.com/compute/v1/projects/sample-project/zones/region-one-a",
            ),
        ]),
        "zones/region-one-b": no_results("region-one-b"),
        "zones/region-two-a": scope("machineTypes", vec![json!({"name": 7})]),
        "regions/region-one": scope("machineTypes", vec![json!({"name": 7})]),
        "global": {},
    });
    let (client, script) = over(vec![json_answer(&listing(items, None))]);
    let types = client.machine_types(&project(), &zones()).unwrap();
    assert_eq!(
        types,
        vec![
            MachineTypeInfo {
                name: MachineTypeName("a2-highgpu-1g".to_owned()),
                zone: zone("region-one-a"),
                vcpus: 12,
                memory_mib: 87040,
                accelerators: vec![AttachedAccelerator {
                    type_name: AcceleratorTypeName("nvidia-tesla-a100".to_owned()),
                    count: 1,
                }],
                arch: Some(ArchName("X86_64".to_owned())),
            },
            machine_info("n1-standard-8", "region-one-a"),
        ]
    );
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method.as_str(), "GET");
    let (base, pairs) = split(&calls[0]);
    assert_eq!(base, format!("{COMPUTE}/aggregated/machineTypes"));
    assert_eq!(
        pairs,
        query(&[("returnPartialSuccess", "true"), ("maxResults", "500")])
    );
    assert_eq!(script.left(), 0);
    assert_eq!(script.sent().len(), calls.len() + 1);
}

/// Asserts that a list in two pages is read whole and in order: the second request carries
/// the first answer's token as `pageToken` and the first carries none, and every other
/// parameter is repeated. It catches a walk that stops at the first page or that drops a
/// parameter on later pages.
#[test]
fn machine_types_in_two_pages_are_read_whole() {
    let first = json!({"zones/region-one-a": scope("machineTypes", vec![machine_json("q9-one", "region-one-a")])});
    let second = json!({"zones/region-one-b": scope("machineTypes", vec![machine_json("q9-two", "region-one-b")])});
    let (client, script) = over(vec![
        json_answer(&listing(first, Some("page2"))),
        json_answer(&listing(second, None)),
    ]);
    let names: Vec<String> = client
        .machine_types(&project(), &zones())
        .unwrap()
        .into_iter()
        .map(|info| info.name.0)
        .collect();
    assert_eq!(names, ["q9-one", "q9-two"]);
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        split(&calls[0]).1,
        query(&[("returnPartialSuccess", "true"), ("maxResults", "500")])
    );
    assert_eq!(
        split(&calls[1]).1,
        query(&[
            ("returnPartialSuccess", "true"),
            ("maxResults", "500"),
            ("pageToken", "page2"),
        ])
    );
    assert_eq!(script.left(), 0);
}

/// Asserts that an empty next-page token ends the walk after one request. It catches a
/// walk that asks for a page with an empty token.
#[test]
fn an_empty_next_token_ends_the_walk() {
    let items = json!({"zones/region-one-a": scope("machineTypes", vec![machine_json("q9-one", "region-one-a")])});
    let (client, script) = over(vec![json_answer(&listing(items, Some("")))]);
    assert_eq!(client.machine_types(&project(), &zones()).unwrap().len(), 1);
    assert_eq!(api_calls(&script).len(), 1);
    assert_eq!(script.left(), 0);
}

/// Asserts that a walk whose second page fails fails with that page's error (a denial),
/// that a first page that fails fails with its error (a throttle) after one request, and
/// that a page that repeats the token it was asked with is incomplete: no short list is
/// ever returned. It catches a walk that returns the rows it has.
#[test]
fn a_page_that_fails_or_repeats_its_token_fails_the_list() {
    let items = json!({"zones/region-one-a": scope("machineTypes", vec![machine_json("q9-one", "region-one-a")])});
    let (client, script) = over(vec![
        json_answer(&listing(items.clone(), Some("page2"))),
        status(403, &error_body(403, "forbidden")),
    ]);
    assert_eq!(
        client.machine_types(&project(), &zones()),
        Err(ApiError::answered("forbidden"))
    );
    assert_eq!(api_calls(&script).len(), 2);

    let (client, script) = over(vec![status(429, &error_body(429, "rateLimitExceeded"))]);
    assert_eq!(
        client.machine_types(&project(), &zones()),
        Err(ApiError::answered("rateLimitExceeded"))
    );
    assert_eq!(api_calls(&script).len(), 1);

    assert_eq!(
        machine_types_of(vec![
            json_answer(&listing(items.clone(), Some("same"))),
            json_answer(&listing(items, Some("same"))),
        ]),
        Err(ApiError::Incomplete)
    );
}

/// Asserts that an aggregated list that names an unreachable scope is incomplete, for
/// machine types and for accelerator types, even when the scope is not one asked for, and
/// that an empty `unreachables` list is not. It catches a partial answer returned as
/// whole.
#[test]
fn an_unreachable_scope_makes_the_list_incomplete() {
    let items = json!({"zones/region-one-a": scope("machineTypes", vec![machine_json("q9-one", "region-one-a")])});
    let page = with(
        listing(items.clone(), None),
        "unreachables",
        json!(["zones/region-two-a"]),
    );
    assert_eq!(
        machine_types_of(vec![json_answer(&page)]),
        Err(ApiError::Incomplete)
    );
    let page = with(
        listing(
            json!({"zones/region-one-a": scope("acceleratorTypes", vec![])}),
            None,
        ),
        "unreachables",
        json!(["zones/region-one-b"]),
    );
    assert_eq!(
        accelerator_types_of(vec![json_answer(&page)]),
        Err(ApiError::Incomplete)
    );
    let page = with(listing(items, None), "unreachables", json!([]));
    assert_eq!(machine_types_of(vec![json_answer(&page)]).unwrap().len(), 1);
}

/// Asserts that a machine type answer missing a field the documentation gives, or holding
/// one that is not what it says (text, a count that fits 32 bits, a list), is unreadable
/// and names the field, as is an answer with no `items` or a scope whose list is not a
/// list. It catches a field that defaults to zero or to empty.
#[test]
fn a_machine_type_with_a_missing_or_bad_field_is_unreadable() {
    let base = machine_json("q9-one", "region-one-a");
    let bad_accelerators = |list: Value| with(base.clone(), "accelerators", list);
    let cases = [
        (without(base.clone(), "name"), "name is missing"),
        (with(base.clone(), "name", json!(7)), "name is not text"),
        (without(base.clone(), "zone"), "zone is missing"),
        (
            with(base.clone(), "zone", json!("Bad Zone")),
            "zone is not a zone name",
        ),
        (without(base.clone(), "guestCpus"), "guestCpus is missing"),
        (
            with(base.clone(), "guestCpus", json!("eight")),
            "guestCpus is not a count",
        ),
        (
            with(base.clone(), "guestCpus", json!(5_000_000_000u64)),
            "guestCpus is not a count",
        ),
        (
            with(base.clone(), "guestCpus", json!(-1)),
            "guestCpus is not a count",
        ),
        (without(base.clone(), "memoryMb"), "memoryMb is missing"),
        (
            with(base.clone(), "memoryMb", json!(1.5)),
            "memoryMb is not a count",
        ),
        (
            bad_accelerators(json!("none")),
            "accelerators is not a list",
        ),
        (
            bad_accelerators(json!([{"guestAcceleratorCount": 1}])),
            "guestAcceleratorType is missing",
        ),
        (
            bad_accelerators(json!([{"guestAcceleratorType": "nvidia-tesla-t4"}])),
            "guestAcceleratorCount is missing",
        ),
        (
            with(base.clone(), "architecture", json!(64)),
            "architecture is not text",
        ),
    ];
    for (element, message) in cases {
        assert_eq!(
            machine_types_of(vec![machine_page(element)]),
            Err(ApiError::Unreadable(message.to_owned())),
            "{message}"
        );
    }
    assert_eq!(
        machine_types_of(vec![json_answer(&json!({"id": "list"}))]),
        Err(ApiError::Unreadable("items is missing".to_owned()))
    );
    let page = listing(
        json!({"zones/region-one-a": {"machineTypes": "none"}}),
        None,
    );
    assert_eq!(
        machine_types_of(vec![json_answer(&page)]),
        Err(ApiError::Unreadable(
            "machineTypes is not a list".to_owned()
        ))
    );
}

/// Asserts that `accelerator_types` is one GET of the project's aggregated accelerator
/// types with the same query, and that the answer is read into the records of the zones
/// asked for (a zone given as a URL is its name), with the other zones' scopes and the
/// warning scope giving nothing. It catches a request sent to the machine types path and
/// a record read from the wrong field.
#[test]
fn accelerator_types_are_requested_and_read_into_the_model() {
    let items = json!({
        "zones/region-one-a": scope("acceleratorTypes", vec![
            accelerator_json("nvidia-tesla-t4", "region-one-a", 4),
            accelerator_json(
                "nvidia-l4",
                "https://www.googleapis.com/compute/v1/projects/sample-project/zones/region-one-a",
                8,
            ),
        ]),
        "zones/region-one-b": no_results("region-one-b"),
        "zones/region-two-a": scope("acceleratorTypes", vec![json!({"name": 7})]),
    });
    let (client, script) = over(vec![json_answer(&listing(items, None))]);
    let types = client.accelerator_types(&project(), &zones()).unwrap();
    let info = |name: &str, max: u32| AcceleratorTypeInfo {
        name: AcceleratorTypeName(name.to_owned()),
        zone: zone("region-one-a"),
        max_per_instance: max,
    };
    assert_eq!(
        types,
        vec![info("nvidia-tesla-t4", 4), info("nvidia-l4", 8)]
    );
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 1);
    let (base, pairs) = split(&calls[0]);
    assert_eq!(base, format!("{COMPUTE}/aggregated/acceleratorTypes"));
    assert_eq!(
        pairs,
        query(&[("returnPartialSuccess", "true"), ("maxResults", "500")])
    );
}

/// Asserts that an accelerator type missing a field, or with a bad one, is unreadable and
/// names the field. It catches a maximum that defaults to zero.
#[test]
fn an_accelerator_type_with_a_missing_or_bad_field_is_unreadable() {
    let base = accelerator_json("nvidia-tesla-t4", "region-one-a", 4);
    let cases = [
        (without(base.clone(), "name"), "name is missing"),
        (without(base.clone(), "zone"), "zone is missing"),
        (
            with(base.clone(), "zone", json!("nowhere")),
            "zone is not a zone name",
        ),
        (
            without(base.clone(), "maximumCardsPerInstance"),
            "maximumCardsPerInstance is missing",
        ),
        (
            with(base, "maximumCardsPerInstance", json!("four")),
            "maximumCardsPerInstance is not a count",
        ),
    ];
    for (element, message) in cases {
        assert_eq!(
            accelerator_types_of(vec![accelerator_page(element)]),
            Err(ApiError::Unreadable(message.to_owned())),
            "{message}"
        );
    }
}

/// Asserts that `region_quotas` is one GET of the region with no query, and that its
/// `quotas` become records: whole numbers written with or without a fraction of zero, a
/// quota with no usage, and a limit of zero. It catches a request for the wrong resource
/// and a usage that defaults to zero.
#[test]
fn region_quotas_are_requested_and_read_into_the_model() {
    let body = json!({"name": "region-one", "quotas": [
        {"metric": "CPUS", "limit": 24.0, "usage": 8.0},
        {"metric": "NVIDIA_A100_GPUS", "limit": 16, "usage": 0},
        {"metric": "PREEMPTIBLE_NVIDIA_L4_GPUS", "limit": 0},
    ]});
    let (client, script) = over(vec![json_answer(&body)]);
    let quotas = client.region_quotas(&project(), &region()).unwrap();
    let metric = |name: &str, limit: u64, usage: Option<u64>| QuotaMetric {
        metric: name.to_owned(),
        limit,
        usage,
    };
    assert_eq!(
        quotas,
        vec![
            metric("CPUS", 24, Some(8)),
            metric("NVIDIA_A100_GPUS", 16, Some(0)),
            metric("PREEMPTIBLE_NVIDIA_L4_GPUS", 0, None),
        ]
    );
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method.as_str(), "GET");
    let (base, pairs) = split(&calls[0]);
    assert_eq!(base, format!("{COMPUTE}/regions/region-one"));
    assert!(pairs.is_empty());
}

/// Asserts that a region answer with no `quotas`, or a quota with no metric or limit, or a
/// limit or usage that is a string, fractional, negative or beyond what a JSON number
/// holds exactly, is unreadable. It catches a bad number truncated or clamped into a
/// quota.
#[test]
fn a_quota_with_a_missing_or_bad_number_is_unreadable() {
    let whole = |name: &str| -> Result<Vec<QuotaMetric>, ApiError> {
        Err(ApiError::Unreadable(format!(
            "{name} is not a whole number that is not negative"
        )))
    };
    assert_eq!(
        quotas_of(&json!({"name": "region-one"})),
        Err(ApiError::Unreadable("quotas is missing".to_owned()))
    );
    let missing = |element: Value| quotas_of(&json!({"quotas": [element]}));
    assert_eq!(
        missing(json!({"limit": 1})),
        Err(ApiError::Unreadable("metric is missing".to_owned()))
    );
    assert_eq!(
        missing(json!({"metric": "CPUS"})),
        Err(ApiError::Unreadable("limit is missing".to_owned()))
    );
    for bad in [json!("24"), json!(2.5), json!(-1), json!(1e300)] {
        assert_eq!(
            missing(json!({"metric": "CPUS", "limit": bad})),
            whole("limit")
        );
    }
    for bad in [json!(0.5), json!(-3.0)] {
        assert_eq!(
            missing(json!({"metric": "CPUS", "limit": 4, "usage": bad})),
            whole("usage")
        );
    }
}

/// A SKU of the Compute Engine service in the made-up catalog.
fn sku_json(description: &str, usage: &str, regions: &[&str], nanos: i64) -> Value {
    json!({
        "name": "services/6F81-5844-456A/skus/AAAA-0000-0000",
        "skuId": "AAAA-0000-0000",
        "description": description,
        "category": {
            "serviceDisplayName": "Compute Engine",
            "resourceFamily": "Compute",
            "resourceGroup": "N1Standard",
            "usageType": usage,
        },
        "serviceRegions": regions,
        "pricingInfo": [{
            "pricingExpression": {
                "usageUnit": "h",
                "tieredRates": [{
                    "startUsageAmount": 0,
                    "unitPrice": {"currencyCode": "USD", "units": "0", "nanos": nanos},
                }],
            },
        }],
    })
}

/// An on-demand SKU offered in `region-one`.
fn here(description: &str, nanos: i64) -> Value {
    sku_json(description, "OnDemand", &["region-one"], nanos)
}

/// A SKU of `N1 Predefined Instance Core` in the region with the given `rates`.
fn core_sku(rates: Value) -> Value {
    let mut sku = here(CORE, 0);
    sku["pricingInfo"][0]["pricingExpression"]["tieredRates"] = rates;
    sku
}

fn skus_page(skus: Vec<Value>, next: Option<&str>) -> Value {
    let mut page = json!({"skus": skus});
    if let Some(token) = next {
        page["nextPageToken"] = json!(token);
    }
    page
}

fn usd(units: &str, nanos: i32) -> Money {
    Money {
        currency: "USD".to_owned(),
        units: units.to_owned(),
        nanos,
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

/// The prices a page of one N1 core SKU with `rates` is read into.
fn core_prices(rates: Value) -> Result<Vec<SkuPrice>, ApiError> {
    let page = skus_page(vec![core_sku(rates)], None);
    prices_of(vec![json_answer(&page)])
}

/// Asserts that `prices` is a walk over the Compute Engine service's SKUs in US dollars
/// with the largest page size, the second request carrying the first answer's
/// `nextPageToken`, and that the SKUs of both pages are read in order. It catches a walk
/// that stops at the first page and a request for another service or currency.
#[test]
fn prices_are_requested_in_dollars_and_walk_every_page() {
    let first = skus_page(vec![here(CORE, 31_611_000)], Some("page2"));
    let second = skus_page(vec![here(RAM, 4_237_000)], None);
    let (client, script) = over(vec![json_answer(&first), json_answer(&second)]);
    let prices = client.prices(&region()).unwrap();
    assert_eq!(
        prices,
        vec![
            SkuPrice {
                item: cores("n1"),
                usage: UsageType::OnDemand,
                unit_price: usd("0", 31_611_000),
            },
            SkuPrice {
                item: memory("n1"),
                usage: UsageType::OnDemand,
                unit_price: usd("0", 4_237_000),
            },
        ]
    );
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].method.as_str(), "GET");
    let (base, pairs) = split(&calls[0]);
    assert_eq!(base, SKUS_URL);
    assert_eq!(
        pairs,
        query(&[("currencyCode", "USD"), ("pageSize", "5000")])
    );
    assert_eq!(
        split(&calls[1]).1,
        query(&[
            ("currencyCode", "USD"),
            ("pageSize", "5000"),
            ("pageToken", "page2"),
        ])
    );
    assert_eq!(script.left(), 0);
}

/// Asserts that a SKU walk whose token repeats is incomplete, that an empty token ends the
/// walk, that a failing page fails the walk with its error, and that an answer with no
/// `skus` is unreadable. It catches a walk that returns a short price list.
#[test]
fn a_sku_walk_that_repeats_fails_or_ends_early_is_not_a_price_list() {
    let one = || vec![here(CORE, 1)];
    assert_eq!(
        prices_of(vec![
            json_answer(&skus_page(one(), Some("same"))),
            json_answer(&skus_page(one(), Some("same"))),
        ]),
        Err(ApiError::Incomplete)
    );
    let (client, script) = over(vec![json_answer(&skus_page(one(), Some("")))]);
    assert_eq!(client.prices(&region()).unwrap().len(), 1);
    assert_eq!(script.left(), 0);
    assert_eq!(
        prices_of(vec![
            json_answer(&skus_page(one(), Some("page2"))),
            status(403, &error_body(403, "PERMISSION_DENIED")),
        ]),
        Err(ApiError::answered("PERMISSION_DENIED"))
    );
    assert_eq!(
        prices_of(vec![json_answer(&json!({"nextPageToken": ""}))]),
        Err(ApiError::Unreadable("skus is missing".to_owned()))
    );
}

/// The descriptions of the SKU table, as the catalog words them, and what each prices.
fn table() -> Vec<(&'static str, PricedItem)> {
    vec![
        ("N1 Predefined Instance Core", cores("n1")),
        ("N1 Predefined Instance Ram", memory("n1")),
        ("A2 Instance Core", cores("a2")),
        ("A2 Instance Ram", memory("a2")),
        ("G4 Instance Core", cores("g4")),
        ("G4 Instance Ram", memory("g4")),
        ("Nvidia Tesla T4 GPU", gpu("nvidia-tesla-t4")),
        ("Nvidia Tesla P4 GPU", gpu("nvidia-tesla-p4")),
        ("Nvidia Tesla V100 GPU", gpu("nvidia-tesla-v100")),
        ("Nvidia Tesla A100 GPU", gpu("nvidia-tesla-a100")),
        ("Nvidia Tesla A100 80GB GPU", gpu("nvidia-a100-80gb")),
        ("Nvidia L4 GPU", gpu("nvidia-l4")),
        ("Nvidia H100 80GB GPU", gpu("nvidia-h100-80gb")),
        ("Nvidia H100 Mega 80GB GPU", gpu("nvidia-h100-mega-80gb")),
        ("Nvidia RTX PRO 6000 GPU", gpu("nvidia-rtx-pro-6000")),
        ("Nvidia B200 GPU", gpu("nvidia-b200")),
    ]
}

/// Asserts that every row of the SKU table is read, on demand as a plain description and
/// as Spot after the words "Spot Preemptible " (and the older "Preemptible "), each into
/// the item it prices with the usage type of its category and its own unit price. It
/// catches a row that does not read its SKU, a Spot lead-in not stripped, and a unit price
/// taken from another SKU.
#[test]
fn every_row_of_the_sku_table_reads_its_sku_on_demand_and_spot() {
    let mut skus = Vec::new();
    let mut expected = Vec::new();
    for (index, (words, item)) in table().into_iter().enumerate() {
        let nanos = i32::try_from(index).unwrap() + 1;
        let leads = [
            ("", "OnDemand", UsageType::OnDemand),
            ("Spot Preemptible ", "Preemptible", UsageType::Preemptible),
            ("Preemptible ", "Preemptible", UsageType::Preemptible),
        ];
        for (lead, category, usage) in leads {
            let description = format!("{lead}{words} running in Americas");
            skus.push(sku_json(
                &description,
                category,
                &["region-one"],
                i64::from(nanos),
            ));
            expected.push(SkuPrice {
                item: item.clone(),
                usage,
                unit_price: usd("0", nanos),
            });
        }
    }
    assert_eq!(
        prices_of(vec![json_answer(&skus_page(skus, None))]),
        Ok(expected)
    );
}

/// Asserts that a SKU is left out, never an error, when it is not offered in the region
/// (even if its price is unreadable), when its usage type is a commitment or is missing,
/// when it has no description, when no row reads its description (storage, a virtual
/// workstation accelerator, words that do not say where it runs), and when it lists no
/// regions; and that a SKU offered in several regions is kept. It catches a SKU of another
/// region priced as this one, a commitment priced as on demand, and an unread SKU that
/// fails the walk.
#[test]
fn a_sku_the_catalog_does_not_read_is_left_out() {
    let mut other_region = sku_json(
        "N1 Predefined Instance Core running in Europe",
        "OnDemand",
        &["region-two"],
        1,
    );
    other_region["pricingInfo"] = json!([]);
    let no_usage = without(sku_json(CORE, "x", &["region-one"], 3), "category");
    let no_description = without(here(CORE, 4), "description");
    let no_regions = without(here(CORE, 5), "serviceRegions");
    let skus = vec![
        other_region,
        sku_json(CORE, "Commit1Yr", &["region-one"], 2),
        sku_json(CORE, "Commit3Yr", &["region-one"], 2),
        no_usage,
        no_description,
        no_regions,
        here("Storage PD Capacity in Americas", 6),
        here("Storage PD Capacity running in Americas", 6),
        here(
            "Nvidia Tesla T4 Virtual Workstation GPU running in Americas",
            7,
        ),
        sku_json(RAM, "OnDemand", &["region-two", "region-one"], 8),
    ];
    assert_eq!(
        prices_of(vec![json_answer(&skus_page(skus, None))]),
        Ok(vec![SkuPrice {
            item: memory("n1"),
            usage: UsageType::OnDemand,
            unit_price: usd("0", 8),
        }])
    );
}

/// Asserts the shapes a unit price is written in: units as a string, units as a number,
/// units absent (zero), nanos absent (zero), a tier with no `startUsageAmount` (it starts
/// at zero), the tier that starts at zero chosen from several, and the first pricing of
/// several. It catches a price read from the wrong tier or pricing and a leaving-out of a
/// zero that is read as an error.
#[test]
fn a_unit_price_is_read_in_every_shape_the_catalog_writes_it() {
    let price = |units: Value, nanos: Option<i64>| {
        let mut unit = json!({"currencyCode": "USD"});
        if !units.is_null() {
            unit["units"] = units;
        }
        if let Some(nanos) = nanos {
            unit["nanos"] = json!(nanos);
        }
        json!([{"startUsageAmount": 0, "unitPrice": unit}])
    };
    let read = |rates: Value| core_prices(rates).unwrap()[0].unit_price.clone();
    assert_eq!(
        read(price(json!("2"), Some(500_000_000))),
        usd("2", 500_000_000)
    );
    assert_eq!(read(price(json!(3), Some(1))), usd("3", 1));
    assert_eq!(
        read(price(Value::Null, Some(31_611_000))),
        usd("0", 31_611_000)
    );
    assert_eq!(read(price(json!("2"), None)), usd("2", 0));
    assert_eq!(
        read(json!([{"unitPrice": {"currencyCode": "USD", "units": "5"}}])),
        usd("5", 0)
    );
    assert_eq!(
        read(json!([
            {"startUsageAmount": 100, "unitPrice": {"currencyCode": "USD", "units": "1"}},
            {"startUsageAmount": 0.0, "unitPrice": {"currencyCode": "USD", "units": "2"}},
        ])),
        usd("2", 0)
    );
    let mut sku = core_sku(price(json!("7"), None));
    sku["pricingInfo"]
        .as_array_mut()
        .unwrap()
        .push(json!({"pricingExpression": {"tieredRates": price(json!("9"), None)}}));
    let page = skus_page(vec![sku], None);
    let prices = prices_of(vec![json_answer(&page)]).unwrap();
    assert_eq!(prices[0].unit_price, usd("7", 0));
}

/// Asserts that a SKU the catalog reads whose price cannot be read is unreadable and says
/// why: no pricing, no rate that starts at zero (no rates, only a tier above zero, a start
/// that is not a number), a rate with no unit price, a unit price with no currency, units
/// that are not a whole number, and nanos that are not a 32-bit whole number. It catches
/// a price that defaults to zero.
#[test]
fn a_sku_with_an_unreadable_price_is_unreadable() {
    let unit = |price: Value| json!([{"startUsageAmount": 0, "unitPrice": price}]);
    let cases = [
        (json!(null), "a SKU has no tiered rates"),
        (
            json!([{"startUsageAmount": 100, "unitPrice": {"currencyCode": "USD"}}]),
            "a SKU has no rate that starts at zero",
        ),
        (
            json!([{"startUsageAmount": "zero", "unitPrice": {"currencyCode": "USD"}}]),
            "a SKU has no rate that starts at zero",
        ),
        (json!([{"startUsageAmount": 0}]), "a rate has no unit price"),
        (unit(json!({"units": "1"})), "a unit price has no currency"),
        (
            unit(json!({"currencyCode": "USD", "units": true})),
            "units of a unit price is not a whole number",
        ),
        (
            unit(json!({"currencyCode": "USD", "units": 1.5})),
            "units of a unit price is not a whole number",
        ),
        (
            unit(json!({"currencyCode": "USD", "nanos": "x"})),
            "nanos of a unit price is not a whole number",
        ),
        (
            unit(json!({"currencyCode": "USD", "nanos": 5_000_000_000u64})),
            "nanos of a unit price is not a whole number",
        ),
    ];
    for (rates, message) in cases {
        assert_eq!(
            core_prices(rates),
            Err(ApiError::Unreadable(message.to_owned())),
            "{message}"
        );
    }
}

/// Asserts that the catalog over the client reads machine types, accelerator types,
/// prices and quotas in that order, each as the request its call makes, and returns the
/// priced offer: an A2 type with one A100 at 12 vCPUs and 85 GiB, priced from the three
/// SKUs at whole cents rounded up (12 x 0.10 + 85 x 0.01 + 1.00 = 3.05 dollars an hour),
/// with no note and no answer left unread. It catches a call whose answer the catalog
/// cannot use and a price that does not reach the offer.
#[test]
fn the_catalog_over_the_client_returns_priced_offers() {
    let zone_name = "region-one-a";
    let machines = listing(
        json!({"zones/region-one-a": scope("machineTypes", vec![a2_json(zone_name)])}),
        None,
    );
    let accelerators = listing(
        json!({"zones/region-one-a": scope("acceleratorTypes", vec![
            accelerator_json("nvidia-tesla-a100", zone_name, 16),
        ])}),
        None,
    );
    let prices = skus_page(
        vec![
            here("A2 Instance Core running in Americas", 100_000_000),
            here("A2 Instance Ram running in Americas", 10_000_000),
            {
                let mut a100 = here("Nvidia Tesla A100 GPU running in Americas", 0);
                a100["pricingInfo"][0]["pricingExpression"]["tieredRates"][0]["unitPrice"] =
                    json!({"currencyCode": "USD", "units": "1"});
                a100
            },
            sku_json(
                "A2 Instance Core running in Americas",
                "Commit1Yr",
                &["region-one"],
                1,
            ),
        ],
        None,
    );
    let quotas = json!({"quotas": [{"metric": "CPUS", "limit": 100, "usage": 0}]});
    let (client, script) = over(vec![
        json_answer(&machines),
        json_answer(&accelerators),
        json_answer(&prices),
        json_answer(&quotas),
    ]);
    let settings = GceCatalogSettings::new(PROJECT, &[zone_name], &["on-demand"], None).unwrap();
    let answer = GceCatalog::new(Arc::new(client), settings)
        .catalog()
        .unwrap();
    assert!(answer.notes.is_empty());
    assert_eq!(answer.offers.len(), 1);
    let offer = &answer.offers[0];
    assert_eq!(offer.id, "a2-highgpu-1g/on-demand");
    assert_eq!(offer.zone.as_deref(), Some(zone_name));
    assert_eq!(offer.arch, "x86_64");
    assert_eq!(offer.price_cents_per_hour, Some(305));
    let bases: Vec<String> = api_calls(&script)
        .iter()
        .map(|request| split(request).0.to_owned())
        .collect();
    assert_eq!(
        bases,
        [
            format!("{COMPUTE}/aggregated/machineTypes"),
            format!("{COMPUTE}/aggregated/acceleratorTypes"),
            SKUS_URL.to_owned(),
            format!("{COMPUTE}/regions/region-one"),
        ]
    );
    assert_eq!(script.left(), 0);
}
