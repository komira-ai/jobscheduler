//! The launch side: `RunInstances`, `DescribeInstances` and `TerminateInstances`, the
//! request each makes and the records each answer is read into.

use js_elastic::{AcquireId, SchedulerId};
use js_provider_ec2::{
    ApiError, ClientToken, DescribeFilter, EC2_API_VERSION, Ec2Api, ImageId, Instance, InstanceId,
    InstanceState, InstanceType, LaunchPurchase, RunRequest, Tag, UserData, Zone,
};

use crate::script::{REGION, answer, form, param, rig, status, xml};

const IMAGE: &str = "ami-0123456789abcdef0";

fn instance_id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn request(purchase: LaunchPurchase, user_data: &[u8], tags: Vec<Tag>) -> RunRequest {
    RunRequest {
        image: ImageId::new(IMAGE).unwrap(),
        instance_type: InstanceType("p5.48xlarge".to_owned()),
        zone: Zone("zone-a1".to_owned()),
        purchase,
        user_data: UserData::new(user_data.to_vec()),
        tags,
        client_token: ClientToken::derive(
            &AcquireId::new("acquire-1").unwrap(),
            &SchedulerId::new("scheduler-1").unwrap(),
        ),
    }
}

fn run_answer(ids: &[&str]) -> String {
    let items: String = ids
        .iter()
        .map(|id| format!("<item><instanceId>{id}</instanceId><imageId>{IMAGE}</imageId></item>"))
        .collect();
    xml(
        "RunInstancesResponse",
        &format!(
            "<reservationId>r-0123456789abcdef0</reservationId><instancesSet>{items}</instancesSet>"
        ),
    )
}

