//! The reads of the catalog: machine types and accelerator types by zone, the quotas of a
//! region and the Compute Engine SKUs with their prices. Each answer is read into the
//! records of [`crate::model`].

use serde_json::Value;

use super::{
    BILLING_BASE, COMPUTE_BASE, COMPUTE_ENGINE_SERVICE, HttpGce, last_segment, pages, skus,
    unreadable,
};
use crate::error::ApiError;
use crate::model::{
    AcceleratorTypeInfo, AcceleratorTypeName, ArchName, AttachedAccelerator, MachineTypeInfo,
    MachineTypeName, Project, QuotaMetric, Region, SkuPrice, Zone,
};

/// The page size asked of an aggregated list (its largest).
const AGGREGATED_PAGE: &str = "500";

/// The page size asked of `services.skus.list` (its largest).
const SKUS_PAGE: &str = "5000";

/// The largest whole number a JSON number holds exactly (2^53).
const EXACT: f64 = 9_007_199_254_740_992.0;

impl HttpGce {
    /// `machineTypes.aggregatedList`, every page, for the machine types of `zones`.
    pub(super) fn read_machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError> {
        self.aggregated(project, zones, "machineTypes", machine_type)
    }

    /// `acceleratorTypes.aggregatedList`, every page, for the accelerator types of `zones`.
    pub(super) fn read_accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
        self.aggregated(project, zones, "acceleratorTypes", accelerator_type)
    }

    /// `services.skus.list` of the Compute Engine service in US dollars, every page, kept
    /// as [`skus::read`] keeps a SKU of `region`.
    pub(super) fn read_prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError> {
        let url = format!("{BILLING_BASE}/{COMPUTE_ENGINE_SERVICE}/skus");
        pages(|token| {
            let mut query = vec![
                ("currencyCode", "USD".to_owned()),
                ("pageSize", SKUS_PAGE.to_owned()),
            ];
            query.extend(token.map(|token| ("pageToken", token.to_owned())));
            let answer = self.get_json(&url, &query)?;
            let list = answer
                .get("skus")
                .and_then(Value::as_array)
                .ok_or_else(|| unreadable("skus is missing"))?;
            let rows: Vec<Option<SkuPrice>> = list
                .iter()
                .map(|sku| skus::read(sku, region))
                .collect::<Result<_, _>>()?;
            Ok((rows.into_iter().flatten().collect(), next_token(&answer)))
        })
    }

    /// `regions.get`, the `quotas` of `region`.
    pub(super) fn read_region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError> {
        let url = format!("{COMPUTE_BASE}/projects/{project}/regions/{region}");
        let answer = self.get_json(&url, &[])?;
        answer
            .get("quotas")
            .and_then(Value::as_array)
            .ok_or_else(|| unreadable("quotas is missing"))?
            .iter()
            .map(quota)
            .collect()
    }

    /// An aggregated list of `kind` (`machineTypes` or `acceleratorTypes`), every page,
    /// asking for the partial answer the cloud gives when a scope cannot be reached. The
    /// scope of each zone of `zones` holds a list named `kind`, or a warning in its place
    /// (the zone has none); `item` reads each element. Scopes of other zones are not read.
    /// An answer that names an unreachable scope is [`ApiError::Incomplete`].
    fn aggregated<T>(
        &self,
        project: &Project,
        zones: &[Zone],
        kind: &str,
        item: fn(&Value) -> Result<T, ApiError>,
    ) -> Result<Vec<T>, ApiError> {
        let url = format!("{COMPUTE_BASE}/projects/{project}/aggregated/{kind}");
        pages(|token| {
            let mut query = vec![
                ("returnPartialSuccess", "true".to_owned()),
                ("maxResults", AGGREGATED_PAGE.to_owned()),
            ];
            query.extend(token.map(|token| ("pageToken", token.to_owned())));
            let answer = self.get_json(&url, &query)?;
            let partial = answer
                .get("unreachables")
                .and_then(Value::as_array)
                .is_some_and(|scopes| !scopes.is_empty());
            if partial {
                return Err(ApiError::Incomplete);
            }
            let scopes = answer
                .get("items")
                .and_then(Value::as_object)
                .ok_or_else(|| unreadable("items is missing"))?;
            let mut rows = Vec::new();
            for (scope, entry) in scopes {
                let asked = scope
                    .strip_prefix("zones/")
                    .is_some_and(|name| zones.iter().any(|zone| zone.as_str() == name));
                if !asked {
                    continue;
                }
                if let Some(list) = entry.get(kind) {
                    let elements = list
                        .as_array()
                        .ok_or_else(|| unreadable(&format!("{kind} is not a list")))?;
                    for element in elements {
                        rows.push(item(element)?);
                    }
                }
            }
            Ok((rows, next_token(&answer)))
        })
    }
}

