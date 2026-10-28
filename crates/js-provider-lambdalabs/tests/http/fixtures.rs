//! The fixtures carry every field the documentation marks required, and the in-memory
//! fake answers what the real client does for the same data.

use js_provider_lambdalabs::{FakeLambda, LambdaApi};
use serde_json::Value;

use crate::reads::client;
use crate::support::{Reply, Server, fixture};

fn json(name: &str) -> Value {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn require(object: &Value, keys: &[&str], what: &str) {
    for key in keys {
        assert!(
            object.get(*key).is_some(),
            "{what} lacks the required field {key}"
        );
    }
}

/// The required fields of `Instance` and `InstanceType` in the published API description.
const INSTANCE: &[&str] = &[
    "id",
    "status",
    "ssh_key_names",
    "file_system_names",
    "region",
    "instance_type",
    "first_healthy",
    "actions",
];
const TYPE: &[&str] = &[
    "name",
    "description",
    "gpu_description",
    "price_cents_per_hour",
    "specs",
    "architecture",
];

#[test]
fn instance_fixtures_carry_the_required_fields() {
    let list = json("instances.json");
    assert!(list.get("page_token").is_some());
    for row in list["data"].as_array().unwrap() {
        require(row, INSTANCE, "instance");
        require(&row["instance_type"], TYPE, "instance type");
    }
    let one = json("instance.json");
    require(&one["data"], INSTANCE, "instance");
    require(&one["data"]["instance_type"], TYPE, "instance type");
}

#[test]
fn type_fixtures_carry_the_required_fields_except_the_sparse_row_made_on_purpose() {
    let types = json("instance_types.json");
    for (name, item) in types["data"].as_object().unwrap() {
        require(item, &["instance_type"], name);
        if name != "gpu_odd_arch" {
            require(item, &["regions_with_capacity_available"], name);
            require(&item["instance_type"], TYPE, name);
        }
    }
}

#[test]
fn error_fixtures_carry_code_and_message() {
    for name in [
        "error_invalid_api_key.json",
        "error_account_inactive.json",
        "error_forbidden.json",
        "error_object_does_not_exist.json",
        "error_not_found.json",
        "error_rate_limited.json",
        "error_internal.json",
        "error_provider_unavailable.json",
    ] {
        require(&json(name)["error"], &["code", "message"], name);
    }
}

#[test]
fn the_fake_answers_what_the_client_does_for_the_same_data() {
    let server = Server::plain();
    server.on(
        "GET /api/v1/instance-types",
        Reply::json(200, &fixture("instance_types.json")),
    );
    server.on(
        "GET /api/v1/instances",
        Reply::json(200, &fixture("instances.json")),
    );
    let real = client(&server, "key-one");
    let offers = real.instance_types().unwrap();
    let instances = real.list_instances().unwrap();

    let fake = FakeLambda::new();
    fake.set_offers(offers.clone());
    for instance in &instances {
        fake.add_instance(instance.clone());
    }
    assert_eq!(fake.instance_types().unwrap(), offers);
    assert_eq!(fake.list_instances().unwrap(), instances);
    for instance in &instances {
        assert_eq!(fake.get_instance(&instance.id).unwrap(), *instance);
    }
}
