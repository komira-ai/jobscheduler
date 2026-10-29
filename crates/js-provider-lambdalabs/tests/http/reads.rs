//! Reading the three endpoints: what the client sends and what it makes of the answers.

use js_elastic::HostStatus;
use js_provider_lambdalabs::{
    ApiError, ApiKey, Architecture, ErrorCode, HttpApi, InstanceId, LambdaApi,
};

use crate::support::{PATIENT, Reply, Scratch, Server, fixture};

const TYPES: &str = "GET /api/v1/instance-types";
const LIST: &str = "GET /api/v1/instances";
const ONE: &str = "GET /api/v1/instances/a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// A client of `server` holding the key `key`.
pub fn client(server: &Server, key: &str) -> HttpApi {
    let scratch = Scratch::new("reads");
    let key = ApiKey::from_file(&scratch.key_file(key)).unwrap();
    HttpApi::connect(&server.url("http"), &key, None, PATIENT).unwrap()
}

fn id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

#[test]
fn instance_types_come_back_ordered_with_gpu_facts_and_regions() {
    let server = Server::plain();
    server.on(TYPES, Reply::json(200, &fixture("instance_types.json")));
    let offers = client(&server, "key-one").instance_types().unwrap();
    let names: Vec<&str> = offers.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "gpu_1x_arm_sample",
            "gpu_1x_sample",
            "gpu_8x_example",
            "gpu_odd_arch"
        ]
    );
    let sample = &offers[1];
    assert_eq!(sample.description, "1x Sample GPU (24 GB)");
    assert_eq!(sample.gpu_description, "Sample GPU (24 GB)");
    assert_eq!(
        (sample.price_cents_per_hour, sample.vcpus, sample.memory_gib),
        (75, 30, 200)
    );
    assert_eq!((sample.storage_gib, sample.gpus), (500, 1));
    assert_eq!(sample.architecture, Architecture::X86_64);
    let regions: Vec<&str> = sample.regions.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(regions, ["region-b", "region-a"]);
    assert_eq!(sample.regions[0].description, "Place B");
    assert!(sample.has_capacity());
    assert!(sample.available_in("region-a"));
    assert!(!sample.available_in("region-c"));
}

#[test]
fn a_type_with_no_capacity_or_an_odd_architecture_is_still_listed() {
    let server = Server::plain();
    server.on(TYPES, Reply::json(200, &fixture("instance_types.json")));
    let offers = client(&server, "key-one").instance_types().unwrap();
    assert_eq!(offers[0].architecture, Architecture::Arm64);
    assert!(!offers[2].has_capacity());
    assert_eq!(offers[2].gpus, 8);
    // No regions key at all, no description, and a control character in the architecture.
    assert!(!offers[3].has_capacity());
    assert_eq!(offers[3].description, "");
    assert_eq!(
        offers[3].architecture,
        Architecture::Other("riscv".to_owned())
    );
}

#[test]
fn the_request_is_a_bearer_get_of_the_documented_path_with_no_query() {
    let server = Server::plain();
    server.on(TYPES, Reply::json(200, &fixture("instance_types.json")));
    server.on(LIST, Reply::json(200, &fixture("instances.json")));
    server.on(ONE, Reply::json(200, &fixture("instance.json")));
    let api = client(&server, "key-two");
    api.instance_types().unwrap();
    api.list_instances().unwrap();
    api.get_instance(&id("a1b2c3d4e5f60718293a4b5c6d7e8f90"))
        .unwrap();
    let seen = server.seen();
    let targets: Vec<(&str, &str)> = seen
        .iter()
        .map(|s| (s.method.as_str(), s.target.as_str()))
        .collect();
    assert_eq!(
        targets,
        [
            ("GET", "/api/v1/instance-types"),
            ("GET", "/api/v1/instances"),
            ("GET", "/api/v1/instances/a1b2c3d4e5f60718293a4b5c6d7e8f90"),
        ]
    );
    for request in &seen {
        assert_eq!(request.header("authorization"), Some("Bearer key-two"));
        assert_eq!(request.header("accept"), Some("application/json"));
    }
}

#[test]
fn the_instance_list_keeps_every_row_in_the_order_given() {
    let server = Server::plain();
    server.on(LIST, Reply::json(200, &fixture("instances.json")));
    let list = client(&server, "key-one").list_instances().unwrap();
    let statuses: Vec<HostStatus> = list.iter().map(|i| i.status.clone()).collect();
    assert_eq!(
        statuses,
        [
            HostStatus::Active,
            HostStatus::Booting,
            HostStatus::Terminated,
            HostStatus::Unknown("paused-for-review".to_owned()),
            HostStatus::Preempted,
            HostStatus::Unhealthy,
            HostStatus::Terminating,
        ]
    );
    let first = &list[0];
    assert_eq!(first.id.as_str(), "a1b2c3d4e5f60718293a4b5c6d7e8f90");
    assert_eq!(first.name.as_deref(), Some("js-0123abcd-000000000001"));
    assert_eq!(first.ip.as_deref(), Some("198.51.100.7"));
    assert_eq!(first.private_ip.as_deref(), Some("10.0.0.7"));
    assert_eq!(first.hostname.as_deref(), Some("host-7"));
    assert_eq!(
        (first.region.as_str(), first.instance_type.as_str()),
        ("region-a", "gpu_1x_sample")
    );
    assert_eq!(first.tags.len(), 2);
    assert_eq!(
        (first.tags[0].key.as_str(), first.tags[0].value.as_str()),
        ("jobscheduler:pool", "fixture")
    );
    let healthy = first.first_healthy.unwrap();
    assert_eq!(
        (healthy.year(), u8::from(healthy.month()), healthy.day()),
        (2026, 10, 20)
    );
    assert_eq!(healthy.millisecond(), 500);
}

