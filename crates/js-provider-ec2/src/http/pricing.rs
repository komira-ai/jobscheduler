//! The two JSON services beside EC2: the Pricing API's `GetProducts`, for the on-demand
//! price of every instance type, and Service Quotas' `GetServiceQuota`, for the vCPU
//! quota of a family of instance types.

use serde_json::{Value, json};

use super::xml::unreadable;
use super::{HttpEc2, PRICING_REGION, Target, pages};
use crate::api::PurchaseKind;
use crate::error::ApiError;
use crate::model::{Region, TypePrice, VcpuQuota};
use crate::quota::QuotaFamily;

/// The operation that lists products.
const GET_PRODUCTS: &str = "AWSPriceListService.GetProducts";

/// The operation that reads one quota, as Service Quotas names it in `x-amz-target` (its
/// version is a protocol constant of the cloud).
pub const QUOTAS_TARGET: &str = "ServiceQuotasV20190624.GetServiceQuota";

/// The page size asked of `GetProducts`.
const PRODUCTS_PAGE: u32 = 100;

/// The error code Service Quotas answers for a quota the account has no entry for.
const NO_QUOTA: &str = "NoSuchResourceException";

impl HttpEc2 {
    /// `GetProducts` for Linux, shared tenancy, no pre-installed software, every page.
    pub(super) fn read_on_demand_prices(
        &self,
        region: &Region,
    ) -> Result<Vec<TypePrice>, ApiError> {
        let target = Target {
            host: format!("api.pricing.{PRICING_REGION}.amazonaws.com"),
            region: PRICING_REGION,
            service: "pricing",
        };
        let filters = [
            ("regionCode", region.as_str()),
            ("operatingSystem", "Linux"),
            ("tenancy", "Shared"),
            ("preInstalledSw", "NA"),
            ("capacitystatus", "Used"),
        ]
        .map(|(field, value)| json!({"Type": "TERM_MATCH", "Field": field, "Value": value}));
        pages(|token| {
            let mut body = json!({
                "ServiceCode": "AmazonEC2",
                "FormatVersion": "aws_v1",
                "MaxResults": PRODUCTS_PAGE,
                "Filters": filters,
            });
            token
                .into_iter()
                .for_each(|token| body["NextToken"] = Value::from(token));
            let answer = self.json(&target, GET_PRODUCTS, &body)?;
            let list = answer
                .get("PriceList")
                .and_then(Value::as_array)
                .ok_or_else(|| unreadable("PriceList is missing"))?;
            let rows: Vec<Option<TypePrice>> =
                list.iter().map(product).collect::<Result<_, _>>()?;
            Ok((rows.into_iter().flatten().collect(), next_token(&answer)))
        })
    }

    /// `GetServiceQuota` of `family` for `purchase`. A purchase option with no quota in
    /// the table (a Capacity Block, or spot for a family that lists none) asks nothing and
    /// has none, and so does a quota the account has no entry for.
    pub(super) fn read_vcpu_quota(
        &self,
        region: &Region,
        family: QuotaFamily,
        purchase: PurchaseKind,
    ) -> Result<Option<VcpuQuota>, ApiError> {
        quota_code(family, purchase)
            .map(|code| self.quota(region, code))
            .transpose()
            .map(Option::flatten)
    }

    /// The quota `code` of EC2 in `region`.
    fn quota(&self, region: &Region, code: &str) -> Result<Option<VcpuQuota>, ApiError> {
        let target = Target {
            host: format!("servicequotas.{region}.amazonaws.com"),
            region: region.as_str(),
            service: "servicequotas",
        };
        let body = json!({"ServiceCode": "ec2", "QuotaCode": code});
        match self.json(&target, QUOTAS_TARGET, &body) {
            Ok(answer) => limit(&answer).map(Some),
            Err(error) if error.code().is_some_and(|code| code.as_str() == NO_QUOTA) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// The code of the quota of `family` for `purchase`, when the cloud lists one: the running
/// on-demand instances, and the spot instance requests where the cloud lists a quota for
/// them (Amazon EC2 instance type quotas, in the EC2 instance types guide).
fn quota_code(family: QuotaFamily, purchase: PurchaseKind) -> Option<&'static str> {
    let (on_demand, spot) = match family {
        QuotaFamily::Standard => ("L-1216C47A", Some("L-34B43A08")),
        QuotaFamily::Dl => ("L-6E869C2A", Some("L-85EED4F7")),
        QuotaFamily::F => ("L-74FC7D96", Some("L-88CF9481")),
        QuotaFamily::GAndVt => ("L-DB2E81BA", Some("L-3819A6DF")),
        QuotaFamily::Hpc => ("L-F7808C92", None),
        QuotaFamily::HighMemory => ("L-43DA4232", None),
        QuotaFamily::Inf => ("L-1945791B", Some("L-B5D1601B")),
        QuotaFamily::P => ("L-417A185B", Some("L-7212CCBC")),
        QuotaFamily::Trn => ("L-2C3B7624", Some("L-6B0D517C")),
        QuotaFamily::X => ("L-7295265B", Some("L-E3A00192")),
    };
    match purchase {
        PurchaseKind::OnDemand => Some(on_demand),
        PurchaseKind::Spot => spot,
        PurchaseKind::CapacityBlock => None,
    }
}

/// The limit `Quota.Value` of a `GetServiceQuota` answer.
fn limit(answer: &Value) -> Result<VcpuQuota, ApiError> {
    answer
        .pointer("/Quota/Value")
        .and_then(Value::as_f64)
        .filter(|value| (0.0..=f64::from(u32::MAX)).contains(value))
        .map(|value| VcpuQuota {
            limit: value as u32,
            in_use: None,
        })
        .ok_or_else(|| unreadable("Quota.Value is not a vCPU count"))
}

/// The token of the next page of a `GetProducts` answer.
fn next_token(answer: &Value) -> Option<String> {
    answer
        .get("NextToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// The price of the product a `PriceList` entry holds (the entry is a JSON document in a
/// string), or none when the product states no hourly on-demand price.
fn product(entry: &Value) -> Result<Option<TypePrice>, ApiError> {
    let text = entry
        .as_str()
        .ok_or_else(|| unreadable("a price list entry is not text"))?;
    let product: Value =
        serde_json::from_str(text).map_err(|_| unreadable("a price list entry is not JSON"))?;
    let name = product
        .pointer("/product/attributes/instanceType")
        .and_then(Value::as_str)
        .ok_or_else(|| unreadable("a product has no instance type"))?;
    Ok(hourly_usd(&product).map(|usd_per_hour| TypePrice {
        instance_type: crate::model::InstanceType(name.to_owned()),
        usd_per_hour,
    }))
}

/// `pricePerUnit.USD` of the hourly (`Hrs`) price dimension of an on-demand term.
fn hourly_usd(product: &Value) -> Option<String> {
    product
        .pointer("/terms/OnDemand")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|offers| offers.values())
        .filter_map(|offer| offer.get("priceDimensions").and_then(Value::as_object))
        .flat_map(|dimensions| dimensions.values())
        .find(|dimension| dimension.get("unit").and_then(Value::as_str) == Some("Hrs"))
        .and_then(|dimension| dimension.pointer("/pricePerUnit/USD"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}
