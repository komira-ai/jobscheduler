//! The launch side: RunInstances, DescribeInstances and TerminateInstances.

use super::*;

/// The client token and tags of the launch the RunInstances fixture answers.
pub(crate) fn run_request(purchase: LaunchPurchase) -> RunRequest {
    let acquire = AcquireId::new("acq-one").unwrap();
    let scheduler = SchedulerId::new(SCHEDULER).unwrap();
    let tag = |key: &str, value: &str| Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    RunRequest {
        image: ImageId::new("ami-0123456789abcdef0").unwrap(),
        instance_type: typed("m7i.large"),
        zone: zone("us-east-1a"),
        purchase,
        user_data: UserData::new(b"#cloud-config\n".to_vec()),
        tags: vec![
            tag("js-scheduler", SCHEDULER),
            tag("js-acquire-id", "acq-one"),
            tag("Name", "js-865c0ebee0c3-2ba7a7c4602a"),
            tag("js-label:validation-run", "run-1"),
        ],
        client_token: ClientToken::derive(&acquire, &scheduler),
    }
}

/// Asserts that `RunInstances` sends the image, type, count of one, zone, client token,
/// the tags as a tag specification for instances, and the user data in base64 (and the
/// spot market option only for a spot launch), and that the ids of the answer's
/// `instancesSet` come back. Catches a launch parameter sent under another name and an id
/// read from another element.
#[test]
fn run_instances_sends_the_launch_and_reads_the_instance_id() {
    let (api, script) = client(vec![ok("run_instances.xml"), ok("run_instances.xml")]);
    let ids = api
        .run_instances(&run_request(LaunchPurchase::OnDemand))
        .unwrap();
    assert_eq!(ids, vec![InstanceId::new("i-0123456789abcdef0").unwrap()]);
    let spot = api
        .run_instances(&run_request(LaunchPurchase::Spot))
        .unwrap();
    assert_eq!(spot, ids);
    let sent = script.sent();
    let request = &sent[0];
    let token = ClientToken::derive(
        &AcquireId::new("acq-one").unwrap(),
        &SchedulerId::new(SCHEDULER).unwrap(),
    );
    let expected = [
        ("Action", "RunInstances"),
        ("Version", "2016-11-15"),
        ("ImageId", "ami-0123456789abcdef0"),
        ("InstanceType", "m7i.large"),
        ("MinCount", "1"),
        ("MaxCount", "1"),
        ("Placement.AvailabilityZone", "us-east-1a"),
        ("ClientToken", token.as_str()),
        ("TagSpecification.1.ResourceType", "instance"),
        ("TagSpecification.1.Tag.1.Key", "js-scheduler"),
        ("TagSpecification.1.Tag.1.Value", SCHEDULER),
        ("TagSpecification.1.Tag.2.Key", "js-acquire-id"),
        ("TagSpecification.1.Tag.2.Value", "acq-one"),
        ("TagSpecification.1.Tag.3.Key", "Name"),
        (
            "TagSpecification.1.Tag.3.Value",
            "js-865c0ebee0c3-2ba7a7c4602a",
        ),
        ("TagSpecification.1.Tag.4.Key", "js-label:validation-run"),
        ("TagSpecification.1.Tag.4.Value", "run-1"),
    ];
    for (name, value) in expected {
        assert_eq!(param(request, name).as_deref(), Some(value), "{name}");
    }
    assert_eq!(
        param(request, "UserData"),
        Some(STANDARD.encode("#cloud-config\n"))
    );
    assert_eq!(param(request, "InstanceMarketOptions.MarketType"), None);
    assert_eq!(
        param(&sent[1], "InstanceMarketOptions.MarketType").as_deref(),
        Some("spot")
    );
}

/// Asserts that `DescribeInstances` by id sends the ids and no page size, and reads the
/// instance's id, state, type, zone, image and tags (the scheduler, acquire, name and
/// label marks) from the reservation set. Catches a tag read back under the wrong key and
/// an id filter sent as a tag filter.
#[test]
fn describe_instances_by_id_reads_the_instance_and_its_tags() {
    let (api, script) = client(vec![ok("describe_instances_by_ids.xml")]);
    let id = InstanceId::new("i-0123456789abcdef0").unwrap();
    let rows = api
        .describe_instances(&DescribeFilter::Ids(vec![id.clone()]))
        .unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("DescribeInstances")
    );
    assert_eq!(
        param(&sent[0], "InstanceId.1").as_deref(),
        Some(id.as_str())
    );
    assert_eq!(param(&sent[0], "MaxResults"), None);
    assert_eq!(param(&sent[0], "Filter.1.Name"), None);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.state, InstanceState::Running);
    assert_eq!(row.instance_type, typed("m7i.large"));
    assert_eq!(row.zone, zone("us-east-1a"));
    assert_eq!(row.image, ImageId::new("ami-0123456789abcdef0").unwrap());
    assert_eq!(row.tag("js-scheduler"), Some(SCHEDULER));
    assert_eq!(row.tag("js-acquire-id"), Some("acq-one"));
    assert_eq!(row.tag("Name"), Some("js-865c0ebee0c3-2ba7a7c4602a"));
    assert_eq!(row.tag("js-label:validation-run"), Some("run-1"));
    assert_eq!(row.tag("absent"), None);
}

