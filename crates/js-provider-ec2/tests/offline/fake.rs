//! The in-memory account's launch side: what a launch, a read and a terminate do to it,
//! and the failures a test can script.

use js_elastic::{AcquireId, SchedulerId};
use js_provider_ec2::{
    Action, ApiError, ClientToken, DescribeFilter, Ec2Api, FakeEc2, ImageId, Instance, InstanceId,
    InstanceState, InstanceType, LaunchPurchase, Region, RunRequest, Tag, UserData, Zone,
};

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn request(acquire: &str, tags: &[(&str, &str)]) -> RunRequest {
    RunRequest {
        image: ImageId::new("ami-0123456789abcdef0").unwrap(),
        instance_type: InstanceType("m7.large".to_owned()),
        zone: Zone("zone-a1".to_owned()),
        purchase: LaunchPurchase::OnDemand,
        user_data: UserData::new(b"boot".to_vec()),
        tags: tags.iter().map(|(k, v)| tag(k, v)).collect(),
        client_token: ClientToken::derive(
            &AcquireId::new(acquire).unwrap(),
            &SchedulerId::new("one").unwrap(),
        ),
    }
}

fn foreign(id: &str, tags: &[(&str, &str)]) -> Instance {
    Instance {
        id: InstanceId::new(id).unwrap(),
        state: InstanceState::Running,
        tags: tags.iter().map(|(k, v)| tag(k, v)).collect(),
        instance_type: InstanceType("c7.large".to_owned()),
        zone: Zone("zone-a2".to_owned()),
        image: ImageId::new("ami-0123456789abcdef1").unwrap(),
    }
}

fn by_tags(tags: &[(&str, &str)]) -> DescribeFilter {
    DescribeFilter::Tags(tags.iter().map(|(k, v)| tag(k, v)).collect())
}

fn launch(fake: &FakeEc2, acquire: &str) -> InstanceId {
    fake.run_instances(&request(acquire, &[("k", "v")]))
        .unwrap()
        .remove(0)
}

/// Asserts that a launch creates one pending instance with the tags, type, zone and image
/// of the request and a valid new id, that a repeat of the client token answers the same
/// id and creates nothing, that a forgotten token launches again under a new id, and that
/// the fake counts and records only the launches that created an instance.
#[test]
fn a_launch_creates_one_instance_and_a_repeated_token_creates_none() {
    let fake = FakeEc2::new();
    let first = launch(&fake, "a");
    let rows = fake.instances();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, first);
    assert_eq!(rows[0].state, InstanceState::Pending);
    assert_eq!(rows[0].tags, [tag("k", "v")]);
    assert_eq!(rows[0].instance_type, InstanceType("m7.large".to_owned()));
    assert_eq!(rows[0].zone, Zone("zone-a1".to_owned()));
    assert_eq!(rows[0].image.as_str(), "ami-0123456789abcdef0");
    assert_eq!(first.as_str().len(), 19);
    assert_eq!(launch(&fake, "a"), first);
    assert_eq!(fake.launched(), 1);
    assert_eq!(fake.run_requests(), [request("a", &[("k", "v")])]);
    fake.forget_client_tokens();
    let again = launch(&fake, "a");
    assert_ne!(again, first);
    assert_eq!(fake.launched(), 2);
    assert_eq!(fake.instances().len(), 2);
    assert_eq!(
        fake.actions(),
        [
            Action::RunInstances,
            Action::RunInstances,
            Action::RunInstances
        ]
    );
    assert!(fake.calls().is_empty());
}

/// Asserts that a failure scripted before a launch leaves nothing behind and is spent by
/// one call, that an answer lost after a launch leaves the instance and still answers the
/// error, and that the retry of a lost launch finds its instance by the client token.
#[test]
fn a_launch_can_fail_before_or_lose_its_answer_after() {
    let fake = FakeEc2::new();
    fake.fail_next(Action::RunInstances, ApiError::Timeout);
    assert_eq!(
        fake.run_instances(&request("a", &[])),
        Err(ApiError::Timeout)
    );
    assert_eq!((fake.launched(), fake.instances().len()), (0, 0));
    fake.lose_after_next(Action::RunInstances, ApiError::Timeout);
    assert_eq!(
        fake.run_instances(&request("a", &[])),
        Err(ApiError::Timeout)
    );
    assert_eq!((fake.launched(), fake.instances().len()), (1, 1));
    let found = fake.run_instances(&request("a", &[])).unwrap();
    assert_eq!(found, [fake.instances()[0].id.clone()]);
    assert_eq!(fake.launched(), 1);
}

