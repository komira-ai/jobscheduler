//! The two JSON services: on-demand prices from the Pricing API and the vCPU quota from
//! Service Quotas.

use js_provider_ec2::{
    ApiError, Ec2Api, InstanceType, PurchaseKind, QUOTAS_TARGET, QuotaFamily, TypePrice, VcpuQuota,
};
use serde_json::{Value, json};

use crate::script::{KEY_ID, answer, json as body_of, region, rig, signed_headers, status};

/// A product as the Pricing API writes it: a JSON document inside a string.
fn product(instance_type: &str, unit: &str, usd: &str) -> Value {
    let document = json!({
        "product": {"attributes": {"instanceType": instance_type, "regionCode": "region-a"}},
        "terms": {"OnDemand": {"OFFER.TERM": {"priceDimensions": {"OFFER.TERM.DIM": {
            "unit": unit, "pricePerUnit": {"USD": usd}}}}}},
    });
    Value::String(document.to_string())
}

fn products(list: &[Value], next: Option<&str>) -> String {
    let mut page = json!({"FormatVersion": "aws_v1", "PriceList": list});
    if let Some(token) = next {
        page["NextToken"] = json!(token);
    }
    page.to_string()
}

/// Asserts that `on_demand_prices` is one signed JSON POST to the Pricing endpoint in
/// `us-east-1` (whatever the client's region), signed for `pricing` with the operation in
/// `x-amz-target`, filtering on the region asked, Linux, shared tenancy, no pre-installed
/// software and used capacity; and that over two pages it reads each product's type and
/// hourly price exactly as written, leaving out a product with no hourly price. It catches
/// a request in the wrong region or with a filter off, and a price taken from a monthly
/// dimension.
#[test]
fn on_demand_prices_are_requested_in_the_pricing_region_and_read_as_written() {
    let monthly = product("t3.nano", "Mo", "9.9");
    let no_terms =
        Value::String(json!({"product": {"attributes": {"instanceType": "t3.micro"}}}).to_string());
    let (client, script) = rig(vec![
        answer(&products(
            &[product("p5.48xlarge", "Hrs", "55.0400000000"), monthly],
            Some("page-two"),
        )),
        answer(&products(
            &[product("m7g.metal", "Hrs", "2.6100000000"), no_terms],
            Some(""),
        )),
    ]);
    assert_eq!(
        client.on_demand_prices(&region("region-b")).unwrap(),
        vec![
            TypePrice {
                instance_type: InstanceType("p5.48xlarge".to_owned()),
                usd_per_hour: "55.0400000000".to_owned(),
            },
            TypePrice {
                instance_type: InstanceType("m7g.metal".to_owned()),
                usd_per_hour: "2.6100000000".to_owned(),
            },
        ]
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    let request = &sent[0];
    assert_eq!(request.url, "https://api.pricing.us-east-1.amazonaws.com/");
    assert_eq!(
        request.header("host"),
        Some("api.pricing.us-east-1.amazonaws.com")
    );
    assert_eq!(
        request.header("content-type"),
        Some("application/x-amz-json-1.1")
    );
    assert_eq!(
        request.header("x-amz-target"),
        Some("AWSPriceListService.GetProducts")
    );
    assert!(
        request
            .header("authorization")
            .unwrap()
            .starts_with(&format!(
                "AWS4-HMAC-SHA256 Credential={KEY_ID}/20261105/us-east-1/pricing/aws4_request"
            ))
    );
    assert_eq!(
        signed_headers(request),
        [
            "content-type",
            "host",
            "x-amz-date",
            "x-amz-security-token",
            "x-amz-target"
        ]
    );
    let filters: Vec<(String, String)> = body_of(request)["Filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|filter| {
            assert_eq!(filter["Type"], "TERM_MATCH");
            (
                filter["Field"].as_str().unwrap().to_owned(),
                filter["Value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected = [
        ("regionCode", "region-b"),
        ("operatingSystem", "Linux"),
        ("tenancy", "Shared"),
        ("preInstalledSw", "NA"),
        ("capacitystatus", "Used"),
    ]
    .map(|(field, value)| (field.to_owned(), value.to_owned()));
    assert_eq!(filters, expected);
    assert_eq!(body_of(request)["ServiceCode"], "AmazonEC2");
    assert_eq!(body_of(request).get("NextToken"), None);
    assert_eq!(body_of(&sent[1])["NextToken"], "page-two");
}

/// Asserts that a price answer with no list, an entry that is not text, an entry that is
/// not JSON, a product with no instance type, or a body that is not JSON is unreadable and
/// says which. It catches a price list read as empty.
#[test]
fn a_bad_price_answer_is_unreadable() {
    let cases = [
        (
            json!({"FormatVersion": "aws_v1"}).to_string(),
            "PriceList is missing",
        ),
        (
            json!({"PriceList": [7]}).to_string(),
            "a price list entry is not text",
        ),
        (
            json!({"PriceList": ["{"]}).to_string(),
            "a price list entry is not JSON",
        ),
        (
            json!({"PriceList": ["{\"product\":{}}"]}).to_string(),
            "a product has no instance type",
        ),
        ("not json".to_owned(), "the answer is not JSON"),
    ];
    for (body, what) in cases {
        let (client, _) = rig(vec![answer(&body)]);
        assert_eq!(
            client.on_demand_prices(&region("region-a")),
            Err(ApiError::Unreadable(what.to_owned())),
            "{what}"
        );
    }
}

/// Asserts that a JSON error answer is the cloud's code taken after the `#` of `__type`
/// (or the whole of it when there is none), throttles and denials are told as such, and an
/// answer with no `__type` or no JSON is unreadable with its status. It catches a code
/// that keeps the namespace in front.
#[test]
fn a_json_error_answer_is_its_type_code() {
    let cases = [
        (
            400,
            json!({"__type": "com.amazonaws.pricing#ThrottlingException", "message": "slow"}),
            ApiError::answered("ThrottlingException"),
        ),
        (
            400,
            json!({"__type": "AccessDeniedException"}),
            ApiError::answered("AccessDeniedException"),
        ),
        (
            400,
            json!({"message": "no type"}),
            ApiError::Unreadable("an error answer with no code (HTTP 400)".to_owned()),
        ),
    ];
    for (code, body, expected) in cases {
        let (client, _) = rig(vec![status(code, &body.to_string())]);
        assert_eq!(client.on_demand_prices(&region("region-a")), Err(expected));
    }
    let (client, _) = rig(vec![status(502, "<html>bad gateway</html>")]);
    assert_eq!(
        client.on_demand_prices(&region("region-a")),
        Err(ApiError::Unreadable(
            "an error answer with no code (HTTP 502)".to_owned()
        ))
    );
}

/// Asserts that `vcpu_quota` asks Service Quotas of the region for the EC2 quota code of
/// every family for on-demand, and for spot where the table lists one, and reads the limit
/// from `Quota.Value` with no usage; that the host, scope and operation are Service
/// Quotas'; and that a family with no spot quota, and a Capacity Block, ask nothing and
/// have none. It catches a code swapped between two families or purchase options.
#[test]
fn each_family_and_purchase_asks_its_own_quota_code() {
    let on_demand = [
        (QuotaFamily::Standard, "L-1216C47A"),
        (QuotaFamily::Dl, "L-6E869C2A"),
        (QuotaFamily::F, "L-74FC7D96"),
        (QuotaFamily::GAndVt, "L-DB2E81BA"),
        (QuotaFamily::Hpc, "L-F7808C92"),
        (QuotaFamily::HighMemory, "L-43DA4232"),
        (QuotaFamily::Inf, "L-1945791B"),
        (QuotaFamily::P, "L-417A185B"),
        (QuotaFamily::Trn, "L-2C3B7624"),
        (QuotaFamily::X, "L-7295265B"),
    ];
    let spot = [
        (QuotaFamily::Standard, Some("L-34B43A08")),
        (QuotaFamily::Dl, Some("L-85EED4F7")),
        (QuotaFamily::F, Some("L-88CF9481")),
        (QuotaFamily::GAndVt, Some("L-3819A6DF")),
        (QuotaFamily::Hpc, None),
        (QuotaFamily::HighMemory, None),
        (QuotaFamily::Inf, Some("L-B5D1601B")),
        (QuotaFamily::P, Some("L-7212CCBC")),
        (QuotaFamily::Trn, Some("L-6B0D517C")),
        (QuotaFamily::X, Some("L-E3A00192")),
    ];
    let value = json!({"Quota": {"QuotaCode": "x", "Value": 1152.0}}).to_string();
    let limit = Some(VcpuQuota {
        limit: 1152,
        in_use: None,
    });
    let (client, script) = rig(vec![answer(&value); 30]);
    let mut asked = Vec::new();
    for (family, code) in on_demand {
        assert_eq!(
            client.vcpu_quota(&region("region-b"), family, PurchaseKind::OnDemand),
            Ok(limit)
        );
        asked.push(code);
    }
    for (family, code) in spot {
        let got = client.vcpu_quota(&region("region-b"), family, PurchaseKind::Spot);
        assert_eq!(got, Ok(code.and(limit)), "{family:?}");
        asked.extend(code);
    }
    assert_eq!(
        client.vcpu_quota(
            &region("region-b"),
            QuotaFamily::P,
            PurchaseKind::CapacityBlock
        ),
        Ok(None)
    );
    let sent = script.sent();
    let codes: Vec<String> = sent
        .iter()
        .map(|request| body_of(request)["QuotaCode"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(codes, asked);
    let request = &sent[0];
    assert_eq!(request.url, "https://servicequotas.region-b.amazonaws.com/");
    assert_eq!(request.header("x-amz-target"), Some(QUOTAS_TARGET));
    assert_eq!(body_of(request)["ServiceCode"], "ec2");
    assert!(
        request
            .header("authorization")
            .unwrap()
            .contains("/20261105/region-b/servicequotas/aws4_request")
    );
}

/// Asserts that an account with no quota entry (`NoSuchResourceException`) has none, that
/// another error is the error, and that a value that is missing, negative or beyond what a
/// count holds is unreadable. It catches a missing quota that fails the whole catalog and a
/// negative limit cast to a huge one.
#[test]
fn a_missing_quota_is_none_and_a_bad_value_is_unreadable() {
    let ask = |reply| {
        let (client, _) = rig(vec![reply]);
        client.vcpu_quota(&region("region-a"), QuotaFamily::P, PurchaseKind::Spot)
    };
    assert_eq!(
        ask(status(
            400,
            &json!({"__type": "NoSuchResourceException"}).to_string()
        )),
        Ok(None)
    );
    assert_eq!(
        ask(status(
            400,
            &json!({"__type": "AccessDeniedException"}).to_string()
        )),
        Err(ApiError::answered("AccessDeniedException"))
    );
    for value in [
        json!({}),
        json!({"Quota": {"Value": -1.0}}),
        json!({"Quota": {"Value": 1.0e12}}),
        json!({"Quota": {"Value": "many"}}),
    ] {
        assert_eq!(
            ask(answer(&value.to_string())),
            Err(ApiError::Unreadable(
                "Quota.Value is not a vCPU count".to_owned()
            )),
            "{value}"
        );
    }
}
