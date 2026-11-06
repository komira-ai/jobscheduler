//! The EC2 reads of the catalog: instance types, the zones that offer them, spot prices
//! and Capacity Block offerings. Each answer is read into the records of
//! [`crate::model`].

use std::time::SystemTime;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::xml::{Node, unreadable};
use super::{Form, HttpEc2};
use crate::error::ApiError;
use crate::model::{
    ArchName, BlockOffering, GpuInfo, InstanceType, InstanceTypeInfo, Region, Zone, ZoneOffering,
    ZonePrice,
};

/// The page size asked of `DescribeInstanceTypes` (its largest).
const TYPES_PAGE: &str = "100";

/// The page size asked of `DescribeInstanceTypeOfferings` and `DescribeSpotPriceHistory`.
const LARGE_PAGE: &str = "1000";

/// The page size asked of `DescribeCapacityBlockOfferings`.
const BLOCKS_PAGE: &str = "100";

impl HttpEc2 {
    /// `DescribeInstanceTypes`, every page.
    pub(super) fn read_instance_types(
        &self,
        region: &Region,
    ) -> Result<Vec<InstanceTypeInfo>, ApiError> {
        let form = Form::new("DescribeInstanceTypes").with("MaxResults", TYPES_PAGE);
        self.listing(region.as_str(), &form, |answer| {
            answer.items("instanceTypeSet").map(type_info).collect()
        })
    }

    /// `DescribeInstanceTypeOfferings` by availability zone, every page.
    pub(super) fn read_zone_offerings(
        &self,
        region: &Region,
    ) -> Result<Vec<ZoneOffering>, ApiError> {
        let form = Form::new("DescribeInstanceTypeOfferings")
            .with("LocationType", "availability-zone")
            .with("MaxResults", LARGE_PAGE);
        self.listing(region.as_str(), &form, |answer| {
            answer
                .items("instanceTypeOfferingSet")
                .map(zone_offering)
                .collect()
        })
    }

    /// `DescribeSpotPriceHistory` for `Linux/UNIX` from now, every page.
    pub(super) fn read_spot_prices(&self, region: &Region) -> Result<Vec<ZonePrice>, ApiError> {
        let form = Form::new("DescribeSpotPriceHistory")
            .with("ProductDescription.1", "Linux/UNIX")
            .with("StartTime", stamp(self.wall.now()))
            .with("MaxResults", LARGE_PAGE);
        self.listing(region.as_str(), &form, |answer| {
            answer
                .items("spotPriceHistorySet")
                .map(zone_price)
                .collect()
        })
    }

    /// `DescribeCapacityBlockOfferings` for blocks of `hours`, every page.
    pub(super) fn read_capacity_blocks(
        &self,
        region: &Region,
        hours: u32,
    ) -> Result<Vec<BlockOffering>, ApiError> {
        let form = Form::new("DescribeCapacityBlockOfferings")
            .with("CapacityDurationHours", hours.to_string())
            .with("MaxResults", BLOCKS_PAGE);
        self.listing(region.as_str(), &form, |answer| {
            answer
                .items("capacityBlockOfferingSet")
                .map(block_offering)
                .collect()
        })
    }
}

/// `now` as the cloud takes a time in a request: `YYYY-MM-DDTHH:MM:SSZ`, in UTC.
fn stamp(now: SystemTime) -> String {
    let at = OffsetDateTime::from(now);
    let (year, month, day) = at.to_calendar_date();
    let (hour, minute, second) = at.to_hms();
    format!(
        "{year:04}-{:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z",
        u8::from(month)
    )
}

/// The instance type an `item` of `instanceTypeSet` describes.
fn type_info(item: &Node) -> Result<InstanceTypeInfo, ApiError> {
    let storage = "instanceStorageInfo/totalSizeInGB";
    Ok(InstanceTypeInfo {
        name: InstanceType(item.required("instanceType")?.to_owned()),
        vcpus: item.number("vCpuInfo/defaultVCpus")?,
        memory_mib: item.number("memoryInfo/sizeInMiB")?,
        architectures: item
            .items("processorInfo/supportedArchitectures")
            .map(|arch| ArchName(arch.text().to_owned()))
            .collect(),
        gpus: item
            .items("gpuInfo/gpus")
            .map(gpu)
            .collect::<Result<_, _>>()?,
        instance_storage_gb: item.at(storage).map(|_| item.number(storage)).transpose()?,
        bare_metal: item.text_at("bareMetal") == Some("true"),
    })
}

/// The GPU an `item` of `gpuInfo/gpus` describes.
fn gpu(item: &Node) -> Result<GpuInfo, ApiError> {
    Ok(GpuInfo {
        name: item.required("name")?.to_owned(),
        manufacturer: item.required("manufacturer")?.to_owned(),
        count: item.number("count")?,
        memory_mib: item.number("memoryInfo/sizeInMiB")?,
    })
}

/// The pair an `item` of `instanceTypeOfferingSet` names.
fn zone_offering(item: &Node) -> Result<ZoneOffering, ApiError> {
    Ok(ZoneOffering {
        instance_type: InstanceType(item.required("instanceType")?.to_owned()),
        zone: Zone(item.required("location")?.to_owned()),
    })
}

/// The price an `item` of `spotPriceHistorySet` states.
fn zone_price(item: &Node) -> Result<ZonePrice, ApiError> {
    Ok(ZonePrice {
        instance_type: InstanceType(item.required("instanceType")?.to_owned()),
        zone: Zone(item.required("availabilityZone")?.to_owned()),
        usd_per_hour: item.required("spotPrice")?.to_owned(),
    })
}

/// The offering an `item` of `capacityBlockOfferingSet` describes. Its length is the
/// hours and the minutes it states (the minutes may be absent).
fn block_offering(item: &Node) -> Result<BlockOffering, ApiError> {
    let minutes = "capacityBlockDurationMinutes";
    let hours: u64 = item.number("capacityBlockDurationHours")?;
    let extra: u64 = item
        .at(minutes)
        .map(|_| item.number(minutes))
        .transpose()?
        .unwrap_or(0);
    Ok(BlockOffering {
        id: item.required("capacityBlockOfferingId")?.to_owned(),
        instance_type: InstanceType(item.required("instanceType")?.to_owned()),
        zone: Zone(item.required("availabilityZone")?.to_owned()),
        instance_count: item.number("instanceCount")?,
        start_unix_seconds: unix_seconds(item.required("startDate")?, "startDate")?,
        duration_minutes: hours * 60 + extra,
        upfront_fee: item.required("upfrontFee")?.to_owned(),
        currency: item.required("currencyCode")?.to_owned(),
    })
}

/// The seconds since the Unix epoch of the RFC 3339 time `text`, which the element `field`
/// holds.
pub(super) fn unix_seconds(text: &str, field: &str) -> Result<u64, ApiError> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .and_then(|at| u64::try_from(at.unix_timestamp()).ok())
        .ok_or_else(|| unreadable(format!("{field} is not a time since the epoch")))
}