/// Asserts that `fail_next_call` fails whichever call comes next, a read or a launch, and
/// only that one; that several queue in order; and that the call it failed did nothing.
#[test]
fn the_next_call_of_any_kind_can_be_failed() {
    let fake = FakeEc2::new();
    fake.fail_next_call(ApiError::Timeout)
        .fail_next_call(ApiError::Incomplete);
    assert_eq!(
        fake.describe_instances(&by_tags(&[])),
        Err(ApiError::Timeout)
    );
    assert_eq!(
        fake.run_instances(&request("a", &[])),
        Err(ApiError::Incomplete)
    );
    assert_eq!(fake.launched(), 0);
    assert!(fake.describe_instances(&by_tags(&[])).unwrap().is_empty());
    assert_eq!(
        fake.actions(),
        [
            Action::DescribeInstances,
            Action::RunInstances,
            Action::DescribeInstances
        ]
    );
}

/// Asserts that a lost answer works for a read of the catalog and for a read of instances
/// as well: the error comes once and the next call answers.
#[test]
fn a_lost_answer_applies_to_reads_too() {
    let fake = FakeEc2::new();
    let region = Region::new("region-a").unwrap();
    fake.lose_after_next(Action::GetProducts, ApiError::Timeout);
    assert_eq!(fake.on_demand_prices(&region), Err(ApiError::Timeout));
    assert_eq!(fake.on_demand_prices(&region), Ok(Vec::new()));
    fake.lose_after_next(Action::DescribeInstances, ApiError::Timeout);
    assert_eq!(
        fake.describe_instances(&by_tags(&[])),
        Err(ApiError::Timeout)
    );
    assert!(fake.describe_instances(&by_tags(&[])).is_ok());
    assert_eq!(fake.calls().len(), 2);
    assert_eq!(fake.actions().len(), 4);
}

/// Asserts that a read by tags answers the instances that carry every tag asked for, in
/// order, of any state (a terminated one included) and of any launcher, and none when one
/// tag is missing or has another value; and that a read with no tag answers all.
#[test]
fn a_read_by_tags_answers_every_instance_with_all_of_them() {
    let fake = FakeEc2::new();
    let mine = launch(&fake, "a");
    fake.add_instance(foreign("i-0123456789abcdef9", &[("k", "v"), ("x", "y")]));
    fake.add_instance(foreign("i-0123456789abcdef8", &[("x", "y")]));
    fake.set_state(&mine, InstanceState::Terminated);
    let ids = |filter| -> Vec<String> {
        fake.describe_instances(&filter)
            .unwrap()
            .iter()
            .map(|row| row.id.to_string())
            .collect()
    };
    assert_eq!(
        ids(by_tags(&[("k", "v")])),
        [mine.to_string(), "i-0123456789abcdef9".to_owned()]
    );
    assert_eq!(ids(by_tags(&[("k", "v"), ("x", "y")])).len(), 1);
    assert!(ids(by_tags(&[("k", "other")])).is_empty());
    assert_eq!(ids(by_tags(&[])).len(), 3);
    assert_eq!(fake.launched(), 1);
}

/// Asserts that a read by ids answers the instances held, and fails with the cloud's
/// not-found code when any id is not held, one removed from the cloud included.
#[test]
fn a_read_by_ids_fails_not_found_for_an_instance_not_held() {
    let fake = FakeEc2::new();
    let one = launch(&fake, "a");
    let two = launch(&fake, "b");
    let rows = fake
        .describe_instances(&DescribeFilter::Ids(vec![one.clone()]))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, one);
    let both = fake
        .describe_instances(&DescribeFilter::Ids(vec![two.clone(), one.clone()]))
        .unwrap();
    assert_eq!(both.len(), 2);
    fake.remove_instance(&two);
    let error = fake
        .describe_instances(&DescribeFilter::Ids(vec![one.clone(), two]))
        .unwrap_err();
    assert!(error.is_not_found());
    assert_eq!(fake.instances().len(), 1);
}

