//! The launch side over EC2: `RunInstances`, `DescribeInstances`, `TerminateInstances`,
//! `PurchaseCapacityBlock` and `DescribeCapacityReservations`. The cloud's XML is read into
//! the records of [`crate::launch`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::catalog_calls::unix_seconds;
use super::xml::{Node, unreadable};
use super::{Form, HttpEc2};
use crate::error::ApiError;
use crate::launch::{
    BlockPurchase, CapacityReservation, CapacityReservationId, DescribeFilter, ImageId, Instance,
    InstanceId, InstanceState, LaunchPurchase, ReservationState, RunRequest, Tag,
};
use crate::model::{InstanceType, Zone};

/// The page size asked of `DescribeInstances` by tag (it may not be given with ids).
const INSTANCES_PAGE: &str = "500";

/// The page size asked of `DescribeCapacityReservations`.
const RESERVATIONS_PAGE: &str = "500";

impl HttpEc2 {
    /// `RunInstances` for one instance: the ids the answer names.
    ///
    /// The user data is sent in base64 and only when there is some; the request logs
    /// nothing of it.
    pub(super) fn launch(&self, request: &RunRequest) -> Result<Vec<InstanceId>, ApiError> {
        let mut form = Form::new("RunInstances")
            .with("ImageId", request.image.as_str())
            .with("InstanceType", request.instance_type.as_str())
            .with("MinCount", "1")
            .with("MaxCount", "1")
            .with("Placement.AvailabilityZone", request.zone.as_str())
            .with("ClientToken", request.client_token.as_str())
            .with("TagSpecification.1.ResourceType", "instance");
        for (index, tag) in request.tags.iter().enumerate() {
            let number = index + 1;
            form = form
                .with(format!("TagSpecification.1.Tag.{number}.Key"), &tag.key)
                .with(format!("TagSpecification.1.Tag.{number}.Value"), &tag.value);
        }
        if !request.user_data.is_empty() {
            form = form.with("UserData", STANDARD.encode(request.user_data.expose()));
        }
        match &request.purchase {
            LaunchPurchase::OnDemand => {}
            LaunchPurchase::Spot => {
                form = form.with("InstanceMarketOptions.MarketType", "spot");
            }
            LaunchPurchase::CapacityBlock(reservation) => {
                form = form
                    .with("InstanceMarketOptions.MarketType", "capacity-block")
                    .with(
                        "CapacityReservationSpecification.CapacityReservationTarget.CapacityReservationId",
                        reservation.as_str(),
                    );
            }
        }
        let answer = self.query(self.region.as_str(), &form)?;
        answer
            .items("instancesSet")
            .map(|item| item.required("instanceId").and_then(instance_id))
            .collect()
    }

    /// `DescribeInstances` for the instances `filter` selects, every page.
    pub(super) fn read_instances(
        &self,
        filter: &DescribeFilter,
    ) -> Result<Vec<Instance>, ApiError> {
        let form = Form::new("DescribeInstances");
        let form = match filter {
            DescribeFilter::Tags(tags) => tags.iter().enumerate().fold(
                form.with("MaxResults", INSTANCES_PAGE),
                |form, (index, tag)| {
                    let number = index + 1;
                    form.with(format!("Filter.{number}.Name"), format!("tag:{}", tag.key))
                        .with(format!("Filter.{number}.Value.1"), &tag.value)
                },
            ),
            DescribeFilter::Ids(ids) => ids.iter().enumerate().fold(form, |form, (index, id)| {
                form.with(format!("InstanceId.{}", index + 1), id.as_str())
            }),
        };
        self.listing(self.region.as_str(), &form, |answer| {
            answer
                .items("reservationSet")
                .flat_map(|reservation| reservation.items("instancesSet"))
                .map(instance)
                .collect()
        })
    }