/// An instance as the state test compares it: id, state, status, type, zone and acquire tag.
type Seen<'a> = (
    &'a str,
    InstanceState,
    HostStatus,
    &'a str,
    &'a str,
    Option<&'a str>,
);

/// Asserts that `DescribeInstances` by the scheduler's marks is read over two pages: the
/// filters name the marks, the second request carries the first answer's token, the
/// reservations of both pages are flattened in order, and every state the fixtures carry
/// is read as itself and maps to the status the scheduler reads it as (pending is
/// booting, running active, shutting-down and stopping terminating, stopped and
/// terminated terminated). Catches a reservation skipped at a page boundary and a state
/// name that is not mapped.
#[test]
fn describe_instances_by_tags_merges_pages_and_maps_every_state() {
    let (api, script) = client(vec![
        ok("describe_instances_by_tags_page1.xml"),
        ok("describe_instances_by_tags_page2.xml"),
    ]);
    let marks = vec![
        Tag {
            key: "js-scheduler".to_owned(),
            value: SCHEDULER.to_owned(),
        },
        Tag {
            key: "js-acquire-id".to_owned(),
            value: "acq-one".to_owned(),
        },
    ];
    let rows = api
        .describe_instances(&DescribeFilter::Tags(marks))
        .unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    for request in &sent {
        assert_eq!(param(request, "MaxResults").as_deref(), Some("500"));
        assert_eq!(
            param(request, "Filter.1.Name").as_deref(),
            Some("tag:js-scheduler")
        );
        assert_eq!(
            param(request, "Filter.1.Value.1").as_deref(),
            Some(SCHEDULER)
        );
        assert_eq!(
            param(request, "Filter.2.Name").as_deref(),
            Some("tag:js-acquire-id")
        );
        assert_eq!(
            param(request, "Filter.2.Value.1").as_deref(),
            Some("acq-one")
        );
    }
    assert_eq!(param(&sent[0], "NextToken"), None);
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some(TOKEN));
    let seen: Vec<Seen> = rows
        .iter()
        .map(|row| {
            (
                row.id.as_str(),
                row.state,
                row.state.status(),
                row.instance_type.as_str(),
                row.zone.as_str(),
                row.tag("js-acquire-id"),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                "i-0123456789abcdef0",
                InstanceState::Running,
                HostStatus::Active,
                "m7i.large",
                "us-east-1a",
                Some("acq-one")
            ),
            (
                "i-0fedcba9876543210",
                InstanceState::Pending,
                HostStatus::Booting,
                "p5.48xlarge",
                "us-east-1b",
                Some("acq-two")
            ),
            (
                "i-00000000000000001",
                InstanceState::ShuttingDown,
                HostStatus::Terminating,
                "m7i.large",
                "us-east-1a",
                Some("acq-three")
            ),
            (
                "i-00000000000000002",
                InstanceState::Stopping,
                HostStatus::Terminating,
                "m7i.large",
                "us-east-1a",
                Some("acq-four")
            ),
            (
                "i-00000000000000003",
                InstanceState::Stopped,
                HostStatus::Terminated,
                "m7i.large",
                "us-east-1a",
                Some("acq-five")
            ),
            (
                "i-00000000000000004",
                InstanceState::Terminated,
                HostStatus::Terminated,
                "m7i.large",
                "us-east-1a",
                Some("acq-six")
            ),
        ]
    );
    assert_eq!(rows[0].tag("js-label:validation-run"), Some("run-1"));
    assert_eq!(rows[1].tag("js-label:validation-run"), None);
    assert_eq!(rows[1].tag("Name"), Some("js-865c0ebee0c3-f1bf74457ee1"));
}

/// Asserts that a `DescribeInstances` answer with an empty reservation set is no
/// instance, not an error. Catches an empty list read as an unreadable answer.
#[test]
fn an_empty_reservation_set_is_no_instances() {
    let (api, _) = client(vec![ok("describe_instances_empty.xml")]);
    assert_eq!(
        api.describe_instances(&DescribeFilter::Tags(Vec::new())),
        Ok(Vec::<Instance>::new())
    );
}

/// Asserts that `TerminateInstances` sends the ids and that its answer is a success
/// whatever its body holds. Catches ids sent under a different parameter name.
#[test]
fn terminate_instances_sends_the_ids_and_succeeds_on_the_answer() {
    let (api, script) = client(vec![ok("terminate_instances.xml")]);
    let id = InstanceId::new("i-0123456789abcdef0").unwrap();
    assert_eq!(api.terminate_instances(std::slice::from_ref(&id)), Ok(()));
    let sent = script.sent();
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("TerminateInstances")
    );
    assert_eq!(
        param(&sent[0], "InstanceId.1").as_deref(),
        Some(id.as_str())
    );
}