/// Asserts that a terminate moves the instances to shutting-down and counts one call, that
/// an unknown id fails not-found and changes and counts nothing even beside a known id,
/// that a terminate that failed before it was sent is not counted, and that one whose
/// answer was lost took effect and is.
#[test]
fn a_terminate_takes_effect_unless_it_fails_before_or_names_an_unknown_id() {
    let fake = FakeEc2::new();
    let one = launch(&fake, "a");
    let two = launch(&fake, "b");
    let unknown = InstanceId::new("i-0123456789abcdef7").unwrap();
    let error = fake
        .terminate_instances(&[one.clone(), unknown])
        .unwrap_err();
    assert!(error.is_not_found());
    assert_eq!(fake.terminates(), 0);
    assert_eq!(fake.instances()[0].state, InstanceState::Pending);
    fake.fail_next(Action::TerminateInstances, ApiError::Timeout);
    assert_eq!(
        fake.terminate_instances(std::slice::from_ref(&one)),
        Err(ApiError::Timeout)
    );
    assert_eq!(fake.terminates(), 0);
    assert_eq!(fake.instances()[0].state, InstanceState::Pending);
    assert_eq!(fake.terminate_instances(&[one]), Ok(()));
    assert_eq!(fake.terminates(), 1);
    assert_eq!(fake.instances()[0].state, InstanceState::ShuttingDown);
    assert_eq!(fake.instances()[1].state, InstanceState::Pending);
    fake.lose_after_next(Action::TerminateInstances, ApiError::Timeout);
    assert_eq!(fake.terminate_instances(&[two]), Err(ApiError::Timeout));
    assert_eq!(fake.terminates(), 2);
    assert_eq!(fake.instances()[1].state, InstanceState::ShuttingDown);
}

/// Asserts that `set_state` changes the instance named and no other, and does nothing for
/// an id the fake does not hold.
#[test]
fn set_state_changes_only_the_instance_named() {
    let fake = FakeEc2::new();
    let one = launch(&fake, "a");
    let two = launch(&fake, "b");
    fake.set_state(&one, InstanceState::Running);
    fake.set_state(
        &InstanceId::new("i-0123456789abcdef7").unwrap(),
        InstanceState::Stopped,
    );
    let states: Vec<InstanceState> = fake.instances().iter().map(|row| row.state).collect();
    assert_eq!(states, [InstanceState::Running, InstanceState::Pending]);
    assert_eq!(fake.instances()[1].id, two);
}

/// Asserts that a read failing after the next terminate lets reads before the terminate
/// answer, fails the first read after it, and only that one, and that a second terminate
/// with nothing queued arms nothing.
#[test]
fn a_read_after_the_next_terminate_can_be_failed() {
    let fake = FakeEc2::new();
    let one = launch(&fake, "a");
    fake.fail_read_after_next_terminate(ApiError::Timeout);
    assert!(fake.describe_instances(&by_tags(&[])).is_ok());
    fake.terminate_instances(std::slice::from_ref(&one))
        .unwrap();
    assert_eq!(
        fake.describe_instances(&by_tags(&[])),
        Err(ApiError::Timeout)
    );
    assert!(fake.describe_instances(&by_tags(&[])).is_ok());
    fake.terminate_instances(&[one]).unwrap();
    assert!(fake.describe_instances(&by_tags(&[])).is_ok());
}

/// Asserts that a page failure fails the whole read with an incomplete answer, never a
/// short list: the page must exist in the walk (the third of three, the fifth of five
/// with a page size of zero read as one, the first of an unpaginated walk), a page past
/// the walk or before the first fails nothing, and one scripted failure fails one walk.
#[test]
fn a_failed_page_fails_the_whole_read() {
    let fake = FakeEc2::new();
    for acquire in ["a", "b", "c", "d", "e"] {
        launch(&fake, acquire);
    }
    let read = || fake.describe_instances(&by_tags(&[]));
    fake.fail_page(1);
    assert_eq!(read(), Err(ApiError::Incomplete));
    fake.fail_page(2);
    assert_eq!(read().unwrap().len(), 5);
    fake.paginate(2);
    fake.fail_page(4);
    assert_eq!(read().unwrap().len(), 5);
    fake.fail_page(0);
    assert_eq!(read().unwrap().len(), 5);
    fake.fail_page(3);
    assert_eq!(read(), Err(ApiError::Incomplete));
    assert_eq!(read().unwrap().len(), 5);
    fake.paginate(0);
    fake.fail_page(5);
    assert_eq!(read(), Err(ApiError::Incomplete));
    assert_eq!(read().unwrap().len(), 5);
}

/// Asserts that an empty account is read in one page, so a page failure of the first page
/// fails it, and that a lister that reads it whole is not told of a page it does not have.
#[test]
fn an_empty_read_is_one_page() {
    let fake = FakeEc2::new();
    fake.paginate(3).fail_page(1);
    assert_eq!(
        fake.describe_instances(&by_tags(&[])),
        Err(ApiError::Incomplete)
    );
    assert_eq!(fake.describe_instances(&by_tags(&[])), Ok(Vec::new()));
}
