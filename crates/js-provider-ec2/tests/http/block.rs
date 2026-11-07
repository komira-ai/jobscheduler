//! The Capacity Block calls: `PurchaseCapacityBlock`, `DescribeCapacityReservations` and a
//! `RunInstances` into a block, the request each makes and the records each answer is read
//! into.

use js_elastic::{AcquireId, SchedulerId};
use js_provider_ec2::{
    ApiError, BlockPurchase, CapacityReservation, CapacityReservationId, ClientToken,
    EC2_API_VERSION, Ec2Api, ImageId, InstanceId, InstanceType, LaunchPurchase, ReservationState,
    RunRequest, Tag, UserData, Zone,
};

use crate::script::{REGION, answer, form, param, rig, status, xml};

const RESERVATION: &str = "cr-0123456789abcdef0";

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn reservation_id() -> CapacityReservationId {
    CapacityReservationId::new(RESERVATION).unwrap()
}

/// The `capacityReservation` fields of an answer, with `id`, `state` and `end` as given.
fn fields(id: &str, state: &str, end: &str) -> String {
    format!(
        "<capacityReservationId>{id}</capacityReservationId><instanceType>p5.48xlarge</instanceType>\
        <state>{state}</state><startDate>2026-11-05T00:00:00.000Z</startDate><endDate>{end}</endDate>"
    )
}

fn purchase_answer(inner: &str) -> String {
    xml(
        "PurchaseCapacityBlockResponse",
        &format!("<capacityReservation>{inner}</capacityReservation>"),
    )
}

fn described(items: &[String], next: &str) -> String {
    let items: String = items
        .iter()
        .map(|inner| format!("<item>{inner}</item>"))
        .collect();
    xml(
        "DescribeCapacityReservationsResponse",
        &format!("<capacityReservationSet>{items}</capacityReservationSet>{next}"),
    )
}

fn buy(tags: Vec<Tag>) -> BlockPurchase {
    BlockPurchase {
        offering: "cb-0123456789abcdef0".to_owned(),
        tags,
    }
}

/// Asserts that a purchase sends one `PurchaseCapacityBlock` with the offering, the Linux
/// platform and every tag under the capacity reservation resource type, and no client
/// token, and reads the reservation it answers: its id, its state as spelled and its end
/// in seconds since the epoch. Catches a purchase that leaves a tag off or tags the wrong
/// resource.
#[test]
fn a_purchase_is_one_purchase_capacity_block_with_its_tags() {
    let (client, script) = rig(vec![answer(&purchase_answer(&fields(
        RESERVATION,
        "scheduled",
        "2026-11-06T00:00:00.000Z",
    )))]);
    let bought = client
        .purchase_capacity_block(&buy(vec![
            tag("js-scheduler", "scheduler-1"),
            tag("js-acquire-id", "a b&c"),
        ]))
        .unwrap();
    assert_eq!(
        bought,
        CapacityReservation {
            id: reservation_id(),
            state: ReservationState::Scheduled,
            end_unix_seconds: 1_793_836_800 + 86_400,
        }
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].url, format!("https://ec2.{REGION}.amazonaws.com/"));
    let mut pairs = form(&sent[0]);
    pairs.sort();
    let expected = [
        ("Action", "PurchaseCapacityBlock"),
        ("CapacityBlockOfferingId", "cb-0123456789abcdef0"),
        ("InstancePlatform", "Linux/UNIX"),
        ("TagSpecification.1.ResourceType", "capacity-reservation"),
        ("TagSpecification.1.Tag.1.Key", "js-scheduler"),
        ("TagSpecification.1.Tag.1.Value", "scheduler-1"),
        ("TagSpecification.1.Tag.2.Key", "js-acquire-id"),
        ("TagSpecification.1.Tag.2.Value", "a b&c"),
        ("Version", EC2_API_VERSION),
    ];
    let mut expected = expected.map(|(n, v)| (n.to_owned(), v.to_owned())).to_vec();
    expected.sort();
    assert_eq!(pairs, expected);
}

/// Asserts that a purchase answer is read field by field with the exact words of what is
/// wrong: no reservation, no id, an id that is not a reservation id, no state, no end and an
/// end that is not a time; and that a state this release does not know is read as it is
/// spelled.
#[test]
fn a_purchase_answer_that_cannot_be_read_says_which_field() {
    let end = "2026-11-06T00:00:00Z";
    let cases = [
        (
            xml("PurchaseCapacityBlockResponse", ""),
            "capacityReservation is missing",
        ),
        (
            purchase_answer(&fields("", "active", end)),
            "capacityReservationId is missing",
        ),
        (
            purchase_answer(&fields("i-0123456789abcdef0", "active", end)),
            "capacityReservationId is not a capacity reservation id",
        ),
        (
            purchase_answer(&fields(RESERVATION, "", end)),
            "state is missing",
        ),
        (
            purchase_answer(&fields(RESERVATION, "active", "")),
            "endDate is missing",
        ),
        (
            purchase_answer(&fields(RESERVATION, "active", "tomorrow")),
            "endDate is not a time since the epoch",
        ),
    ];
    for (body, words) in cases {
        let (client, _) = rig(vec![answer(&body)]);
        assert_eq!(
            client.purchase_capacity_block(&buy(Vec::new())),
            Err(ApiError::Unreadable(words.to_owned())),
            "{words}"
        );
    }
    let (client, _) = rig(vec![answer(&purchase_answer(&fields(
        RESERVATION,
        "brand-new",
        end,
    )))]);
    assert_eq!(
        client
            .purchase_capacity_block(&buy(Vec::new()))
            .unwrap()
            .state,
        ReservationState::Other("brand-new".to_owned())
    );
}