#[test]
fn optional_fields_may_be_missing_and_a_bad_timestamp_reads_as_none() {
    let server = Server::plain();
    server.on(LIST, Reply::json(200, &fixture("instances.json")));
    let list = client(&server, "key-one").list_instances().unwrap();
    let booting = &list[1];
    assert_eq!(
        (
            &booting.name,
            &booting.ip,
            &booting.private_ip,
            &booting.hostname
        ),
        (&None, &None, &None, &None)
    );
    assert!(booting.tags.is_empty() && booting.first_healthy.is_none());
    // `tags: null`, a terminated row.
    assert!(list[2].tags.is_empty());
    assert_eq!(list[2].name.as_deref(), Some("renamed by a person"));
    assert!(list[2].first_healthy.is_some());
    // "not a time" does not fail the list.
    assert_eq!(
        list[3].status,
        HostStatus::Unknown("paused-for-review".to_owned())
    );
    assert!(list[3].first_healthy.is_none());
}

#[test]
fn an_empty_list_is_an_empty_list() {
    let server = Server::plain();
    server.on(
        LIST,
        Reply::json(200, r#"{"data": [], "page_token": null}"#),
    );
    assert!(
        client(&server, "key-one")
            .list_instances()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_page_token_makes_the_list_incomplete_not_short() {
    let server = Server::plain();
    server.on(
        LIST,
        Reply::json(200, r#"{"data": [], "page_token": "eyJuZXh0IjoxfQ=="}"#),
    );
    assert_eq!(
        client(&server, "key-one").list_instances().unwrap_err(),
        ApiError::Incomplete
    );
}

#[test]
fn an_empty_or_missing_page_token_is_a_complete_list() {
    let server = Server::plain();
    server.on(LIST, Reply::json(200, r#"{"data": [], "page_token": ""}"#));
    server.on(LIST, Reply::json(200, r#"{"data": []}"#));
    let api = client(&server, "key-one");
    assert!(api.list_instances().unwrap().is_empty());
    assert!(api.list_instances().unwrap().is_empty());
}

#[test]
fn one_instance_is_read_by_id() {
    let server = Server::plain();
    server.on(ONE, Reply::json(200, &fixture("instance.json")));
    let found = client(&server, "key-one")
        .get_instance(&id("a1b2c3d4e5f60718293a4b5c6d7e8f90"))
        .unwrap();
    assert_eq!(found.status, HostStatus::Active);
    assert_eq!(found.tags.len(), 1);
}

#[test]
fn only_object_does_not_exist_on_a_404_reads_as_absent() {
    let server = Server::plain();
    server.on(
        ONE,
        Reply::json(404, &fixture("error_object_does_not_exist.json")),
    );
    server.on(ONE, Reply::json(404, &fixture("error_not_found.json")));
    server.on(ONE, Reply::json(404, "<html>not json</html>"));
    server.on(
        ONE,
        Reply::json(500, &fixture("error_object_does_not_exist.json")),
    );
    let api = client(&server, "key-one");
    let target = id("a1b2c3d4e5f60718293a4b5c6d7e8f90");
    let gone = api.get_instance(&target).unwrap_err();
    assert!(gone.is_absent());
    assert_eq!(
        gone,
        ApiError::Api {
            status: 404,
            code: ErrorCode::ObjectDoesNotExist,
            request_id: Some("req-object_does_not_exist".to_owned()),
        }
    );
    for expected in [
        ErrorCode::NotFound,
        ErrorCode::Unparsed,
        ErrorCode::ObjectDoesNotExist,
    ] {
        let err = api.get_instance(&target).unwrap_err();
        assert!(!err.is_absent(), "{err:?}");
        assert!(
            matches!(&err, ApiError::Api { code, .. } if *code == expected),
            "{err:?}"
        );
    }
}

#[test]
fn an_answer_for_another_instance_than_the_one_asked_for_is_malformed() {
    let server = Server::plain();
    // The fixture is the instance a1b2...; the route is another one's.
    server.on(
        "GET /api/v1/instances/ffffffffffffffffffffffffffffffff",
        Reply::json(200, &fixture("instance.json")),
    );
    let err = client(&server, "key-one")
        .get_instance(&id("ffffffffffffffffffffffffffffffff"))
        .unwrap_err();
    assert!(matches!(err, ApiError::Malformed(_)), "{err:?}");
    // The right one still reads.
    server.on(ONE, Reply::json(200, &fixture("instance.json")));
    let found = client(&server, "key-one")
        .get_instance(&id("a1b2c3d4e5f60718293a4b5c6d7e8f90"))
        .unwrap();
    assert_eq!(found.id.as_str(), "a1b2c3d4e5f60718293a4b5c6d7e8f90");
}
