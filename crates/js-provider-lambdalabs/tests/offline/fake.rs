//! The in-memory account.

use js_provider_lambdalabs::{
    ApiError, Architecture, Call, ErrorCode, FakeLambda, Instance, InstanceId, InstanceTypeOffer,
    LambdaApi, Region, Status,
};

fn instance(id: &str, status: Status) -> Instance {
    Instance {
        id: InstanceId::new(id).unwrap(),
        name: None,
        status,
        region: "region-a".to_owned(),
        instance_type: "gpu_1x_sample".to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags: Vec::new(),
    }
}

fn offer(name: &str) -> InstanceTypeOffer {
    InstanceTypeOffer {
        name: name.to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: 1,
        vcpus: 1,
        memory_gib: 1,
        storage_gib: 1,
        gpus: 1,
        architecture: Architecture::X86_64,
        regions: vec![Region {
            name: "region-a".to_owned(),
            description: String::new(),
        }],
    }
}

#[test]
fn a_new_account_is_empty() {
    let fake = FakeLambda::new();
    assert!(fake.instance_types().unwrap().is_empty());
    assert!(fake.list_instances().unwrap().is_empty());
    assert!(fake.calls().len() == 2);
}

#[test]
fn offers_come_back_ordered_by_name() {
    let fake = FakeLambda::new();
    fake.set_offers(vec![offer("b"), offer("a")]);
    let names: Vec<String> = fake
        .instance_types()
        .unwrap()
        .into_iter()
        .map(|o| o.name)
        .collect();
    assert_eq!(names, ["a", "b"]);
}

#[test]
fn instances_are_listed_in_the_order_added_and_read_by_id() {
    let fake = FakeLambda::new();
    fake.add_instance(instance("one", Status::Active));
    fake.add_instance(instance("two", Status::Booting));
    let ids: Vec<String> = fake
        .list_instances()
        .unwrap()
        .iter()
        .map(|i| i.id.to_string())
        .collect();
    assert_eq!(ids, ["one", "two"]);
    assert_eq!(
        fake.get_instance(&InstanceId::new("two").unwrap())
            .unwrap()
            .status,
        Status::Booting
    );
}

#[test]
fn an_instance_it_does_not_hold_fails_as_the_real_api_does() {
    let fake = FakeLambda::new();
    let err = fake
        .get_instance(&InstanceId::new("gone").unwrap())
        .unwrap_err();
    assert!(err.is_absent());
    assert!(matches!(
        err,
        ApiError::Api {
            status: 404,
            code: ErrorCode::ObjectDoesNotExist,
            ..
        }
    ));
}

#[test]
fn scripted_failures_are_used_once_each_in_order_per_call() {
    let fake = FakeLambda::new();
    fake.add_instance(instance("one", Status::Active));
    fake.fail_next(Call::ListInstances, ApiError::Transport("first".to_owned()));
    fake.fail_next(Call::ListInstances, ApiError::Incomplete);
    fake.fail_next(Call::InstanceTypes, ApiError::Malformed("types".to_owned()));
    fake.fail_next(Call::GetInstance, ApiError::Transport("get".to_owned()));
    assert_eq!(
        fake.list_instances().unwrap_err(),
        ApiError::Transport("first".to_owned())
    );
    assert_eq!(fake.list_instances().unwrap_err(), ApiError::Incomplete);
    assert_eq!(fake.list_instances().unwrap().len(), 1);
    assert_eq!(
        fake.instance_types().unwrap_err(),
        ApiError::Malformed("types".to_owned())
    );
    assert!(fake.instance_types().is_ok());
    let id = InstanceId::new("one").unwrap();
    assert_eq!(
        fake.get_instance(&id).unwrap_err(),
        ApiError::Transport("get".to_owned())
    );
    assert!(fake.get_instance(&id).is_ok());
}

#[test]
fn every_call_is_recorded_failed_ones_included() {
    let fake = FakeLambda::new();
    fake.fail_next(Call::ListInstances, ApiError::Incomplete);
    let _ = fake.list_instances();
    let _ = fake.instance_types();
    let _ = fake.get_instance(&InstanceId::new("x").unwrap());
    assert_eq!(
        fake.calls(),
        [Call::ListInstances, Call::InstanceTypes, Call::GetInstance]
    );
}
