//! The provider over the documented answers: acquire, list, observe and release.

use super::*;
use crate::catalog::machine;

/// The provider of the fixtures' scheduler and region over `answers`: live, enabled, and
/// with room for five machines.
fn provider(answers: Vec<script::Answer>) -> (Ec2Provider, Arc<script::Script>) {
    let (api, script) = client(answers);
    let images = Images::new(None, Some(ImageId::new("ami-0123456789abcdef0").unwrap())).unwrap();
    let config = Ec2Config::new(
        SchedulerId::new(SCHEDULER).unwrap(),
        region(),
        images,
        NonZeroUsize::new(5).unwrap(),
    )
    .enable()
    .live();
    let settings = Ec2CatalogSettings::new(REGION, &["on-demand"], None, None).unwrap();
    let provider = Ec2Provider::new(
        Arc::new(api),
        Arc::new(ManualClock::new()),
        settings,
        config,
    );
    (provider, script)
}

/// An acquire of the scheduler's `m7i.large` on demand in `us-east-1a` for `acquire`.
fn acquire_request(acquire: &str) -> AcquireRequest {
    let acquire = AcquireId::new(acquire).unwrap();
    let offer = Offer {
        id: "m7i.large/on-demand".to_owned(),
        zone: Some("us-east-1a".to_owned()),
        purchase: Some(Purchase::OnDemand),
        price_cents_per_hour: Some(11),
        ..machine("m7i.large", 2, 8192, 0)
    };
    AcquireRequest {
        name: HostName::derive(&SchedulerId::new(SCHEDULER).unwrap(), &acquire),
        acquire,
        pool: "pool-a".to_owned(),
        lifetime: None,
        offer,
        labels: vec![("validation-run".to_owned(), "run-1".to_owned())],
        payload: BootPayload::new("#cloud-config\n".to_owned()),
    }
}

/// Asserts that an acquire over the documented answers reads the acquire's instances
/// first (the empty reservation set), then sends `RunInstances`, and answers a booting
/// machine of the instance id the launch answer names; and that a launch answered with the
/// capacity error fixture is a capacity refusal carrying the code. Catches a provider that
/// launches without reading first and a refusal that is not the documented constant.
#[test]
fn the_provider_acquires_over_the_documented_answers() {
    let (provider, script) = provider(vec![
        ok("describe_instances_empty.xml"),
        ok("run_instances.xml"),
    ]);
    let request = acquire_request("acq-one");
    let answer = provider.acquire(&request);
    assert_eq!(
        answer,
        Acquired::Accepted {
            held: Held {
                id: HostRef("i-0123456789abcdef0".to_owned()),
                acquire: "acq-one".to_owned(),
                name: Some("js-865c0ebee0c3-2ba7a7c4602a".to_owned()),
                offer: "m7i.large/on-demand".to_owned(),
                zone: Some("us-east-1a".to_owned()),
                status: HostStatus::Booting,
            },
            extras: Vec::new(),
        }
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        param(&sent[0], "Action").as_deref(),
        Some("DescribeInstances")
    );
    assert_eq!(
        param(&sent[0], "Filter.2.Value.1").as_deref(),
        Some("acq-one")
    );
    assert_eq!(param(&sent[1], "Action").as_deref(), Some("RunInstances"));
    assert_eq!(
        param(&sent[1], "TagSpecification.1.Tag.4.Key").as_deref(),
        Some("js-label:validation-run")
    );

    let (provider, _) = provider_refusing();
    assert_eq!(
        provider.acquire(&request),
        refused_launch(RefusalCode::Capacity, "InsufficientInstanceCapacity")
    );
}

/// A provider whose launch is answered with the capacity error fixture.
fn provider_refusing() -> (Ec2Provider, Arc<script::Script>) {
    provider(vec![
        ok("describe_instances_empty.xml"),
        failing(500, "error_insufficient_instance_capacity.xml"),
    ])
}

/// Asserts that the provider's list over the two documented pages asks for the
/// scheduler's mark with the second page's token, and holds the four instances of this
/// scheduler that are not terminated or stopped, each with its acquire id, name, type, zone
/// and status in the order the cloud gave them. Catches a page dropped and a stopped
/// instance counted as held.
#[test]
fn the_provider_lists_the_machines_of_both_pages() {
    let (provider, script) = provider(vec![
        ok("describe_instances_by_tags_page1.xml"),
        ok("describe_instances_by_tags_page2.xml"),
    ]);
    let held = |id: &str, acquire: &str, name: &str, offer: &str, at: &str, status| Held {
        id: HostRef(id.to_owned()),
        acquire: acquire.to_owned(),
        name: Some(name.to_owned()),
        offer: offer.to_owned(),
        zone: Some(at.to_owned()),
        status,
    };
    assert_eq!(
        provider.list().unwrap(),
        vec![
            held(
                "i-0123456789abcdef0",
                "acq-one",
                "js-865c0ebee0c3-2ba7a7c4602a",
                "m7i.large",
                "us-east-1a",
                HostStatus::Active
            ),
            held(
                "i-0fedcba9876543210",
                "acq-two",
                "js-865c0ebee0c3-f1bf74457ee1",
                "p5.48xlarge",
                "us-east-1b",
                HostStatus::Booting
            ),
            held(
                "i-00000000000000001",
                "acq-three",
                "js-865c0ebee0c3-20af312fcbec",
                "m7i.large",
                "us-east-1a",
                HostStatus::Terminating
            ),
            held(
                "i-00000000000000002",
                "acq-four",
                "js-865c0ebee0c3-45ec05fda159",
                "m7i.large",
                "us-east-1a",
                HostStatus::Terminating
            ),
        ]
    );
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        param(&sent[0], "Filter.1.Name").as_deref(),
        Some("tag:js-scheduler")
    );
    assert_eq!(param(&sent[0], "Filter.2.Name"), None);
    assert_eq!(param(&sent[1], "NextToken").as_deref(), Some(TOKEN));
}

/// Asserts that the provider observes an instance by id from the by-id fixture as present
/// and active, observes the not-found fixture as absent, and releases an instance it
/// launched by reading it, terminating it and reading it again, which the not-found
/// fixture answers as gone. Catches a not-found taken for unknown and a terminate sent for
/// an instance the read did not show as ours.
#[test]
fn the_provider_observes_and_releases_over_the_documented_answers() {
    let id = HostRef("i-0123456789abcdef0".to_owned());
    let (provider, script) = provider(vec![
        ok("describe_instances_by_ids.xml"),
        failing(400, "error_invalid_instance_id_not_found.xml"),
        ok("describe_instances_by_ids.xml"),
        ok("terminate_instances.xml"),
        failing(400, "error_invalid_instance_id_not_found.xml"),
    ]);
    assert_eq!(provider.observe(&id), Observed::Present(HostStatus::Active));
    assert_eq!(provider.observe(&id), Observed::Absent);
    assert_eq!(provider.release(&id), Released::Gone);
    let actions: Vec<Option<String>> = script
        .sent()
        .iter()
        .map(|request| param(request, "Action"))
        .collect();
    assert_eq!(
        actions,
        [
            Some("DescribeInstances".to_owned()),
            Some("DescribeInstances".to_owned()),
            Some("DescribeInstances".to_owned()),
            Some("TerminateInstances".to_owned()),
            Some("DescribeInstances".to_owned()),
        ]
    );
}

// The checks of the fixtures themselves.