/// Asserts that an on-demand `run_instances` sends one `RunInstances` for one instance
/// with the image, type, zone, client token, the user data in base64 and every tag under
/// the instance resource type, and no market option, and answers the id in the reply. It
/// catches a launch that leaves a tag off, sends the user data as it is, or buys spot.
#[test]
fn an_on_demand_launch_is_one_run_instances_with_its_tags_and_user_data() {
    let launch = request(
        LaunchPurchase::OnDemand,
        b"join=secret-join-token\n",
        vec![tag("js-scheduler", "scheduler-1"), tag("Name", "a b&c=d")],
    );
    let (client, script) = rig(vec![answer(&run_answer(&["i-0123456789abcdef0"]))]);
    assert_eq!(
        client.run_instances(&launch).unwrap(),
        vec![instance_id("i-0123456789abcdef0")]
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    let request = &sent[0];
    assert_eq!(request.url, format!("https://ec2.{REGION}.amazonaws.com/"));
    let mut pairs = form(request);
    pairs.sort();
    let token = launch.client_token.as_str();
    let expected = [
        ("Action", "RunInstances"),
        ("ClientToken", token),
        ("ImageId", IMAGE),
        ("InstanceType", "p5.48xlarge"),
        ("MaxCount", "1"),
        ("MinCount", "1"),
        ("Placement.AvailabilityZone", "zone-a1"),
        ("TagSpecification.1.ResourceType", "instance"),
        ("TagSpecification.1.Tag.1.Key", "js-scheduler"),
        ("TagSpecification.1.Tag.1.Value", "scheduler-1"),
        ("TagSpecification.1.Tag.2.Key", "Name"),
        ("TagSpecification.1.Tag.2.Value", "a b&c=d"),
        ("UserData", "am9pbj1zZWNyZXQtam9pbi10b2tlbgo="),
        ("Version", EC2_API_VERSION),
    ];
    let mut expected = expected.map(|(n, v)| (n.to_owned(), v.to_owned())).to_vec();
    expected.sort();
    assert_eq!(pairs, expected);
}

/// Asserts that a spot launch adds the spot market type and nothing else, and that a launch
/// with no user data and no tags sends neither a `UserData` nor a tag specification
/// entry. It catches an empty `UserData` parameter, which the cloud may refuse.
#[test]
fn a_spot_launch_names_the_market_and_empty_user_data_is_left_out() {
    let (client, script) = rig(vec![answer(&run_answer(&["i-0123456789abcdef0"]))]);
    client
        .run_instances(&request(LaunchPurchase::Spot, b"", Vec::new()))
        .unwrap();
    let sent = script.sent();
    assert_eq!(
        param(&sent[0], "InstanceMarketOptions.MarketType").as_deref(),
        Some("spot")
    );
    assert_eq!(param(&sent[0], "UserData"), None);
    assert_eq!(param(&sent[0], "TagSpecification.1.Tag.1.Key"), None);
}

/// Asserts that an answer naming no instance is an empty list (the caller decides what
/// that means), that an answer naming two gives both in order, and that an id that is not
/// an instance id is unreadable. It catches a launch that reads only the first instance.
#[test]
fn run_instances_answers_every_id_the_cloud_names() {
    let launch = request(LaunchPurchase::OnDemand, b"", Vec::new());
    let (client, _) = rig(vec![answer(&run_answer(&[]))]);
    assert_eq!(client.run_instances(&launch), Ok(vec![]));
    let (client, _) = rig(vec![answer(&run_answer(&[
        "i-0123456789abcdef0",
        "i-0123abcd",
    ]))]);
    assert_eq!(
        client.run_instances(&launch),
        Ok(vec![
            instance_id("i-0123456789abcdef0"),
            instance_id("i-0123abcd")
        ])
    );
    let (client, _) = rig(vec![answer(&run_answer(&["not-an-id"]))]);
    assert_eq!(
        client.run_instances(&launch),
        Err(ApiError::Unreadable(
            "instanceId is not an instance id".to_owned()
        ))
    );
}

/// Asserts that a refusal of the launch comes back as the cloud's error code, and the
/// message of the answer is not in the error. It catches an error that keeps the cloud's
/// text, which may echo the request.
#[test]
fn a_refused_launch_is_the_cloud_code_without_its_message() {
    let body = "<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code>\
        <Message>no capacity for join=secret-join-token</Message></Error></Errors>\
        <RequestID>0000</RequestID></Response>";
    let (client, _) = rig(vec![status(500, body)]);
    let error = client
        .run_instances(&request(LaunchPurchase::OnDemand, b"", Vec::new()))
        .unwrap_err();
    assert_eq!(error, ApiError::answered("InsufficientInstanceCapacity"));
    assert!(!format!("{error:?} {error}").contains("secret-join-token"));
}

fn instance_xml(id: &str, state: &str, tags: &str) -> String {
    format!(
        "<item><instanceId>{id}</instanceId><imageId>{IMAGE}</imageId>\
        <instanceState><code>16</code><name>{state}</name></instanceState>\
        <instanceType>p5.48xlarge</instanceType>\
        <placement><availabilityZone>zone-a1</availabilityZone></placement>\
        <tagSet>{tags}</tagSet></item>"
    )
}

fn described(reservations: &[String], next: &str) -> String {
    let sets: String = reservations
        .iter()
        .map(|instances| {
            format!("<item><reservationId>r-1</reservationId><instancesSet>{instances}</instancesSet></item>")
        })
        .collect();
    xml(
        "DescribeInstancesResponse",
        &format!("<reservationSet>{sets}</reservationSet>{next}"),
    )
}

fn row(id: &str, state: InstanceState, tags: Vec<Tag>) -> Instance {
    Instance {
        id: instance_id(id),
        state,
        tags,
        instance_type: InstanceType("p5.48xlarge".to_owned()),
        zone: Zone("zone-a1".to_owned()),
        image: ImageId::new(IMAGE).unwrap(),
    }
}

/// Asserts that a describe by tags sends a filter per tag (`tag:<key>` and its value) and
/// a page size, reads the instances of every reservation of every page with their states
/// and tags (a tag with no value has an empty one, text with entities and CDATA is read
/// as written), and sends the token of the first page with the second. It catches a
/// filter on the wrong name and a read of the first reservation only.
#[test]
fn a_describe_by_tags_filters_pages_and_reads_every_instance() {
    let first = instance_xml(
        "i-0123456789abcdef0",
        "running",
        "<item><key>js-scheduler</key><value>scheduler-1</value></item>\
         <item><key>note</key><value>a &amp; b &#33; <![CDATA[<c>]]></value></item>\
         <!-- a comment --><item><key>empty</key></item>",
    );
    let second = instance_xml("i-0123abcd", "terminated", "");
    let (client, script) = rig(vec![
        answer(&described(
            &[first, second],
            "<nextToken>page-two</nextToken>",
        )),
        answer(&described(
            &[instance_xml("i-0fedcba987654321f", "shutting-down", "")],
            "<nextToken></nextToken>",
        )),
    ]);
    let filter = DescribeFilter::Tags(vec![tag("js-scheduler", "scheduler-1"), tag("a", "b")]);
    assert_eq!(
        client.describe_instances(&filter).unwrap(),
        vec![
            row(
                "i-0123456789abcdef0",
                InstanceState::Running,
                vec![
                    tag("js-scheduler", "scheduler-1"),
                    tag("note", "a & b ! <c>"),
                    tag("empty", ""),
                ]
            ),
            row("i-0123abcd", InstanceState::Terminated, vec![]),
            row("i-0fedcba987654321f", InstanceState::ShuttingDown, vec![]),
        ]
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    let mut pairs = form(&sent[0]);
    pairs.sort();
    let expected = [
        ("Action", "DescribeInstances"),
        ("Filter.1.Name", "tag:js-scheduler"),
        ("Filter.1.Value.1", "scheduler-1"),
        ("Filter.2.Name", "tag:a"),
        ("Filter.2.Value.1", "b"),
        ("MaxResults", "500"),
        ("Version", EC2_API_VERSION),
    ]
    .map(|(n, v)| (n.to_owned(), v.to_owned()));
    assert_eq!(pairs, expected);
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some("page-two"));
}

/// Asserts that a describe by ids sends one `InstanceId.N` per id and no page size (the
/// cloud refuses both together), that an empty answer is an empty list, and that the
/// cloud's `InvalidInstanceID.NotFound` is an error that says so. It catches a page size
/// sent with ids.
#[test]
fn a_describe_by_ids_names_the_ids_and_not_found_is_an_error() {
    let ids = vec![
        instance_id("i-0123456789abcdef0"),
        instance_id("i-0123abcd"),
    ];
    let (client, script) = rig(vec![
        answer(&described(&[instance_xml("i-0123abcd", "pending", "")], "")),
        answer(&xml("DescribeInstancesResponse", "")),
        status(
            400,
            "<Response><Errors><Error><Code>InvalidInstanceID.NotFound</Code></Error></Errors></Response>",
        ),
    ]);
    let found = client
        .describe_instances(&DescribeFilter::Ids(ids.clone()))
        .unwrap();
    assert_eq!(
        found,
        vec![row("i-0123abcd", InstanceState::Pending, vec![])]
    );
    let sent = script.sent();
    assert_eq!(
        param(&sent[0], "InstanceId.1").as_deref(),
        Some("i-0123456789abcdef0")
    );
    assert_eq!(
        param(&sent[0], "InstanceId.2").as_deref(),
        Some("i-0123abcd")
    );
    assert_eq!(param(&sent[0], "MaxResults"), None);
    assert_eq!(
        client.describe_instances(&DescribeFilter::Ids(ids.clone())),
        Ok(vec![])
    );
    let error = client
        .describe_instances(&DescribeFilter::Ids(ids))
        .unwrap_err();
    assert!(error.is_not_found());
}

/// Asserts that an instance with a missing field, an id, state or image the crate does not
/// know, or a tag with no key is unreadable and names what. It catches a read that skips
/// the instance and returns a short list.
#[test]
fn an_instance_with_a_missing_or_bad_field_is_unreadable() {
    let good = instance_xml(
        "i-0123456789abcdef0",
        "running",
        "<item><key>k</key><value>v</value></item>",
    );
    let cases = [
        (
            "<instanceId>i-0123456789abcdef0</instanceId>",
            "<instanceId>vm-1</instanceId>",
            "instanceId is not an instance id",
        ),
        (
            "<instanceId>i-0123456789abcdef0</instanceId>",
            "",
            "instanceId is missing",
        ),
        (
            "<name>running</name>",
            "<name>sleeping</name>",
            "instanceState/name is not a state",
        ),
        ("<name>running</name>", "", "instanceState/name is missing"),
        ("<key>k</key>", "", "key is missing"),
        (
            "<instanceType>p5.48xlarge</instanceType>",
            "",
            "instanceType is missing",
        ),
        (
            "<availabilityZone>zone-a1</availabilityZone>",
            "",
            "placement/availabilityZone is missing",
        ),
        (IMAGE, "image-1", "imageId is not an image id"),
    ];
    for (from, to, what) in cases {
        assert!(good.contains(from), "{from}");
        let broken = good.replacen(from, to, 1);
        let (client, _) = rig(vec![answer(&described(&[broken], ""))]);
        let filter = DescribeFilter::Tags(vec![tag("k", "v")]);
        assert_eq!(
            client.describe_instances(&filter),
            Err(ApiError::Unreadable(what.to_owned())),
            "{what}"
        );
    }
}

/// Asserts that `terminate_instances` sends one `TerminateInstances` naming every id,
/// succeeds on any success answer whatever its body, and fails with the cloud's code when
/// the cloud refuses. It catches a terminate that reads the answer's body and fails on
/// one it does not understand.
#[test]
fn terminate_names_every_id_and_succeeds_on_a_success_answer() {
    let ids = [
        instance_id("i-0123456789abcdef0"),
        instance_id("i-0123abcd"),
    ];
    let (client, script) = rig(vec![
        answer("not xml at all <"),
        status(
            400,
            "<Response><Errors><Error><Code>InvalidInstanceID.NotFound</Code></Error></Errors></Response>",
        ),
    ]);
    assert_eq!(client.terminate_instances(&ids), Ok(()));
    let error = client.terminate_instances(&ids).unwrap_err();
    assert!(error.is_not_found());
    let sent = script.sent();
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("TerminateInstances")
    );
    assert_eq!(
        param(&sent[0], "InstanceId.1").as_deref(),
        Some("i-0123456789abcdef0")
    );
    assert_eq!(
        param(&sent[0], "InstanceId.2").as_deref(),
        Some("i-0123abcd")
    );
}