/// Asserts that a purchase the cloud refuses is the cloud's error code without its
/// message.
#[test]
fn a_refused_purchase_is_the_cloud_code() {
    let body = "<Response><Errors><Error><Code>Unsupported</Code>\
        <Message>no block for tag secret-tag</Message></Error></Errors>\
        <RequestID>0000</RequestID></Response>";
    let (client, _) = rig(vec![status(400, body)]);
    let error = client
        .purchase_capacity_block(&buy(Vec::new()))
        .unwrap_err();
    assert_eq!(error, ApiError::answered("Unsupported"));
    assert!(!format!("{error:?} {error}").contains("secret-tag"));
}

/// Asserts that a read of reservations by tags sends `DescribeCapacityReservations` with
/// one name and value filter per tag and the page size, reads every item of every page in
/// order (the second request carries the token of the first answer), and sends no filter
/// for no tags.
#[test]
fn a_reservation_read_filters_by_tag_and_walks_the_pages() {
    let first = fields(RESERVATION, "active", "2026-11-06T00:00:00Z");
    let second = fields("cr-0123456789abcdef1", "expired", "2026-11-07T00:00:00Z");
    let (client, script) = rig(vec![
        answer(&described(
            std::slice::from_ref(&first),
            "<nextToken>page-2</nextToken>",
        )),
        answer(&described(std::slice::from_ref(&second), "")),
    ]);
    let read = client
        .capacity_reservations(&[tag("js-scheduler", "s 1"), tag("js-acquire-id", "a1")])
        .unwrap();
    assert_eq!(
        read,
        [
            CapacityReservation {
                id: reservation_id(),
                state: ReservationState::Active,
                end_unix_seconds: 1_793_836_800 + 86_400,
            },
            CapacityReservation {
                id: CapacityReservationId::new("cr-0123456789abcdef1").unwrap(),
                state: ReservationState::Expired,
                end_unix_seconds: 1_793_836_800 + 2 * 86_400,
            },
        ]
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    let expected = [
        ("Action", "DescribeCapacityReservations"),
        ("Version", EC2_API_VERSION),
        ("MaxResults", "500"),
        ("Filter.1.Name", "tag:js-scheduler"),
        ("Filter.1.Value.1", "s 1"),
        ("Filter.2.Name", "tag:js-acquire-id"),
        ("Filter.2.Value.1", "a1"),
    ];
    let mut expected: Vec<(String, String)> =
        expected.map(|(n, v)| (n.to_owned(), v.to_owned())).to_vec();
    let sorted = |mut pairs: Vec<(String, String)>| {
        pairs.sort();
        pairs
    };
    assert_eq!(sorted(form(&sent[0])), sorted(expected.clone()));
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some("page-2"));
    expected.push(("NextToken".to_owned(), "page-2".to_owned()));
    assert_eq!(sorted(form(&sent[1])), sorted(expected));

    let (client, script) = rig(vec![answer(&described(&[], ""))]);
    assert_eq!(client.capacity_reservations(&[]), Ok(Vec::new()));
    assert_eq!(param(&script.sent()[0], "Filter.1.Name"), None);
}

/// Asserts that an item of a reservation list is read with the same words as a purchase:
/// an unreadable id fails the whole read.
#[test]
fn a_reservation_list_with_an_unreadable_item_fails() {
    let bad = fields("not-an-id", "active", "2026-11-06T00:00:00Z");
    let (client, _) = rig(vec![answer(&described(&[bad], ""))]);
    assert_eq!(
        client.capacity_reservations(&[]),
        Err(ApiError::Unreadable(
            "capacityReservationId is not a capacity reservation id".to_owned()
        ))
    );
}

/// Asserts that a launch into a block names the capacity-block market type and the
/// reservation as the target, and that the other purchases name neither. Catches a launch
/// into a block that is bought as on demand.
#[test]
fn a_launch_into_a_block_names_the_market_and_the_reservation() {
    let run = RunRequest {
        image: ImageId::new("ami-0123456789abcdef0").unwrap(),
        instance_type: InstanceType("p5.48xlarge".to_owned()),
        zone: Zone("zone-a1".to_owned()),
        purchase: LaunchPurchase::CapacityBlock(reservation_id()),
        user_data: UserData::new(Vec::new()),
        tags: Vec::new(),
        client_token: ClientToken::derive(
            &AcquireId::new("acquire-1").unwrap(),
            &SchedulerId::new("scheduler-1").unwrap(),
        ),
    };
    let reply = xml(
        "RunInstancesResponse",
        "<instancesSet><item><instanceId>i-0123456789abcdef0</instanceId></item></instancesSet>",
    );
    let (client, script) = rig(vec![answer(&reply)]);
    assert_eq!(
        client.run_instances(&run).unwrap(),
        vec![InstanceId::new("i-0123456789abcdef0").unwrap()]
    );
    let sent = script.sent();
    let name = "CapacityReservationSpecification.CapacityReservationTarget.CapacityReservationId";
    assert_eq!(
        param(&sent[0], "InstanceMarketOptions.MarketType").as_deref(),
        Some("capacity-block")
    );
    assert_eq!(param(&sent[0], name).as_deref(), Some(RESERVATION));

    let on_demand = RunRequest {
        purchase: LaunchPurchase::OnDemand,
        ..run
    };
    let (client, script) = rig(vec![answer(&reply)]);
    client.run_instances(&on_demand).unwrap();
    assert_eq!(
        param(&script.sent()[0], "InstanceMarketOptions.MarketType"),
        None
    );
    assert_eq!(param(&script.sent()[0], name), None);
}