/// The token of the next page of an answer, when it names one (an empty token means none).
fn next_token(answer: &Value) -> Option<String> {
    answer
        .get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// The member `name` of `item`.
fn field<'a>(item: &'a Value, name: &str) -> Result<&'a Value, ApiError> {
    item.get(name)
        .ok_or_else(|| unreadable(&format!("{name} is missing")))
}

/// The text member `name` of `item`.
fn text<'a>(item: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    field(item, name)?
        .as_str()
        .ok_or_else(|| unreadable(&format!("{name} is not text")))
}

/// The whole-number member `name` of `item`, as a count that fits 32 bits.
fn count(item: &Value, name: &str) -> Result<u32, ApiError> {
    field(item, name)?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| unreadable(&format!("{name} is not a count")))
}

/// The zone the member `zone` of `item` names, as a name or a URL.
fn zone_of(item: &Value) -> Result<Zone, ApiError> {
    Zone::new(last_segment(text(item, "zone")?)).map_err(|_| unreadable("zone is not a zone name"))
}

/// The machine type an element of `machineTypes` describes.
fn machine_type(item: &Value) -> Result<MachineTypeInfo, ApiError> {
    Ok(MachineTypeInfo {
        name: MachineTypeName(text(item, "name")?.to_owned()),
        zone: zone_of(item)?,
        vcpus: count(item, "guestCpus")?,
        memory_mib: field(item, "memoryMb")?
            .as_u64()
            .ok_or_else(|| unreadable("memoryMb is not a count"))?,
        accelerators: attached(item)?,
        arch: item
            .get("architecture")
            .map(|arch| {
                arch.as_str()
                    .map(|name| ArchName(name.to_owned()))
                    .ok_or_else(|| unreadable("architecture is not text"))
            })
            .transpose()?,
    })
}

/// The accelerators built into the machine type `item`; none when it lists none.
fn attached(item: &Value) -> Result<Vec<AttachedAccelerator>, ApiError> {
    match item.get("accelerators") {
        None => Ok(Vec::new()),
        Some(list) => list
            .as_array()
            .ok_or_else(|| unreadable("accelerators is not a list"))?
            .iter()
            .map(built_in)
            .collect(),
    }
}

/// The accelerator an element of a machine type's `accelerators` describes.
fn built_in(item: &Value) -> Result<AttachedAccelerator, ApiError> {
    Ok(AttachedAccelerator {
        type_name: AcceleratorTypeName(text(item, "guestAcceleratorType")?.to_owned()),
        count: count(item, "guestAcceleratorCount")?,
    })
}

/// The accelerator type an element of `acceleratorTypes` describes.
fn accelerator_type(item: &Value) -> Result<AcceleratorTypeInfo, ApiError> {
    Ok(AcceleratorTypeInfo {
        name: AcceleratorTypeName(text(item, "name")?.to_owned()),
        zone: zone_of(item)?,
        max_per_instance: count(item, "maximumCardsPerInstance")?,
    })
}

/// The quota an element of a region's `quotas` describes.
fn quota(item: &Value) -> Result<QuotaMetric, ApiError> {
    Ok(QuotaMetric {
        metric: text(item, "metric")?.to_owned(),
        limit: whole(field(item, "limit")?, "limit")?,
        usage: item
            .get("usage")
            .map(|usage| whole(usage, "usage"))
            .transpose()?,
    })
}

/// The number `value` as a whole number that is not negative. A quota is a JSON number
/// that may be written with a fraction of zero (`24.0`).
fn whole(value: &Value, name: &str) -> Result<u64, ApiError> {
    value
        .as_f64()
        .filter(|number| number.fract() == 0.0 && (0.0..=EXACT).contains(number))
        .map(|number| number as u64)
        .ok_or_else(|| {
            unreadable(&format!(
                "{name} is not a whole number that is not negative"
            ))
        })
}