    /// `PurchaseCapacityBlock` for a Linux block, tagged as the request says: the
    /// reservation the answer holds. The call has no client token.
    pub(super) fn purchase_block(
        &self,
        request: &BlockPurchase,
    ) -> Result<CapacityReservation, ApiError> {
        let form = Form::new("PurchaseCapacityBlock")
            .with("CapacityBlockOfferingId", &request.offering)
            .with("InstancePlatform", "Linux/UNIX")
            .with("TagSpecification.1.ResourceType", "capacity-reservation");
        let form = request
            .tags
            .iter()
            .enumerate()
            .fold(form, |form, (index, tag)| {
                let number = index + 1;
                form.with(format!("TagSpecification.1.Tag.{number}.Key"), &tag.key)
                    .with(format!("TagSpecification.1.Tag.{number}.Value"), &tag.value)
            });
        let answer = self.query(self.region.as_str(), &form)?;
        let reserved = answer
            .at("capacityReservation")
            .ok_or_else(|| unreadable("capacityReservation is missing"))?;
        reservation(reserved)
    }

    /// `DescribeCapacityReservations` for the reservations that carry `tags`, every page.
    pub(super) fn read_reservations(
        &self,
        tags: &[Tag],
    ) -> Result<Vec<CapacityReservation>, ApiError> {
        let form = tags.iter().enumerate().fold(
            Form::new("DescribeCapacityReservations").with("MaxResults", RESERVATIONS_PAGE),
            |form, (index, tag)| {
                let number = index + 1;
                form.with(format!("Filter.{number}.Name"), format!("tag:{}", tag.key))
                    .with(format!("Filter.{number}.Value.1"), &tag.value)
            },
        );
        self.listing(self.region.as_str(), &form, |answer| {
            answer
                .items("capacityReservationSet")
                .map(reservation)
                .collect()
        })
    }

    /// `TerminateInstances` for `ids`. The answer is not read: a success is all it says.
    pub(super) fn terminate(&self, ids: &[InstanceId]) -> Result<(), ApiError> {
        let form = ids
            .iter()
            .enumerate()
            .fold(Form::new("TerminateInstances"), |form, (index, id)| {
                form.with(format!("InstanceId.{}", index + 1), id.as_str())
            });
        self.ec2(self.region.as_str(), &form).map(|_| ())
    }
}

/// The reservation a `capacityReservation` element, or an `item` of
/// `capacityReservationSet`, describes. An unknown state is read as it is spelled.
fn reservation(item: &Node) -> Result<CapacityReservation, ApiError> {
    Ok(CapacityReservation {
        id: CapacityReservationId::new(item.required("capacityReservationId")?)
            .ok_or_else(|| unreadable("capacityReservationId is not a capacity reservation id"))?,
        state: ReservationState::parse(item.required("state")?),
        end_unix_seconds: unix_seconds(item.required("endDate")?, "endDate")?,
    })
}

/// The instance id `text`.
fn instance_id(text: &str) -> Result<InstanceId, ApiError> {
    InstanceId::new(text).ok_or_else(|| unreadable("instanceId is not an instance id"))
}

/// The instance an `item` of `instancesSet` describes.
fn instance(item: &Node) -> Result<Instance, ApiError> {
    Ok(Instance {
        id: instance_id(item.required("instanceId")?)?,
        state: InstanceState::parse(item.required("instanceState/name")?)
            .ok_or_else(|| unreadable("instanceState/name is not a state"))?,
        tags: item.items("tagSet").map(tag).collect::<Result<_, _>>()?,
        instance_type: InstanceType(item.required("instanceType")?.to_owned()),
        zone: Zone(item.required("placement/availabilityZone")?.to_owned()),
        image: ImageId::new(item.required("imageId")?)
            .ok_or_else(|| unreadable("imageId is not an image id"))?,
    })
}

/// The tag an `item` of `tagSet` holds. A tag with no value has an empty one.
fn tag(item: &Node) -> Result<Tag, ApiError> {
    Ok(Tag {
        key: item.required("key")?.to_owned(),
        value: item.text_at("value").unwrap_or_default().to_owned(),
    })
}
